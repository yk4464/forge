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

    // Reuse the latest check session when present so consecutive
    // `forge check` invocations form one continuous conversation; the
    // agent then reloads the transcript from storage (memory test).
    let sessions = store.list_sessions(50).await?;
    let sid = match sessions.iter().find(|s| s.title == "check") {
        Some(s) => s.id,
        None => store.create_session("check").await?,
    };
    let agent = build_agent(&cfg, &api_key, &tools, &store, &provider, Some(sid)).await;

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
                    if submit && !app.busy {
                        match app.on_submit() {
                            CommandAction::Send(text) => {
                                ensure_session(&mut app, &*store).await;
                                if agent.is_none() {
                                    agent = Some(
                                        build_agent(
                                            &cfg, &api_key, &tools, &store, &provider,
                                            app.session_id,
                                        )
                                        .await,
                                    );
                                }
                                if let Some(a) = &agent {
                                    let (tx, rx) = mpsc::unbounded_channel();
                                    let a = a.clone();
                                    let text = text.clone();
                                    app.busy = true;
                                    tokio::spawn(async move {
                                        if let Err(e) = a.run_turn(&text, tx).await {
                                            let _ = AgentEvent::Error { message: e.to_string() };
                                        }
                                    });
                                    events_rx = rx;
                                }
                            }
                            CommandAction::NewSession => {
                                app.session_id = None;
                                app.lines.clear();
                                app.session_title = "(new session)".into();
                                app.status = "ready".into();
                                agent = None; // fresh context next turn
                                ensure_session(&mut app, &*store).await;
                            }
                            CommandAction::Resume => {
                                let sessions = store.list_sessions(20).await?;
                                if sessions.is_empty() {
                                    app.push_line(app::Line::System(
                                        "no saved sessions".into(),
                                    ));
                                } else {
                                    let latest = &sessions[0];
                                    app.load_session(&store, latest.id).await;
                                    // Rebuild the agent with this session's
                                    // history so conversation continuity works.
                                    agent = Some(
                                        build_agent(
                                            &cfg, &api_key, &tools, &store, &provider,
                                            app.session_id,
                                        )
                                        .await,
                                    );
                                }
                            }
                            CommandAction::Compact => {
                                if agent.is_none() {
                                    agent = Some(
                                        build_agent(
                                            &cfg, &api_key, &tools, &store, &provider,
                                            app.session_id,
                                        )
                                        .await,
                                    );
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

async fn build_agent(
    cfg: &Config,
    api_key: &str,
    tools: &Registry<dyn forge_core::traits::Tool>,
    store: &Arc<SqliteSessionStore>,
    provider: &Arc<dyn forge_core::traits::ModelProvider>,
    session_id: Option<uuid::Uuid>,
) -> Arc<Agent> {
    let mut context = ContextManager::new(cfg.context.clone());
    // Seed the system prompt and restore the persisted transcript so the
    // agent remembers previous turns within this session.
    context.push(Message::system(SYSTEM_PROMPT));
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
            }
            Err(e) => tracing::warn!("cannot load session history: {e}"),
        }
    }
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
        session_id,
    })
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
                app.session_id = Some(id);
                app.session_title = title;
            }
            Err(e) => app.push_line(app::Line::Error(format!("session create failed: {e}"))),
        }
    }
}
