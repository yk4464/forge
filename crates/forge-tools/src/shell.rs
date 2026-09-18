//! The single MVP tool: run a shell command. On Windows we follow the
//! Claude Code convention — prefer Git Bash (`bash.exe -c`), with the path
//! configurable plus auto-detection of common install locations.
//!
//! Implementation notes (from the research phase):
//! - stdin is null: interactive prompts would hang until timeout
//! - stdout/stderr are read concurrently on their own tasks; live chunks
//!   are forwarded to the UI through `ToolCallbacks` while the child runs
//!   (driven by a select loop on this task, where `emit` lives)
//! - kill_on_drop(true): a dropped future kills the child
//! - capture memory is bounded per stream (head+tail keep, middle marker);
//!   the pipes are always drained to EOF so the child can never block
//! - on timeout the child is killed but the already-captured output is
//!   kept and returned
//! - UTF-8 decode with lossy fallback (Git Bash tools are UTF-8; GBK
//!   native exes degrade gracefully instead of erroring)

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::error::{Error, Result};
use forge_core::traits::{Tool, ToolCallbacks, ToolOutput};

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// Per-stream capture budget: head half + tail half, middle bytes counted
/// and marked. The pipes are drained regardless so the child never blocks.
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// Live-UI forwarding budget per call: once this many bytes have been
/// pushed onto the event stream, further chunks are drained but suppressed
/// (merge strategy — the full capture still arrives with the result).
pub const MAX_FORWARD_BYTES: usize = 256 * 1024;
/// How long to keep draining pipes after normal child exit, so output
/// produced just before exit is not lost. Bounded because an orphaned
/// grandchild can hold the write end open (process-tree cleanup is a
/// separate S1 item).
const DRAIN_GRACE: Duration = Duration::from_millis(1500);
/// After a timeout kill we return promptly: captured bytes are already in
/// the shared buffers, so only a short drain for in-flight chunks is
/// warranted. A grandchild may hold the pipe open much longer than this.
const TIMEOUT_DRAIN: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct ShellTool {
    bash_path: Option<PathBuf>,
    default_timeout: Duration,
}

