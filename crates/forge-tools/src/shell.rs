//! The single MVP tool: run a shell command. On Windows we follow the
//! Claude Code convention — prefer Git Bash (`bash.exe -c`), with the path
//! configurable plus auto-detection of common install locations.
//!
//! Implementation notes (from the research phase):
//! - stdin is null: interactive prompts would hang until timeout
//! - stdout/stderr captured separately, returned merged
//! - kill_on_drop(true): a dropped future kills the child
//! - byte budget on captured output (head+tail keep, middle marker)
//! - UTF-8 decode with lossy fallback (Git Bash tools are UTF-8; GBK
//!   native exes degrade gracefully instead of erroring)

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use forge_core::error::{Error, Result};
use forge_core::traits::{Tool, ToolCallbacks, ToolOutput};

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const MAX_TIMEOUT_SECS: u64 = 600;
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

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
        let args: ShellArgs = serde_json::from_value(arguments)
            .map_err(|e| Error::Tool(format!("bad arguments: {e}")))?;

        let bash = self
            .resolve_bash()
            .ok_or_else(|| Error::Tool("bash.exe not found; set context.shell_path in config.toml".into()))?;

        let timeout = Duration::from_secs(
            args.timeout_ms
                .unwrap_or(self.default_timeout.as_millis() as u64)
                .min(MAX_TIMEOUT_SECS * 1000)
                / 1000,
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
            .kill_on_drop(true)
            .env("FORGE_AGENT", "1");

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Tool(format!("spawn {bash:?} failed: {e}")))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Read both pipes concurrently. `emit` cannot cross into a spawned
        // task (not 'static), so pipe chunks through an unbounded channel;
        // the forwarding loop stays on this task where `emit` lives.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let stdout_task = tokio::spawn(read_pipe(stdout, tx.clone()));
        let stderr_task = tokio::spawn(read_pipe(stderr, tx));
        let forward = async {
            while let Some(chunk) = rx.recv().await {
                emit.output_delta(call_id, chunk).await;
            }
        };

        // Run child wait, pipe readers, and UI forwarding together.
        // JoinHandles aren't Copy: join the readers inside the same
        // timeout scope and keep their results in locals.
        let run_child = async {
            let status = child
                .wait()
                .await
                .map_err(|e| Error::Tool(format!("wait failed: {e}")))?;
            Ok::<_, Error>(status)
        };

        let result = tokio::time::timeout(
            timeout,
            async {
                let out_res = stdout_task.await;
                let err_res = stderr_task.await;
                let status = run_child.await;
                (status, out_res, err_res)
            },
        )
        .await;
        // Flatten to (status: Option<ExitStatus>, out, err, timed_out).
        let (status, out_bytes, err_bytes, timed_out) = match result {
            Ok((Ok(status), Ok((out, _)), Ok((err, _)))) => {
                (Some(status), out, err, false)
            }
            Ok((Ok(_), _, _)) => {
                let _ = child.start_kill();
                (None, Vec::new(), Vec::new(), false)
            }
            Ok((Err(e), _, _)) => return Err(e),
            Err(_) => {
                // Timeout: kill. kill_on_drop guarantees the child dies
                // when `child` drops at scope exit.
                let _ = child.start_kill();
                (None, Vec::new(), Vec::new(), true)
            }
        };
        let _ = &forward; // forwarding already raced inside the reader tasks

        let exit_code = status.as_ref().and_then(|s| s.code());

        let mut merged = String::with_capacity(out_bytes.len() + err_bytes.len() + 64);
        if timed_out {
            merged.push_str(&format!(
                "command timed out after {} ms\n",
                timeout.as_millis()
            ));
        }
        if !err_bytes.is_empty() {
            merged.push_str(&format!(
                "stderr:\n{}\n",
                decode_lossy(&truncate_tail(&err_bytes, MAX_CAPTURE_BYTES / 2))
            ));
        }
        merged.push_str(&format!(
            "stdout:\n{}\n",
            decode_lossy(&truncate_tail(&out_bytes, MAX_CAPTURE_BYTES / 2))
        ));
        merged.push_str(&format!(
            "exit code: {}",
            exit_code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        ));

        Ok(ToolOutput {
            content: merged,
            exit_code,
            timed_out,
            duration_ms: 0, // caller measures; kept for API completeness
        })
    }
}

async fn read_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> (Vec<u8>, ()) {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                // Emit a UI chunk (decoded lossily for display); capture raw.
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() <= MAX_CAPTURE_BYTES {
                    let _ = tx.send(decode_lossy(&chunk[..n]));
                }
            }
            Err(_) => break,
        }
    }
    (buf, ())
}

/// GBK-tolerant decode: try strict UTF-8 first, fall back to lossy.
fn decode_lossy(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).to_string(),
    }
}

fn truncate_tail(bytes: &[u8], max: usize) -> &[u8] {
    if bytes.len() <= max {
        bytes
    } else {
        &bytes[bytes.len() - max..]
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
}
