mod app;
mod ui;

use anyhow::{bail, Context as _};
use app::{App, CommandAction};
use forge_core::agent_loop::Agent;
use forge_core::config::Config;
use forge_core::context::ContextManager;
use forge_core::event::AgentEvent;
use forge_core::registry::Registry;
use forge_core::session::{AllowAll, SessionStore};
use forge_core::message::Message;
use forge_storage::SqliteSessionStore;
use forge_tools::ShellTool;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

fn default_db_path() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".forge").join("forge.db")
}

fn current_project_root() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}

/// Latest session belonging to this project. Sessions carry a
/// `project_root` meta entry stamped at creation; explicit matches win,
/// and entries without a marker (legacy, pre-isolation) are only used as
/// a fallback so old histories stay reachable. `title` optionally narrows
/// the search (used by `forge check`'s named check session).
async fn latest_session_for_project(
    store: &SqliteSessionStore,
    project_root: &str,
    title: Option<&str>,
) -> anyhow::Result<Option<forge_core::session::SessionSummary>> {
    let sessions = store.list_sessions(50).await?;
    let mut legacy = None;
    for s in sessions {
        if let Some(t) = title {
            if s.title != t {
                continue;
            }
        }
        match store.get_meta(s.id, "project_root").await {
            Ok(Some(v)) => {
                if v.as_str().map(|p| p.eq_ignore_ascii_case(project_root)).unwrap_or(false) {
                    return Ok(Some(s));
                }
            }
            Ok(None) => {
                // Legacy session without a marker: remember the newest,
                // but an explicit project match always wins.
                if legacy.is_none() {
                    legacy = Some(s);
                }
            }
            Err(_) => continue,
        }
    }
    Ok(legacy)
}

fn find_config() -> Option<PathBuf> {
    for p in ["forge.toml", "config.toml"] {
        let cwd = PathBuf::from(p);
        if cwd.is_file() {
            return Some(cwd);
        }
    }
    let home = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .ok()?;
    [home.join(".forge").join("config.toml")]
        .into_iter()
        .find(|p| p.is_file())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `forge check "prompt"`: headless one-shot mode (no TUI). Prints the
    // streamed answer to stdout — used for scripted acceptance runs.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "check" {
        return run_check(args[2..].join(" ")).await;
    }
    run_tui().await
}

async fn run_check(prompt: String) -> anyhow::Result<()> {
    let cfg_path = find_config();
    let cfg: Config = match &cfg_path {
        Some(p) => Config::load(p).with_context(|| format!("loading {}", p.display()))?,
        None => Config::default(),
    };
    if cfg.model.name.is_empty() {
        bail!("model name is empty: set [model] name in config.toml");
    }
    let api_key = cfg.resolve_api_key()?;
    let db_path = default_db_path();
    let store = Arc::new(SqliteSessionStore::open(&db_path).await?);

    let shell = ShellTool::new(
        cfg.context.shell_path.clone().map(PathBuf::from),
        cfg.context.shell_timeout_secs,
    );
    let mut tools: Registry<dyn forge_core::traits::Tool> = Registry::new();
    tools.register(Arc::new(shell));

    let provider = forge_provider::from_config(&cfg.provider.protocol, &cfg.provider.base_url)?;

    // Reuse the latest check session for THIS project when present so
    // consecutive `forge check` invocations form one continuous
    // conversation without bleeding into other projects' history.
    let project_root = current_project_root();
    let sid = match latest_session_for_project(&store, &project_root, Some("check")).await? {
        Some(s) => s.id,
        None => {
            let id = store.create_session("check").await?;
            let _ = store
                .set_meta(id, "project_root", &serde_json::json!(project_root))
                .await;
            id
        }
    };
    let (agent, warning) = build_agent(&cfg, &api_key, &tools, &(store.clone() as Arc<dyn SessionStore>), &provider, Some(sid)).await;
    if let Some(w) = warning {
        // Never run a headless acceptance run against a transcript we
        // could not read.
        bail!("{w}");
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent2 = agent.clone();
    let handle = tokio::spawn(async move {
        agent2.run_turn(&prompt, tx).await
    });
    let mut saw_error = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::MessageDelta { delta } => print!("{delta}"),
            AgentEvent::ReasoningDelta { .. } => {}
            AgentEvent::ToolCallStarted { command, .. } => {
                println!("\n[tool] {command}");
            }
            AgentEvent::ToolCallOutputDelta { chunk, .. } => print!("{chunk}"),
            AgentEvent::ToolCallCompleted { output, .. } => {
                println!("[tool done] {}", output.lines().last().unwrap_or(""));
            }
            AgentEvent::Error { message } => {
                saw_error = true;
                eprintln!("\n[error] {message}");
            }
            AgentEvent::TokenCountUpdated { used, limit } => {
                eprintln!("[tokens ~{used}/{limit}]");
            }
            _ => {}
        }
    }
    let result = handle.await??;
    println!();
    if saw_error {
        bail!("turn completed with errors");
    }
    let preview: String = result.chars().take(200).collect();
    println!("=== final answer: {preview} ===");
    Ok(())
}