impl ShellTool {
    pub fn new(bash_path: Option<PathBuf>, timeout_secs: u64) -> Self {
        Self {
            bash_path,
            default_timeout: Duration::from_secs(timeout_secs.min(MAX_TIMEOUT_SECS)),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(None, DEFAULT_TIMEOUT_SECS)
    }

    /// Locate bash.exe: explicit path first, then PATH, then the usual Git
    /// for Windows install locations.
    pub fn resolve_bash(&self) -> Option<PathBuf> {
        if let Some(p) = &self.bash_path {
            if p.is_file() {
                return Some(p.clone());
            }
        }
        // PATH lookup via where.exe is more reliable than manual env scan
        // on Windows; fall back to known locations.
        if let Ok(out) = std::process::Command::new("where").arg("bash").output() {
            if out.status.success() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    if let Some(first) = s.lines().next() {
                        let p = PathBuf::from(first.trim());
                        if p.is_file() {
                            return Some(p);
                        }
                    }
                }
            }
        }
        for candidate in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ] {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Executes a shell command (Git Bash) and returns its output. \
         Use this for reading files (cat/ls), searching (grep/find), \
         editing via command-line tools, running builds and tests. \
         Prefer focused commands; avoid interactive ones."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional working directory (Windows path)."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (max 600000)."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        call_id: &str,
        arguments: Value,
        emit: &dyn ToolCallbacks,
    ) -> Result<ToolOutput> {
        let started_at = std::time::Instant::now();
        let args: ShellArgs = serde_json::from_value(arguments)
            .map_err(|e| Error::Tool(format!("bad arguments: {e}")))?;

        let bash = self
            .resolve_bash()
            .ok_or_else(|| Error::Tool("bash.exe not found; set context.shell_path in config.toml".into()))?;

        // timeout_ms is milliseconds: no integer-second truncation here.
        // Values below 100ms clamp up so 0 cannot mean "kill instantly".
        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(self.default_timeout.as_millis() as u64)
                .min(MAX_TIMEOUT_SECS * 1000),
        )
        .max(Duration::from_millis(100));

        let mut cmd = tokio::process::Command::new(&bash);
        cmd.arg("-c").arg(&args.command);
        if let Some(wd) = &args.workdir {
            cmd.current_dir(wd);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Strip credential-shaped variables so a model-executed command
        // cannot `printenv` the API key into tool output (which lands in
        // the model context and logs). FORGE_AGENT is re-added below.
        for (name, _) in std::env::vars() {
            let upper = name.to_uppercase();
            if upper.contains("API_KEY")
                || upper.contains("APIKEY")
                || upper.ends_with("_TOKEN")
                || upper.starts_with("FORGE_")
            {
                cmd.env_remove(&name);
            }
        }
        cmd.env("FORGE_AGENT", "1");

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Tool(format!("spawn {bash:?} failed: {e}")))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Raw chunks flow to this task (where `emit` lives) through an
        // unbounded channel; captured bytes accumulate in shared bounded
        // buffers so results survive a timeout kill.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let stdout_cap = Arc::new(Mutex::new(BoundedCapture::new(MAX_CAPTURE_BYTES)));
        let stderr_cap = Arc::new(Mutex::new(BoundedCapture::new(MAX_CAPTURE_BYTES)));
        let stdout_task = tokio::spawn(read_pipe(stdout, tx.clone(), stdout_cap.clone()));
        let stderr_task = tokio::spawn(read_pipe(stderr, tx, stderr_cap.clone()));

        let mut fwd = LiveForwarder {
            call_id,
            emit,
            bytes_sent: 0,
            noted: false,
        };

        // Phase 1 — drive child exit and live forwarding concurrently.
        // Pipe EOF alone is not an exit condition (a grandchild can hold
        // the write end open), so the loop only leaves on child exit or
        // the deadline; a closed channel just disables that branch.
        let mut exit: Option<std::io::Result<std::process::ExitStatus>> = None;
        let mut timed_out = false;
        let mut rx_closed = false;
        {
            let deadline = tokio::time::Instant::now() + timeout;
            let wait = child.wait();
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline), if exit.is_none() => {
                        timed_out = true;
                        break;
                    }
                    st = &mut wait, if !timed_out => {
                        exit = Some(st);
                        break;
                    }
                    chunk = rx.recv(), if !rx_closed => match chunk {
                        Some(c) => fwd.push(c).await,
                        None => rx_closed = true,
                    }
                }
            }
        }

        if timed_out {
            // The pinned wait future was dropped with the block above, so
            // the child borrow is free here. kill_on_drop backs this up.
            let _ = child.start_kill();
        }

        // Phase 2 — bounded drain: forward what is already in flight and
        // keep reading until EOF. After a timeout the cap is short so the
        // tool result still lands close to the deadline.
        {
            let drain_deadline =
                tokio::time::Instant::now() + if timed_out { TIMEOUT_DRAIN } else { DRAIN_GRACE };
            loop {
                match tokio::time::timeout_at(drain_deadline, rx.recv()).await {
                    Ok(Some(c)) => fwd.push(c).await,
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }
        // Reap the reader tasks best-effort; their capture already lives
        // in the shared buffers, and stragglers finish at pipe EOF.
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            async {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
            },
        )
        .await;

        let status = match exit {
            Some(Ok(s)) => Some(s),
            Some(Err(e)) => return Err(Error::Tool(format!("wait failed: {e}"))),
            // Killed child: reap the status so the exit code is accurate.
            None => tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .ok()
                .and_then(|r| r.ok()),
        };

        let exit_code = status.as_ref().and_then(|s| s.code());

        let mut merged = String::new();
        if timed_out {
            merged.push_str(&format!(
                "command timed out after {} ms (captured output preserved)\n",
                timeout.as_millis()
            ));
        }
        let err_text = stderr_cap
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .render();
        if !err_text.is_empty() {
            merged.push_str(&format!("stderr:\n{err_text}\n"));
        }
        let out_text = stdout_cap
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .render();
        merged.push_str(&format!("stdout:\n{out_text}\n"));
        merged.push_str(&format!(
            "exit code: {}",
            exit_code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        ));

        Ok(ToolOutput {
            content: merged,
            exit_code,
            timed_out,
            duration_ms: started_at.elapsed().as_millis() as u64,
        })
    }
}

async fn read_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    cap: Arc<Mutex<BoundedCapture>>,
) {
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                // Capture is bounded; the pipe is always drained to EOF so
                // the child never blocks on a full pipe buffer.
                cap.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(&chunk[..n]);
                let _ = tx.send(decode_lossy(&chunk[..n]));
            }
            Err(_) => break,
        }
    }
}

/// Bounded head+tail capture: keeps the first `cap/2` and the last `cap/2`
/// bytes, counts everything in between. Invariant:
/// `total_pushed == head.len() + tail.len() + dropped`.
struct BoundedCapture {
    half: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    dropped: u64,
}

