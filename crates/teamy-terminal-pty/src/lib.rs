//! Terminal-oriented process lifetime and I/O around [`portable_pty`].
//!
//! This crate deliberately does not know about windows, Vulkan, terminal
//! parsing, or the application's command-line schema. It owns only the
//! process boundary: spawn a shell in a bounded PTY, send input, resize it,
//! and receive bounded output/exit events.

use eyre::Context;
use eyre::Result;
use portable_pty::Child;
use portable_pty::ChildKiller;
use portable_pty::CommandBuilder;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::native_pty_system;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

const TEAMY_PWSH_OSC133_BOOTSTRAP: &str = r#"$global:__teamy_prompt_seen = $false; if (Test-Path Function:\prompt) { $function:__teamy_original_prompt = $function:prompt }; function global:prompt { $status = if ($?) { 0 } elseif ($null -ne $LASTEXITCODE) { [int]$LASTEXITCODE } else { 1 }; if ($global:__teamy_prompt_seen) { [Console]::Out.Write("`e]133;D;$status`a") } else { $global:__teamy_prompt_seen = $true }; [Console]::Out.Write("`e]133;A`a"); $promptText = if (Test-Path Function:\__teamy_original_prompt) { & $function:__teamy_original_prompt } else { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }; [Console]::Out.Write("`e]133;B`a"); $promptText }"#;

/// Initial terminal dimensions used before a window reports its real size.
pub const DEFAULT_COLUMNS: u16 = 120;
/// Initial terminal dimensions used before a window reports its real size.
pub const DEFAULT_ROWS: u16 = 36;
const EVENT_QUEUE_CAPACITY: usize = 128;
const MAX_OUTPUT_CHUNK: usize = 16 * 1024;

/// Configuration for a shell PTY.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtyConfig {
    /// Executable passed to `portable-pty`.
    pub shell: String,
    /// Optional initial working directory.
    pub current_dir: Option<PathBuf>,
    /// Initial character dimensions.
    pub size: PtySize,
    /// Whether the shell should suppress the user's profile.
    pub no_profile: bool,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            shell: String::from("pwsh.exe"),
            current_dir: std::env::current_dir().ok(),
            size: PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            },
            no_profile: false,
        }
    }
}

impl PtyConfig {
    /// Construct the normal user-facing configuration.
    #[must_use]
    pub fn interactive() -> Self {
        Self::default()
    }

    /// Construct a deterministic test configuration.
    #[must_use]
    pub fn test() -> Self {
        Self {
            no_profile: true,
            ..Self::default()
        }
    }

    fn command(&self) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.shell);
        command.arg("-NoLogo");
        if self.no_profile {
            command.arg("-NoProfile");
        }
        if let Some(current_dir) = &self.current_dir {
            command.cwd(current_dir);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "teamy-terminal");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        if !self.no_profile && is_pwsh_shell(&self.shell) {
            command.env("TEAMY_SHELL_INTEGRATION", "osc133-pwsh-prompt");
            command.arg("-NoExit");
            command.arg("-Command");
            command.arg(TEAMY_PWSH_OSC133_BOOTSTRAP);
        }
        command
    }
}

fn is_pwsh_shell(shell: &str) -> bool {
    std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe"
            )
        })
}

/// A process exit notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtyExit {
    /// Portable-pty's numeric exit code, when one is available.
    pub code: Option<u32>,
    /// Whether the process reported success.
    pub success: bool,
}

/// Events emitted by a running PTY.
#[derive(Debug)]
pub enum PtyEvent {
    /// Bytes read from the PTY master.
    Output(Vec<u8>),
    /// The child process has exited.
    Exited(PtyExit),
    /// A reader or lifecycle error occurred.
    Error(String),
}

/// A bounded, nonblocking PTY session.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    events: Receiver<PtyEvent>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession").finish_non_exhaustive()
    }
}

impl PtySession {
    /// Spawn a shell using the requested PTY configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the PTY cannot be allocated, the shell cannot be
    /// spawned, or the PTY master cannot provide reader/writer handles.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(config.size)
            .map_err(|error| eyre::eyre!("failed to allocate a pseudoterminal: {error}"))?;
        let child = pair
            .slave
            .spawn_command(config.command())
            .map_err(|error| {
                eyre::eyre!("failed to spawn the configured shell in the pseudoterminal: {error}")
            })?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| eyre::eyre!("failed to clone the pseudoterminal reader: {error}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| eyre::eyre!("failed to take the pseudoterminal writer: {error}"))?;
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);

        spawn_reader(reader, sender.clone());
        spawn_waiter(child, sender);