async fn run_tui() -> anyhow::Result<()> {
    // Logging to a file so the TUI stays clean.
    let log_dir = PathBuf::from(".").join(".forge-logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = std::fs::File::create(log_dir.join("forge.log")).ok();
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file.unwrap_or_else(|| {
            std::fs::File::create(std::env::temp_dir().join("forge.log")).unwrap()
        })))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg_path = find_config();
    let cfg: Config = match &cfg_path {
        Some(p) => Config::load(p)
            .with_context(|| format!("loading {}", p.display()))?,
        None => Config::default(),
    };
    if cfg.model.name.is_empty() {
        bail!("model name is empty: set [model] name in config.toml (e.g. \"deepseek-flash\")");
    }

    let api_key = cfg
        .resolve_api_key()
        .with_context(|| "no API key: set the env var named in config (provider.api_key_env)")?;

    let db_path = default_db_path();
    let store = Arc::new(SqliteSessionStore::open(&db_path).await?);
    // Trait-object view for build_agent; keep the concrete handle for
    // session listing/creation helpers.
    let store_dyn: Arc<dyn SessionStore> = store.clone();

    let shell = ShellTool::new(
        cfg.context.shell_path.clone().map(PathBuf::from),
        cfg.context.shell_timeout_secs,
    );
    let mut tools: Registry<dyn forge_core::traits::Tool> = Registry::new();
    tools.register(Arc::new(shell));

    let provider: Arc<dyn forge_core::traits::ModelProvider> =
        forge_provider::from_config(&cfg.provider.protocol, &cfg.provider.base_url)?;

    // Terminal setup.
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new();
    let mut events_rx: mpsc::UnboundedReceiver<AgentEvent> =
        bootstrap_agent_channel();
    // The live agent is kept across turns so context/history persist
    // within a session; rebuilt on /new and /resume.
    let mut agent: Option<Arc<Agent>> = None;

    loop {
        // Redraw.
        terminal.draw(|f| ui::draw(f, &app))?;

        // Handle UI events with a small timeout so we also drain agent events.
        let ev_available = crossterm::event::poll(std::time::Duration::from_millis(50))?;
        if ev_available {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind == crossterm::event::KeyEventKind::Press {
                    let submit = ui::on_key(&mut app, key);
                    if submit {
                        let action = app.on_submit();
                        // /exit must work even mid-turn: the agent task
                        // outlives the UI loop and the process exits.
                        if matches!(action, CommandAction::Exit) {
                            app.should_quit = true;
                        } else if !app.busy {
                            match action {
                                CommandAction::Send(text) => {
                                    ensure_session(&mut app, &*store).await;
                                    if agent.is_none() {
                                        let (a, warning) = build_agent(
                                            &cfg, &api_key, &tools, &store_dyn, &provider,
                                            app.session_id,
                                        )
                                        .await;
                                        if let Some(w) = warning {
                                            app.push_line(app::Line::Error(w));
                                        }
                                        agent = Some(a);
                                    }
                                    if let Some(a) = &agent {
                                        let (tx, rx) = mpsc::unbounded_channel();
                                        let a = a.clone();
                                        let text = text.clone();
                                        let tx_guard = tx.clone();
                                        app.busy = true;
                                        tokio::spawn(async move {
                                            let inner = tokio::spawn(async move {
                                                a.run_turn(&text, tx).await
                                            });
                                            // run_turn guarantees terminal
                                            // events; this only catches a
                                            // panicked agent task.
                                            if let Err(join) = inner.await {
                                                let _ = tx_guard.send(AgentEvent::Error {
                                                    message: format!("agent task crashed: {join}"),
                                                });
                                            }
                                        });
                                        events_rx = rx;
                                    }
                                }
                                CommandAction::NewSession => {
                                    app.session_id = None;
                                    app.clear_transient();
                                    app.session_title = "(new session)".into();
                                    app.status = "ready".into();
                                    agent = None; // fresh context next turn
                                    ensure_session(&mut app, &*store).await;
                                }
                                CommandAction::Resume => {
                                    let project_root = current_project_root();
                                    match latest_session_for_project(
                                        &*store, &project_root, None,
                                    )
                                    .await
                                    {
                                        Ok(None) => app.push_line(app::Line::System(
                                            "no saved sessions for this project".into(),
                                        )),
                                        Ok(Some(s)) => {
                                            app.load_session(&store, &s).await;
                                            // Rebuild the agent with this session's
                                            // history so conversation continuity works.
                                            let (a, warning) = build_agent(
                                                &cfg, &api_key, &tools, &store_dyn, &provider,
                                                app.session_id,
                                            )
                                            .await;
                                            if let Some(w) = warning {
                                                app.push_line(app::Line::Error(w));
                                            }
                                            agent = Some(a);
                                        }
                                        Err(e) => {
                                            app.push_line(app::Line::Error(format!(
                                                "cannot list sessions: {e}"
                                            )));
                                        }
                                    }
                                }
                                CommandAction::Compact => {
                                    if agent.is_none() {
                                        let (a, warning) = build_agent(
                                            &cfg, &api_key, &tools, &store_dyn, &provider,
                                            app.session_id,
                                        )
                                        .await;
                                        if let Some(w) = warning {
                                            app.push_line(app::Line::Error(w));
                                        }
                                        agent = Some(a);
                                    }
                                    if let Some(a) = &agent {
                                        let (tx, rx) = mpsc::unbounded_channel();
                                        let a = a.clone();
                                        events_rx = rx;
                                        let res = a.compact_manual(&tx).await;
                                        match res {
                                            Ok((b, aft)) => {
                                                app.push_line(app::Line::System(format!(
                                                    "manual compaction: ~{b} → ~{aft} tokens"
                                                )));
                                            }
                                            Err(e) => app.push_line(app::Line::Error(e.to_string())),
                                        }
                                    }
                                }
                                CommandAction::Exit | CommandAction::None => {}
                            }
                        }
                        if app.should_quit {
                            break;
                        }
                    }
                }
            }
        }

        // Drain agent events.
        while let Ok(ev) = events_rx.try_recv() {
            app.on_agent_event(ev);
        }

        if app.should_quit {
            break;
        }
    }

    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