impl BoundedCapture {
    fn new(cap: usize) -> Self {
        Self {
            half: cap / 2,
            head: Vec::new(),
            tail: Vec::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        if self.head.len() < self.half {
            let take = (self.half - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        self.tail.extend_from_slice(rest);
        if self.tail.len() > self.half {
            let evict = self.tail.len() - self.half;
            self.tail.drain(..evict);
            self.dropped += evict as u64;
        }
    }

    /// Decoded text with a middle marker when bytes were omitted.
    fn render(&self) -> String {
        let head = decode_lossy(&self.head);
        let tail = decode_lossy(&self.tail);
        if self.dropped == 0 {
            format!("{head}{tail}")
        } else {
            format!(
                "{head}\n\u{2026}[forge] {} bytes of output omitted\u{2026}\n{tail}",
                self.dropped
            )
        }
    }
}

/// Forwards live chunks to the UI until the per-call byte budget is spent,
/// then suppresses further chunks (drain-and-drop, so the event queue
/// stays bounded). The full capture still arrives with `ToolCallCompleted`.
struct LiveForwarder<'a> {
    call_id: &'a str,
    emit: &'a dyn ToolCallbacks,
    bytes_sent: usize,
    noted: bool,
}

impl LiveForwarder<'_> {
    async fn push(&mut self, chunk: String) {
        if self.bytes_sent < MAX_FORWARD_BYTES {
            self.bytes_sent += chunk.len();
            self.emit.output_delta(self.call_id, chunk).await;
        } else if !self.noted {
            self.noted = true;
            self.emit
                .output_delta(
                    self.call_id,
                    "\n\u{2026}[forge] live output capped; full capture follows on completion\u{2026}\n"
                        .into(),
                )
                .await;
        }
    }
}

/// GBK-tolerant decode: try strict UTF-8 first, fall back to lossy.
fn decode_lossy(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    struct NullEmit;

    #[async_trait]
    impl ToolCallbacks for NullEmit {
        async fn output_delta(&self, _call_id: &str, _chunk: String) {}
    }

    /// Thread-safe collector for forwarded live chunks.
    #[derive(Default, Clone)]
    struct Collect(Arc<std::sync::Mutex<Vec<String>>>);

    impl Collect {
        fn contains(&self, needle: &str) -> bool {
            self.0.lock().unwrap().iter().any(|c| c.contains(needle))
        }
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
        fn total_bytes(&self) -> usize {
            self.0.lock().unwrap().iter().map(|c| c.len()).sum()
        }
    }

    #[async_trait]
    impl ToolCallbacks for Collect {
        async fn output_delta(&self, _call_id: &str, chunk: String) {
            self.0.lock().unwrap().push(chunk);
        }
    }

    fn tool() -> ShellTool {
        ShellTool::with_defaults()
    }

    #[tokio::test]
    async fn runs_simple_command() {
        let t = tool();
        let emit = Arc::new(NullEmit);
        let out = t
            .execute("c1", json!({"command": "echo hello"}), &*emit)
            .await
            .unwrap();
        assert!(out.content.contains("hello"), "got: {}", out.content);
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let t = tool();
        let emit = Arc::new(NullEmit);
        let out = t
            .execute("c1", json!({"command": "exit 3"}), &*emit)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
    }

    #[tokio::test]
    async fn timeout_kills() {
        let t = ShellTool::new(None, 1);
        let emit = Arc::new(NullEmit);
        let out = t
            .execute("c1", json!({"command": "sleep 10", "timeout_ms": 1500}), &*emit)
            .await
            .unwrap();
        assert!(out.timed_out);
    }

    #[tokio::test]
    async fn subsecond_timeout_is_preserved() {
        // timeout_ms=300 used to be truncated to 0s (then clamped to 100ms);
        // it must now hold for the full 300ms.
        let t = tool();
        let emit = Arc::new(NullEmit);
        let start = std::time::Instant::now();
        let out = t
            .execute("c1", json!({"command": "sleep 10", "timeout_ms": 300}), &*emit)
            .await
            .unwrap();
        assert!(out.timed_out);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(280) && elapsed < Duration::from_millis(2000),
            "elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn duration_ms_is_measured() {
        let t = tool();
        let emit = Arc::new(NullEmit);
        let out = t
            .execute("c1", json!({"command": "sleep 0.2"}), &*emit)
            .await
            .unwrap();
        assert!(
            out.duration_ms >= 150,
            "expected real duration, got {} ms",
            out.duration_ms
        );
    }

    #[tokio::test]
    async fn secrets_are_stripped_from_child_env() {
        std::env::set_var("FORGE_API_KEY", "supersecret");
        let t = tool();
        let emit = Arc::new(NullEmit);
        let out = t
            .execute("c1", json!({"command": "printenv FORGE_API_KEY || echo stripped"}), &*emit)
            .await
            .unwrap();
        assert!(
            !out.content.contains("supersecret"),
            "API key leaked into tool output: {}",
            out.content
        );
        assert!(out.content.contains("stripped"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn captures_stderr() {
        let t = tool();
        let emit = Arc::new(NullEmit);
        let out = t
            .execute("c1", json!({"command": "echo oops 1>&2"}), &*emit)
            .await
            .unwrap();
        assert!(out.content.contains("oops"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn workdir_is_honored() {
        let t = tool();
        let emit = Arc::new(NullEmit);
        let tmp = std::env::temp_dir();
        let out = t
            .execute(
                "c1",
                json!({"command": "pwd", "workdir": tmp.to_string_lossy()}),
                &*emit,
            )
            .await
            .unwrap();
        assert!(
            out.content.to_lowercase().contains(&tmp.to_string_lossy().to_lowercase().replace("\\\\", "\\")) || out.content.contains("tmp") || out.content.contains("Temp"),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn live_output_is_forwarded_during_execution() {
        // Regression (S0): the forwarding future used to be created but
        // never polled, so nothing reached the UI until the tool returned.
        let t = Arc::new(ShellTool::with_defaults());
        let collect = Collect::default();
        let exec = {
            let t = t.clone();
            let collect = collect.clone();
            tokio::spawn(async move {
                t.execute(
                    "c1",
                    json!({"command": "echo first; sleep 0.6; echo second"}),
                    &collect,
                )
                .await
                .unwrap()
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !collect.contains("first") {
            assert!(
                std::time::Instant::now() < deadline,
                "no live output arrived before the command finished"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !exec.is_finished(),
            "output must arrive while the command is still running"
        );
        let out = exec.await.unwrap();
        assert!(out.content.contains("second"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn large_output_capture_is_bounded_and_forwarding_capped() {
        // Regression (S0): capture used to grow without bound and every
        // chunk was pushed onto the event queue. ~6.9 MB of stdout here.
        let t = Arc::new(ShellTool::with_defaults());
        let collect = Collect::default();
        let out = t
            .execute("c1", json!({"command": "seq 1 1000000"}), &collect)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(
            out.content.len() < 2 * MAX_CAPTURE_BYTES + 4096,
            "captured {} bytes; capture must be bounded",
            out.content.len()
        );
        assert!(
            out.content.contains("bytes of output omitted"),
            "no truncation marker in {} bytes",
            out.content.len()
        );
        assert!(out.content.starts_with("stdout:\n1\n2\n"), "head must be preserved");
        assert!(
            out.content.ends_with("1000000\n\nexit code: 0"),
            "tail must be preserved, got: …{}",
            &out.content[out.content.len().saturating_sub(80)..]
        );
        // Live forwarding is capped (merge strategy); everything beyond the
        // budget is drained but suppressed.
        assert!(
            collect.total_bytes() <= MAX_FORWARD_BYTES + 256,
            "forwarded {} bytes",
            collect.total_bytes()
        );
        assert!(collect.count() > 0, "some live output must still be forwarded");
    }

    #[tokio::test]
    async fn timeout_preserves_partial_output() {
        // Regression (S0): the timeout path used to throw away everything
        // the reader tasks had already captured.
        let t = tool();
        let collect = Collect::default();
        let out = t
            .execute(
                "c1",
                json!({"command": "echo early-marker; sleep 5", "timeout_ms": 800}),
                &collect,
            )
            .await
            .unwrap();
        assert!(out.timed_out);
        assert!(
            out.content.contains("early-marker"),
            "captured output lost on timeout: {}",
            out.content
        );
    }

    #[test]
    fn capture_below_cap_is_contiguous_without_marker() {
        let mut c = BoundedCapture::new(1024);
        c.push(b"hello ");
        c.push(b"world");
        assert_eq!(c.render(), "hello world");
        assert_eq!(c.dropped, 0);
    }

    #[test]
    fn capture_keeps_head_and_tail_with_marker() {
        let mut c = BoundedCapture::new(16); // half = 8
        c.push(b"AAABBBCCCDDDEEEFFF"); // 18 bytes
        let r = c.render();
        assert!(r.starts_with("AAABBBCC"), "{r}");
        assert!(r.ends_with("DDEEEFFF"), "{r}");
        assert!(r.contains("2 bytes of output omitted"), "{r}");
        assert_eq!(c.dropped, 2);
    }

    #[test]
    fn capture_invariant_holds_across_chunks() {
        let mut c = BoundedCapture::new(1024);
        let mut total = 0usize;
        for i in 0..100 {
            let chunk = vec![b'a' + (i % 26) as u8; 97];
            total += chunk.len();
            c.push(&chunk);
        }
        assert_eq!(
            (c.head.len() + c.tail.len()) as u64 + c.dropped,
            total as u64
        );
        assert!(c.dropped > 0);
    }
}
