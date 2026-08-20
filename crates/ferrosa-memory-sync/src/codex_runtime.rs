//! One-process Codex CLI adapter for the mobile "sandbox light" proof.
//!
//! The adapter never accepts a shell command. It maps typed launch, instruct,
//! interrupt, and cancel operations to fixed `tmux` and `codex exec` argument
//! vectors with one configured workspace root.
//! Correctness: only typed operations reach fixed argument vectors, all input
//! and captured output is bounded, and completion is parsed from Codex JSONL.
//! Last revised: 2026-08-19
//! Last changed: Added the first tmux-light Codex execution adapter.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

/// Maximum instruction text accepted from one mobile command.
pub const MAX_CODEX_INSTRUCTION_BYTES: usize = 16 * 1024;
/// Maximum joined pane capture retained as a durable output event.
pub const MAX_CODEX_CAPTURE_BYTES: usize = 256 * 1024;

/// Fixed configuration for one managed Codex process.
#[derive(Debug, Clone)]
pub struct CodexTmuxConfig {
    pub workspace: PathBuf,
    pub server_fingerprint: String,
    pub codex_binary: PathBuf,
    pub tmux_binary: PathBuf,
    pub completion_timeout: Duration,
    pub poll_interval: Duration,
}

impl CodexTmuxConfig {
    /// Safe default uses binaries resolved through the listener environment;
    /// the session name remains derived solely from the enrolled server key.
    pub fn new(workspace: impl Into<PathBuf>, server_fingerprint: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            server_fingerprint: server_fingerprint.into(),
            codex_binary: PathBuf::from("codex"),
            tmux_binary: PathBuf::from("tmux"),
            completion_timeout: Duration::from_secs(30 * 60),
            poll_interval: Duration::from_millis(250),
        }
    }

    pub fn session_name(&self) -> anyhow::Result<String> {
        let fingerprint = self.server_fingerprint.trim();
        if fingerprint.len() < 12
            || !fingerprint
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            anyhow::bail!("server fingerprint must be at least 12 hexadecimal characters");
        }
        Ok(format!(
            "ferrosa-mobile-{}",
            fingerprint[..12].to_ascii_lowercase()
        ))
    }

    fn validate(&self) -> anyhow::Result<()> {
        self.session_name()?;
        if !self.workspace.is_absolute() {
            anyhow::bail!("Codex workspace must be an absolute path");
        }
        let metadata = std::fs::metadata(&self.workspace).map_err(|error| {
            anyhow::anyhow!(
                "Codex workspace {} is unavailable: {error}",
                self.workspace.display()
            )
        })?;
        if !metadata.is_dir() {
            anyhow::bail!("Codex workspace must be a directory");
        }
        Ok(())
    }
}

/// Parsed, bounded terminal state of one non-interactive Codex turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexRunResult {
    pub thread_id: Option<String>,
    pub final_message: Option<String>,
    pub error: Option<String>,
    pub success: bool,
    pub captured_jsonl: String,
}

/// Runtime errors stay typed so the control dispatcher can persist a stable
/// category without disclosing arbitrary process internals to the gateway.
#[derive(Debug, thiserror::Error)]
pub enum CodexRuntimeError {
    #[error("invalid Codex runtime configuration: {0}")]
    Config(String),
    #[error("invalid Codex instruction: {0}")]
    Instruction(String),
    #[error("tmux operation {operation} failed: {message}")]
    Tmux {
        operation: &'static str,
        message: String,
    },
    #[error("a Codex turn is already running")]
    Busy,
    #[error("timed out waiting for Codex turn completion")]
    Timeout,
    #[error("captured Codex output exceeds {limit} bytes")]
    OutputTooLarge { limit: usize },
    #[error("Codex event stream is invalid: {0}")]
    InvalidEvents(String),
}

/// One configured tmux-light runtime. Cloneable so completion monitoring can
/// continue after the mobile peer disconnects.
#[derive(Debug, Clone)]
pub struct CodexTmuxRuntime {
    config: CodexTmuxConfig,
}

impl CodexTmuxRuntime {
    pub fn new(config: CodexTmuxConfig) -> Result<Self, CodexRuntimeError> {
        config
            .validate()
            .map_err(|error| CodexRuntimeError::Config(error.to_string()))?;
        Ok(Self { config })
    }

    pub fn session_name(&self) -> Result<String, CodexRuntimeError> {
        self.config
            .session_name()
            .map_err(|error| CodexRuntimeError::Config(error.to_string()))
    }