        Ok(Self {
            master: pair.master,
            writer: Mutex::new(writer),
            killer,
            events,
        })
    }

    /// Send raw terminal input to the child.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer is poisoned or the PTY rejects the
    /// bytes.
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|error| eyre::eyre!("pseudoterminal writer lock was poisoned: {error}"))?;
        writer
            .write_all(bytes)
            .wrap_err("failed to write input to the pseudoterminal")?;
        writer
            .flush()
            .wrap_err("failed to flush input to the pseudoterminal")?;
        Ok(())
    }

    /// Resize the PTY in character and pixel units.
    ///
    /// # Errors
    ///
    /// Returns an error when the PTY rejects the new dimensions.
    pub fn resize(
        &self,
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<()> {
        self.master
            .resize(PtySize {
                cols: columns.max(1),
                rows: rows.max(1),
                pixel_width,
                pixel_height,
            })
            .map_err(|error| eyre::eyre!("failed to resize the pseudoterminal: {error}"))
    }

    /// Return the next event without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when the session's synchronization state is poisoned.
    pub fn try_recv(&self) -> Result<Option<PtyEvent>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Ok(Some(PtyEvent::Error(String::from(
                "pseudoterminal event queue disconnected",
            )))),
        }
    }

    /// Wait briefly for the next event, useful for headless tests.
    ///
    /// # Errors
    ///
    /// Returns an error when the session's synchronization state is poisoned.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<PtyEvent>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(Some(PtyEvent::Error(String::from(
                "pseudoterminal event queue disconnected",
            )))),
        }
    }

    /// Ask the child process to terminate.
    ///
    /// # Errors
    ///
    /// Returns an error when the killer lock is poisoned or the child rejects
    /// the termination request.
    pub fn kill(&self) -> Result<()> {
        self.killer
            .lock()
            .map_err(|error| eyre::eyre!("pseudoterminal killer lock was poisoned: {error}"))?
            .kill()
            .wrap_err("failed to terminate the pseudoterminal child")
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, sender: SyncSender<PtyEvent>) {
    std::thread::spawn(move || {
        let mut buffer = vec![0_u8; MAX_OUTPUT_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(length) => {
                    if sender
                        .send(PtyEvent::Output(buffer[..length].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(PtyEvent::Error(format!(
                        "pseudoterminal reader failed: {error}"
                    )));
                    break;
                }
            }
        }
    });
}

fn spawn_waiter(mut child: Box<dyn Child + Send + Sync>, sender: SyncSender<PtyEvent>) {
    std::thread::spawn(move || match child.wait() {
        Ok(status) => {
            let _ = sender.send(PtyEvent::Exited(PtyExit {
                code: Some(status.exit_code()),
                success: status.success(),
            }));
        }
        Err(error) => {
            let _ = sender.send(PtyEvent::Error(format!(
                "pseudoterminal child wait failed: {error}"
            )));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_shell_keeps_the_profile_enabled() {
        assert!(!PtyConfig::interactive().no_profile);
        assert!(PtyConfig::test().no_profile);
    }

    #[test]
    fn command_contains_only_the_test_profile_override() {
        let interactive_command = PtyConfig::interactive().command();
        let test_command = PtyConfig::test().command();
        let interactive = interactive_command.get_argv();
        let test = test_command.get_argv();
        assert!(!interactive.iter().any(|argument| argument == "-NoProfile"));
        assert!(test.iter().any(|argument| argument == "-NoProfile"));
        assert!(interactive.iter().any(|argument| argument == "-NoExit"));
        assert!(interactive.iter().any(|argument| argument == "-Command"));
        assert!(interactive
            .iter()
            .any(|argument| argument == TEAMY_PWSH_OSC133_BOOTSTRAP));
        assert!(!test.iter().any(|argument| argument == "-NoExit"));
        assert!(!test
            .iter()
            .any(|argument| argument == TEAMY_PWSH_OSC133_BOOTSTRAP));
    }

    #[cfg(windows)]
    #[test]
    fn no_profile_shell_round_trips_a_bounded_sequence() {
        let config = PtyConfig::test();
        let session = PtySession::spawn(&config).expect("pwsh should start in a test PTY");
        session
            .write_input(b"Write-Output ((1..1000) -join ','); exit\r")
            .expect("test command should be accepted by pwsh");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut output = Vec::new();
        let mut exit = None;
        while std::time::Instant::now() < deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Output(bytes) => output.extend(bytes),
                PtyEvent::Exited(status) => {
                    exit = Some(status);
                    break;
                }
                PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
            }
        }

        let output = String::from_utf8_lossy(&output);
        assert!(output.contains('1') && output.contains("1000"));
        assert_eq!(exit.map(|status| status.success), Some(true));
    }
}
