use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::{ToolError, shared::truncate_output, tool_cancel_token};

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_TIMEOUT_SECS: u64 = 3600;
const TERM_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct RunCommandTool {
    cwd: PathBuf,
}

impl RunCommandTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize)]
pub struct RunCommandArgs {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn default_timeout_secs() -> u64 {
    std::env::var("SMOOTH_CODE_RUN_COMMAND_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Terminate the command's process group: SIGTERM first, then SIGKILL once
/// `TERM_GRACE` elapses without the child exiting. The child was spawned with
/// `process_group(0)`, so the group id equals the child pid and the kill also
/// reaches grandchildren the shell spawned.
async fn kill_group_graceful(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let pgid = pid as libc::pid_t;
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
            if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok() {
                return;
            }
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            let _ = child.wait().await;
            return;
        }
    }
    let _ = child.kill().await;
}

fn render_output(stdout: &[u8], stderr: &[u8], status: Option<std::process::ExitStatus>) -> String {
    let mut text = String::new();
    if !stdout.is_empty() {
        text.push_str(&String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(stderr));
    }
    if let Some(status) = status
        && !status.success()
    {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("command exited with status {status}"));
    }
    text
}

impl Tool for RunCommandTool {
    const NAME: &'static str = "run_command";

    type Error = ToolError;
    type Args = RunCommandArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Run a shell command inside the current workspace and return combined stdout/stderr. Use this for inspection, validation, formatters, and project commands; use structured file tools such as edit, write, and delete for source changes instead of shell rewrite scripts.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Optional timeout in seconds for this command; the process group is killed when it expires. Defaults to 300."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let timeout_secs = args
            .timeout_secs
            .filter(|secs| *secs > 0)
            .unwrap_or_else(default_timeout_secs)
            .min(MAX_TIMEOUT_SECS);
        let timeout = Duration::from_secs(timeout_secs);

        let mut command = tokio::process::Command::new("zsh");
        command
            .arg("-lc")
            .arg(&args.command)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|err| ToolError::io(format!("failed to run command: {err}")))?;

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        enum Outcome {
            Done(std::io::Result<std::process::ExitStatus>),
            Cancelled,
            TimedOut,
        }

        let token = tool_cancel_token();
        let outcome = {
            // Reading the pipes alongside `wait()` keeps the child from
            // blocking on a full pipe; the reads finish once the child (and
            // anything holding the pipe write ends) exits or is killed.
            let wait_with_output = async {
                let stdout_read = async {
                    if let Some(pipe) = stdout_pipe.as_mut() {
                        let _ = pipe.read_to_end(&mut stdout).await;
                    }
                };
                let stderr_read = async {
                    if let Some(pipe) = stderr_pipe.as_mut() {
                        let _ = pipe.read_to_end(&mut stderr).await;
                    }
                };
                let ((), (), status) = tokio::join!(stdout_read, stderr_read, child.wait());
                status
            };
            tokio::select! {
                status = wait_with_output => Outcome::Done(status),
                _ = token.cancelled() => Outcome::Cancelled,
                _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            }
        };

        let status = match outcome {
            Outcome::Done(status) => {
                status.map_err(|err| ToolError::io(format!("failed to run command: {err}")))?
            }
            Outcome::Cancelled => {
                kill_group_graceful(&mut child).await;
                return Err(ToolError::interrupted(format!(
                    "command interrupted before completing: {}",
                    args.command
                )));
            }
            Outcome::TimedOut => {
                kill_group_graceful(&mut child).await;
                return Err(ToolError::io(format!(
                    "command timed out after {timeout_secs}s: {}",
                    args.command
                )));
            }
        };

        Ok(truncate_output(render_output(
            &stdout,
            &stderr,
            Some(status),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::with_tool_cancel_scope;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    fn tool() -> RunCommandTool {
        RunCommandTool::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn runs_command_and_returns_combined_output() {
        let output = tool()
            .call(RunCommandArgs {
                command: "echo out; echo err 1>&2".to_string(),
                timeout_secs: None,
            })
            .await
            .unwrap_or_else(|err| panic!("command should succeed: {err}"));
        assert!(output.contains("out"));
        assert!(output.contains("err"));
    }

    #[tokio::test]
    async fn reports_non_zero_exit_status() {
        let output = tool()
            .call(RunCommandArgs {
                command: "exit 3".to_string(),
                timeout_secs: None,
            })
            .await
            .unwrap_or_else(|err| panic!("command should succeed: {err}"));
        assert!(output.contains("command exited with status"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_process_group_and_returns_interrupted() {
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let started = Instant::now();
        let result = with_tool_cancel_scope(
            token,
            tool().call(RunCommandArgs {
                command: "sleep 30 & sleep 30".to_string(),
                timeout_secs: None,
            }),
        )
        .await;

        match result {
            Err(err) => assert!(err.is_interrupted(), "expected interrupted, got {err}"),
            Ok(output) => panic!("expected interruption, got output: {output}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel should not wait for the command"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let started = Instant::now();
        let result = tool()
            .call(RunCommandArgs {
                command: "sleep 30".to_string(),
                timeout_secs: Some(1),
            })
            .await;
        match result {
            Err(ToolError::Io { message }) => {
                assert!(message.contains("timed out"), "unexpected error: {message}")
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout should not wait for the command"
        );
    }
}