    /// Start a new persisted Codex thread in the configured workspace.
    pub async fn launch(&self, instruction: &str) -> Result<(), CodexRuntimeError> {
        self.start_turn(None, instruction).await
    }

    /// Continue an exact Codex thread created by this managed runtime.
    pub async fn instruct(
        &self,
        thread_id: &str,
        instruction: &str,
    ) -> Result<(), CodexRuntimeError> {
        if uuid::Uuid::parse_str(thread_id).is_err() {
            return Err(CodexRuntimeError::Instruction(
                "thread id must be a UUID".to_owned(),
            ));
        }
        self.start_turn(Some(thread_id), instruction).await
    }

    /// Send Ctrl-C to the one managed pane. No caller-provided key sequence is
    /// accepted.
    pub async fn interrupt(&self) -> Result<(), CodexRuntimeError> {
        let target = self.pane_target()?;
        self.tmux("interrupt", ["send-keys", "-t", &target, "C-c"])
            .await
            .map(|_| ())
    }

    /// Kill only this server's derived tmux session.
    pub async fn cancel(&self) -> Result<(), CodexRuntimeError> {
        let name = self.session_name()?;
        self.tmux("cancel", ["kill-session", "-t", &name])
            .await
            .map(|_| ())
    }

    /// Wait for the running pane to exit, then parse its joined JSONL stream.
    pub async fn wait(&self) -> Result<CodexRunResult, CodexRuntimeError> {
        let started = tokio::time::Instant::now();
        loop {
            if self.pane_is_dead().await? {
                return self.capture_result().await;
            }
            let remaining = self
                .config
                .completion_timeout
                .checked_sub(started.elapsed())
                .ok_or(CodexRuntimeError::Timeout)?;
            tokio::time::sleep(remaining.min(self.config.poll_interval)).await;
        }
    }

    async fn start_turn(
        &self,
        thread_id: Option<&str>,
        instruction: &str,
    ) -> Result<(), CodexRuntimeError> {
        validate_instruction(instruction)?;
        let created = self.ensure_session().await?;
        if !created && !self.pane_is_dead().await? {
            return Err(CodexRuntimeError::Busy);
        }
        let target = self.pane_target()?;
        let workspace = path_text(&self.config.workspace)?;
        let codex = path_text(&self.config.codex_binary)?;
        let mut command = Command::new(&self.config.tmux_binary);
        command.args([
            "respawn-pane",
            "-k",
            "-t",
            &target,
            "-c",
            workspace,
            "--",
            codex,
        ]);
        if let Some(thread_id) = thread_id {
            command.args(["exec", "resume", "--json", thread_id, instruction]);
        } else {
            command.args([
                "exec",
                "--json",
                "--color",
                "never",
                "--sandbox",
                "workspace-write",
                "--cd",
                workspace,
                instruction,
            ]);
        }
        run_tmux(command, "start Codex turn").await.map(|_| ())
    }

