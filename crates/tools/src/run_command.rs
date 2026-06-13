use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::{ToolError, shared::MAX_TOOL_OUTPUT_BYTES, shared::truncate_output, tool_cancel_token};

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_TIMEOUT_SECS: u64 = 3600;
const TERM_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct RunCommandTool {
    cwd: PathBuf,
    default_timeout_secs: u64,
    max_timeout_secs: u64,
    term_grace: Duration,
    max_output_bytes: usize,
}

impl RunCommandTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            default_timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_timeout_secs: MAX_TIMEOUT_SECS,
            term_grace: TERM_GRACE,
            max_output_bytes: MAX_TOOL_OUTPUT_BYTES,
        }
    }

    /// Override the configured limits (from the resolved app config).
    pub fn with_limits(
        mut self,
        default_timeout_secs: u64,
        max_timeout_secs: u64,
        term_grace_ms: u64,
        max_output_bytes: usize,
    ) -> Self {
        self.default_timeout_secs = default_timeout_secs;
        self.max_timeout_secs = max_timeout_secs;
        self.term_grace = Duration::from_millis(term_grace_ms);
        self.max_output_bytes = max_output_bytes;
        self
    }
}

#[derive(Deserialize)]
pub struct RunCommandArgs {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Terminate the command's process group: SIGTERM now, then an unconditional
/// SIGKILL sweep once `TERM_GRACE` elapses. The sweep is unconditional because
/// the direct shell exiting does not mean the group is empty — a grandchild
/// that ignores SIGTERM survives the shell; killing an already-empty group is
/// a harmless ESRCH. The sweep is scheduled through the kill-sweep registry:
/// a detached task fires it after the grace (surviving the tool future being
/// dropped or its turn task being hard-aborted), and exit paths fire any
/// still-pending sweeps immediately via
/// [`crate::sweep_pending_process_kills`] so process exit cannot outrun the
/// kill. The child was spawned with `process_group(0)`, so the group id
/// equals the child pid and the kill reaches everything the shell spawned.
fn kill_group_detached(child: &mut tokio::process::Child, term_grace: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let pgid = pid as libc::pid_t;
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        crate::kill_sweep::schedule_kill_sweep(pgid, term_grace);
        return;
    }
    let _ = child.start_kill();
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
                        "description": format!("Optional timeout in seconds for this command; the process group is killed when it expires. Defaults to {}.", self.default_timeout_secs)
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
            .unwrap_or(self.default_timeout_secs)
            .min(self.max_timeout_secs);
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
            // Cancel/timeout return immediately after arming the detached
            // kill: the caller's tool-batch grace must not depend on how long
            // the process takes to die, and the kill completes even if this
            // future never gets polled again.
            Outcome::Cancelled => {
                kill_group_detached(&mut child, self.term_grace);
                return Err(ToolError::interrupted(format!(
                    "command interrupted before completing: {}",
                    args.command
                )));
            }
            Outcome::TimedOut => {
                kill_group_detached(&mut child, self.term_grace);
                return Err(ToolError::io(format!(
                    "command timed out after {timeout_secs}s: {}",
                    args.command
                )));
            }
        };

        Ok(truncate_output(
            render_output(&stdout, &stderr, Some(status)),
            self.max_output_bytes,
        ))
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

    /// The direct shell can exit on SIGTERM while a grandchild that ignores
    /// SIGTERM lives on in the group — the unconditional SIGKILL sweep must
    /// still reap it.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_sigkills_grandchildren_that_ignore_sigterm() {
        async fn marker_process_alive(marker: &str) -> bool {
            tokio::process::Command::new("pgrep")
                .arg("-f")
                .arg(marker)
                .stdout(Stdio::null())
                .status()
                .await
                .map(|status| status.success())
                .unwrap_or(false)
        }

        let marker = format!("smooth-grandchild-{}", std::process::id());
        // Outer shell dies on SIGTERM; the inner zsh traps (ignores) it.
        let command = format!(r#"zsh -c 'trap "" TERM; sleep 30; : {marker}' & sleep 30"#);

        let token = CancellationToken::new();
        let cancel = token.clone();
        let runner = tool();
        let command_for_call = command;
        let call = tokio::spawn(with_tool_cancel_scope(token, async move {
            runner
                .call(RunCommandArgs {
                    command: command_for_call,
                    timeout_secs: None,
                })
                .await
        }));

        // Wait until the marker shows up in a process command line, plus a
        // beat for the inner shell to install its trap, before cancelling.
        let spawn_deadline = Instant::now() + Duration::from_secs(5);
        while !marker_process_alive(&marker).await {
            assert!(
                Instant::now() < spawn_deadline,
                "test command never started"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();

        let result = call
            .await
            .unwrap_or_else(|err| panic!("join failed: {err}"));
        match result {
            Err(err) => assert!(err.is_interrupted(), "expected interrupted, got {err}"),
            Ok(output) => panic!("expected interruption, got output: {output}"),
        }

        // SIGTERM is immediate; the SIGKILL sweep lands after TERM_GRACE.
        let gone_deadline = Instant::now() + TERM_GRACE + Duration::from_secs(5);
        while marker_process_alive(&marker).await {
            assert!(
                Instant::now() < gone_deadline,
                "SIGTERM-ignoring grandchild survived the SIGKILL sweep"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The exit-path sweep must fire still-pending SIGKILLs immediately —
    /// process exit cannot wait out the SIGTERM grace, and a detached sweep
    /// task would die with the process.
    #[cfg(unix)]
    #[tokio::test]
    async fn exit_sweep_kills_pending_groups_without_waiting_out_the_grace() {
        async fn marker_process_alive(marker: &str) -> bool {
            tokio::process::Command::new("pgrep")
                .arg("-f")
                .arg(marker)
                .stdout(Stdio::null())
                .status()
                .await
                .map(|status| status.success())
                .unwrap_or(false)
        }

        let marker = format!("smooth-exit-sweep-{}", std::process::id());
        let command = format!(r#"zsh -c 'trap "" TERM; sleep 30; : {marker}' & sleep 30"#);

        let token = CancellationToken::new();
        let cancel = token.clone();
        let runner = tool();
        let command_for_call = command;
        let call = tokio::spawn(with_tool_cancel_scope(token, async move {
            runner
                .call(RunCommandArgs {
                    command: command_for_call,
                    timeout_secs: None,
                })
                .await
        }));

        let spawn_deadline = Instant::now() + Duration::from_secs(5);
        while !marker_process_alive(&marker).await {
            assert!(
                Instant::now() < spawn_deadline,
                "test command never started"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        let result = call
            .await
            .unwrap_or_else(|err| panic!("join failed: {err}"));
        assert!(result.is_err(), "expected interruption");

        // Simulate the process-exit path: sweep now, well inside TERM_GRACE.
        let swept_at = Instant::now();
        crate::sweep_pending_process_kills();

        let gone_deadline = swept_at + Duration::from_secs(1);
        while marker_process_alive(&marker).await {
            assert!(
                Instant::now() < gone_deadline,
                "pending group should be SIGKILLed by the exit sweep immediately"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            swept_at.elapsed() < TERM_GRACE,
            "the exit sweep must not depend on the grace elapsing"
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