fn bootstrap_agent_channel() -> mpsc::UnboundedReceiver<AgentEvent> {
    let (_tx, rx) = mpsc::unbounded_channel();
    // Placeholder receiver until the first turn spawns its own channel.
    rx
}

/// Build the agent. Returns the agent plus an optional user-facing
/// warning: when the stored transcript cannot be read, persistence is
/// disabled (session id dropped) so a later `replace_messages` can never
/// wipe a transcript we failed to load.
async fn build_agent(
    cfg: &Config,
    api_key: &str,
    tools: &Registry<dyn forge_core::traits::Tool>,
    store: &Arc<dyn SessionStore>,
    provider: &Arc<dyn forge_core::traits::ModelProvider>,
    session_id: Option<uuid::Uuid>,
) -> (Arc<Agent>, Option<String>) {
    let mut context = ContextManager::new(cfg.context.clone());
    context.set_max_output_tokens(cfg.model.max_tokens as i64);
    // Seed the system prompt and restore the persisted transcript so the
    // agent remembers previous turns within this session.
    context.push(Message::system(SYSTEM_PROMPT));
    let mut effective_sid = session_id;
    let mut warning = None;
    if let Some(sid) = session_id {
        match store.load_messages(sid).await {
            Ok(msgs) => {
                for m in msgs {
                    // Skip persisted system prompts; ours is fresh above.
                    if matches!(m, Message::System { .. }) {
                        continue;
                    }
                    context.push(m);
                }
                // Drop empty assistant shells / orphan tool results that
                // older builds could have persisted; providers reject the
                // empty shapes and replaying them breaks later turns.
                context.history.normalize();
            }
            Err(e) => {
                warning = Some(format!(
                    "session history failed to load ({e}); this conversation runs without saving — the original transcript is untouched"
                ));
                effective_sid = None;
            }
        }
    }
    (
        Arc::new(Agent {
            provider: provider.clone(),
            tools: clone_registry(tools),
            store: store.clone(),
            permissions: Arc::new(AllowAll),
            context: tokio::sync::Mutex::new(context),
            api_key: api_key.to_string(),
            model: cfg.model.name.clone(),
            temperature: cfg.model.temperature,
            max_tokens: cfg.model.max_tokens,
            session_id: effective_sid,
        }),
        warning,
    )
}

const SYSTEM_PROMPT: &str = "\
You are forge, a pragmatic coding agent running in the user's terminal on Windows (Git Bash).
You have one tool: `shell`, which executes a shell command and returns its output.
Use it freely to read files (cat/ls), search (grep/find), run builds and tests.
Prefer focused commands; avoid interactive ones. Answer in the user's language. Be concise.";

fn clone_registry(r: &Registry<dyn forge_core::traits::Tool>) -> Registry<dyn forge_core::traits::Tool> {
    let mut out: Registry<dyn forge_core::traits::Tool> = Registry::new();
    for t in r.all() {
        out.register(t);
    }
    out
}