    async fn ensure_session(&self) -> Result<bool, CodexRuntimeError> {
        let name = self.session_name()?;
        let exists = Command::new(&self.config.tmux_binary)
            .args(["has-session", "-t", &name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|error| CodexRuntimeError::Tmux {
                operation: "find session",
                message: error.to_string(),
            })?
            .success();
        if !exists {
            let workspace = path_text(&self.config.workspace)?;
            self.tmux(
                "create session",
                ["new-session", "-d", "-s", &name, "-c", workspace],
            )
            .await?;
            self.tmux(
                "retain completed pane",
                ["set-option", "-t", &name, "remain-on-exit", "on"],
            )
            .await?;
            self.tmux(
                "bound pane history",
                ["set-option", "-t", &name, "history-limit", "5000"],
            )
            .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn pane_is_dead(&self) -> Result<bool, CodexRuntimeError> {
        let target = self.pane_target()?;
        let output = self
            .tmux(
                "read pane state",
                ["display-message", "-p", "-t", &target, "#{pane_dead}"],
            )
            .await?;
        Ok(output.trim() == "1")
    }

    async fn capture_result(&self) -> Result<CodexRunResult, CodexRuntimeError> {
        let target = self.pane_target()?;
        let output = self
            .tmux(
                "capture Codex events",
                ["capture-pane", "-p", "-J", "-t", &target, "-S", "-5000"],
            )
            .await?;
        if output.len() > MAX_CODEX_CAPTURE_BYTES {
            return Err(CodexRuntimeError::OutputTooLarge {
                limit: MAX_CODEX_CAPTURE_BYTES,
            });
        }
        parse_codex_jsonl(&output)
    }

    fn pane_target(&self) -> Result<String, CodexRuntimeError> {
        Ok(format!("{}:0.0", self.session_name()?))
    }

    async fn tmux<const N: usize>(
        &self,
        operation: &'static str,
        args: [&str; N],
    ) -> Result<String, CodexRuntimeError> {
        let mut command = Command::new(&self.config.tmux_binary);
        command.args(args);
        run_tmux(command, operation).await
    }
}

async fn run_tmux(
    mut command: Command,
    operation: &'static str,
) -> Result<String, CodexRuntimeError> {
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| CodexRuntimeError::Tmux {
            operation,
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(CodexRuntimeError::Tmux {
            operation,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn validate_instruction(instruction: &str) -> Result<(), CodexRuntimeError> {
    let instruction = instruction.trim();
    if instruction.is_empty() {
        return Err(CodexRuntimeError::Instruction(
            "instruction cannot be empty".to_owned(),
        ));
    }
    if instruction.len() > MAX_CODEX_INSTRUCTION_BYTES {
        return Err(CodexRuntimeError::Instruction(format!(
            "instruction exceeds {MAX_CODEX_INSTRUCTION_BYTES} bytes"
        )));
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, CodexRuntimeError> {
    path.to_str()
        .ok_or_else(|| CodexRuntimeError::Config("runtime paths must be valid UTF-8".to_owned()))
}

/// Parse the documented JSONL event mode while ignoring blank terminal rows.
pub fn parse_codex_jsonl(input: &str) -> Result<CodexRunResult, CodexRuntimeError> {
    let mut thread_id = None;
    let mut final_message = None;
    let mut error = None;
    let mut completed = false;
    let mut saw_event = false;
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let event: Value = serde_json::from_str(line)
            .map_err(|parse_error| CodexRuntimeError::InvalidEvents(parse_error.to_string()))?;
        saw_event = true;
        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                thread_id = event
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("item.completed") => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("agent_message") {
                    final_message = event
                        .pointer("/item/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            Some("turn.completed") => completed = true,
            Some("error") => {
                error = event
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("turn.failed") => {
                error = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(error);
            }
            _ => {}
        }
    }
    if !saw_event {
        return Err(CodexRuntimeError::InvalidEvents(
            "Codex emitted no JSON events".to_owned(),
        ));
    }
    Ok(CodexRunResult {
        thread_id,
        final_message,
        success: completed && error.is_none(),
        error,
        captured_jsonl: input.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_runtime_session_name_is_derived_not_caller_selected() {
        let config = CodexTmuxConfig::new(
            std::env::current_dir().expect("cwd"),
            "D49D0CE1F02281D42E3B530ECCF5EFEBB547EBEBA482E3A8C62225FB33B72F6C",
        );
        assert_eq!(
            config.session_name().expect("valid fingerprint"),
            "ferrosa-mobile-d49d0ce1f022"
        );
    }

    #[test]
    fn codex_runtime_parser_extracts_thread_and_final_message() {
        let input = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"01a01dcf-3c01-76e1-96c8-59a2eb7637c9\"}\n",
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{}}\n",
        );
        let result = parse_codex_jsonl(input).expect("valid event stream");
        assert!(result.success);
        assert_eq!(result.final_message.as_deref(), Some("done"));
        assert_eq!(
            result.thread_id.as_deref(),
            Some("01a01dcf-3c01-76e1-96c8-59a2eb7637c9")
        );
    }

    #[test]
    fn codex_runtime_parser_preserves_usage_limit_as_typed_failure() {
        let input = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"01a01dcf-3c01-76e1-96c8-59a2eb7637c9\"}\n",
            "{\"type\":\"error\",\"message\":\"You've hit your usage limit.\"}\n",
            "{\"type\":\"turn.failed\",\"error\":{\"message\":\"You've hit your usage limit.\"}}\n",
        );
        let result = parse_codex_jsonl(input).expect("valid failure event stream");
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("You've hit your usage limit.")
        );
    }

    #[test]
    fn codex_runtime_rejects_blank_and_oversized_instructions() {
        assert!(validate_instruction("   ").is_err());
        assert!(validate_instruction(&"x".repeat(MAX_CODEX_INSTRUCTION_BYTES + 1)).is_err());
    }
}