async fn ensure_session(app: &mut App, store: &SqliteSessionStore) {
    if app.session_id.is_none() {
        let title = app.maybe_title(app.input.trim());
        match store.create_session(&title).await {
            Ok(id) => {
                // Tag the session with the current project so /resume and
                // `forge check` never mix histories across projects.
                let root = current_project_root();
                let _ = store
                    .set_meta(id, "project_root", &serde_json::json!(root))
                    .await;
                app.session_id = Some(id);
                app.session_title = title;
            }
            Err(e) => app.push_line(app::Line::Error(format!("session create failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::session::SessionStore;

    struct FailingStore;

    #[async_trait::async_trait]
    impl SessionStore for FailingStore {
        async fn create_session(&self, _t: &str) -> forge_core::Result<uuid::Uuid> {
            Ok(uuid::Uuid::new_v4())
        }
        async fn list_sessions(
            &self,
            _: u32,
        ) -> forge_core::Result<Vec<forge_core::session::SessionSummary>> {
            Ok(vec![])
        }
        async fn load_messages(&self, _: uuid::Uuid) -> forge_core::Result<Vec<Message>> {
            Err(forge_core::Error::Storage("corrupt payload".into()))
        }
        async fn append_messages(&self, _: uuid::Uuid, _: &[Message]) -> forge_core::Result<()> {
            Ok(())
        }
        async fn replace_messages(&self, _: uuid::Uuid, _: &[Message]) -> forge_core::Result<()> {
            Ok(())
        }
        async fn rename_session(&self, _: uuid::Uuid, _: &str) -> forge_core::Result<()> {
            Ok(())
        }
        async fn set_meta(
            &self,
            _: uuid::Uuid,
            _: &str,
            _: &serde_json::Value,
        ) -> forge_core::Result<()> {
            Ok(())
        }
        async fn get_meta(
            &self,
            _: uuid::Uuid,
            _: &str,
        ) -> forge_core::Result<Option<serde_json::Value>> {
            Ok(None)
        }
    }

    fn test_tools() -> Registry<dyn forge_core::traits::Tool> {
        Registry::new()
    }

    fn test_provider() -> Arc<dyn forge_core::traits::ModelProvider> {
        // Client construction only; no network is touched in this test.
        forge_provider::from_config("openai", "http://127.0.0.1:9").unwrap()
    }

    #[tokio::test]
    async fn corrupt_history_disables_persistence() {
        let cfg = Config::default();
        let api_key = cfg.resolve_api_key().unwrap_or_default();
        let store: Arc<dyn SessionStore> = Arc::new(FailingStore);
        let (agent, warning) = build_agent(
            &cfg,
            &api_key,
            &test_tools(),
            &store,
            &test_provider(),
            Some(uuid::Uuid::new_v4()),
        )
        .await;
        assert!(warning.is_some(), "load failure must surface a warning");
        assert_eq!(
            agent.session_id, None,
            "persistence must be disabled so the stored transcript is never overwritten"
        );
    }

    #[tokio::test]
    async fn sessions_are_scoped_to_project_root() {
        let dir = std::env::temp_dir().join(format!("forge-tui-test-{}", uuid::Uuid::new_v4()));
        let store = SqliteSessionStore::open(&dir.join("t.db")).await.unwrap();
        let sid_a = store.create_session("proj a").await.unwrap();
        store
            .set_meta(sid_a, "project_root", &serde_json::json!("D:/projA"))
            .await
            .unwrap();
        let sid_b = store.create_session("proj b").await.unwrap();
        store
            .set_meta(sid_b, "project_root", &serde_json::json!("D:/projB"))
            .await
            .unwrap();
        // Legacy session with no marker.
        let sid_legacy = store.create_session("legacy").await.unwrap();

        // Explicit marker wins even though the legacy session is newer.
        let found = latest_session_for_project(&store, "D:/projA", None)
            .await
            .unwrap();
        assert_eq!(found.map(|s| s.id), Some(sid_a));

        // No marker on any match → legacy fallback keeps old data reachable.
        let found = latest_session_for_project(&store, "D:/projC", None)
            .await
            .unwrap();
        assert_eq!(found.map(|s| s.id), Some(sid_legacy));

        // Title narrowing (forge check).
        let sid_check = store.create_session("check").await.unwrap();
        store
            .set_meta(sid_check, "project_root", &serde_json::json!("D:/projA"))
            .await
            .unwrap();
        let found = latest_session_for_project(&store, "D:/projA", Some("check"))
            .await
            .unwrap();
        assert_eq!(found.map(|s| s.id), Some(sid_check));
        let _ = (sid_b,);
    }
}
