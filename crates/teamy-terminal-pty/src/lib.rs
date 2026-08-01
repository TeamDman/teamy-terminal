//! Terminal-oriented process lifetime and I/O around [`portable_pty`].
//!
//! This crate deliberately does not know about windows, Vulkan, terminal
//! parsing, or the application's command-line schema. It owns only the
//! process boundary: spawn a shell in a bounded PTY, send input, resize it,
//! and receive bounded output/exit events.

use eyre::Context;
use eyre::Result;
use portable_pty::native_pty_system;
use portable_pty::Child;
use portable_pty::ChildKiller;
use portable_pty::CommandBuilder;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

const TEAMY_PWSH_OSC133_BOOTSTRAP: &str = r#"if (Get-Module -ListAvailable -Name PSReadLine) { Import-Module PSReadLine -ErrorAction SilentlyContinue }; $global:__teamy_prompt_seen = $false; if (Test-Path Function:\prompt) { $function:__teamy_original_prompt = $function:prompt }; function global:prompt { $status = if ($?) { 0 } elseif ($null -ne $LASTEXITCODE) { [int]$LASTEXITCODE } else { 1 }; if ($global:__teamy_prompt_seen) { [Console]::Out.Write("`e]133;D;$status`a") } else { $global:__teamy_prompt_seen = $true }; [Console]::Out.Write("`e]133;A`a"); $promptText = if (Test-Path Function:\__teamy_original_prompt) { & $function:__teamy_original_prompt } else { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }; [Console]::Out.Write("`e]133;B`a"); $promptText }"#;

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
        if is_pwsh_shell(&self.shell) {
            if !self.no_profile {
                command.env("TEAMY_SHELL_INTEGRATION", "osc133-pwsh-prompt");
            }
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
    process_id: Option<u32>,
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
        let process_id = child.process_id();
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
            process_id,
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
        let result = self
            .killer
            .lock()
            .map_err(|error| eyre::eyre!("pseudoterminal killer lock was poisoned: {error}"))?
            .kill()
            .wrap_err("failed to terminate the pseudoterminal child");
        if result.is_ok() {
            return Ok(());
        }

        #[cfg(windows)]
        {
            let primary_error = result.expect_err("the successful termination case returned early");
            match terminate_process_tree(self.process_id) {
                Ok(()) => Ok(()),
                Err(fallback_error) => Err(eyre::eyre!(
                    "{primary_error:#}; Windows process-tree fallback also failed: {fallback_error:#}"
                )),
            }
        }

        #[cfg(not(windows))]
        result
    }
}

#[cfg(windows)]
mod process_cleanup {
    use eyre::Context;
    use eyre::Result;
    use windows::core::HRESULT;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows::Win32::Foundation::ERROR_NO_MORE_FILES;
    use windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
    use windows::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
    use windows::Win32::System::Diagnostics::ToolHelp::Process32NextW;
    use windows::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
    use windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
    use windows::Win32::System::Threading::GetExitCodeProcess;
    use windows::Win32::System::Threading::OpenProcess;
    use windows::Win32::System::Threading::TerminateProcess;
    use windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
    use windows::Win32::System::Threading::PROCESS_TERMINATE;

    const STILL_ACTIVE: u32 = 259;
    type ProcessInfo = (u32, u32, String);

    struct OwnedHandle(windows::Win32::Foundation::HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by a successful Win32 API call
            // and is owned exclusively by this guard.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn is_error(
        error: &windows::core::Error,
        code: windows::Win32::Foundation::WIN32_ERROR,
    ) -> bool {
        error.code() == HRESULT::from_win32(code.0)
    }

    fn snapshot_processes() -> Result<Vec<ProcessInfo>> {
        // SAFETY: the snapshot flags and process id are valid for a system-wide
        // process snapshot; the returned handle is owned by `OwnedHandle`.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .wrap_err("failed to snapshot Windows processes for PTY cleanup")
            .map(OwnedHandle)?;
        let mut processes = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>())
                .expect("PROCESSENTRY32W fits in u32"),
            ..Default::default()
        };
        // SAFETY: `entry` is initialized with the required size and remains
        // alive for the duration of the enumeration call.
        unsafe { Process32FirstW(snapshot.0, &raw mut entry) }
            .wrap_err("failed to enumerate Windows processes for PTY cleanup")?;
        loop {
            let executable_name = String::from_utf16_lossy(
                &entry.szExeFile[..entry
                    .szExeFile
                    .iter()
                    .position(|character| *character == 0)
                    .unwrap_or(entry.szExeFile.len())],
            );
            processes.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                executable_name,
            ));
            // SAFETY: `entry` is the same valid enumeration buffer used for
            // `Process32FirstW` and remains alive for this call.
            match unsafe { Process32NextW(snapshot.0, &raw mut entry) } {
                Ok(()) => {}
                Err(error) if is_error(&error, ERROR_NO_MORE_FILES) => break,
                Err(error) => {
                    return Err(eyre::eyre!(
                        "failed to continue enumerating Windows processes for PTY cleanup: {error}"
                    ));
                }
            }
        }
        Ok(processes)
    }

    fn process_tree(root: u32, processes: &[ProcessInfo]) -> Vec<u32> {
        let mut tree = vec![root];
        let mut index = 0;
        while index < tree.len() {
            let parent_id = tree[index];
            for &(child_id, child_parent_id, _) in processes {
                if child_parent_id == parent_id && !tree.contains(&child_id) {
                    tree.push(child_id);
                }
            }
            index += 1;
        }
        tree
    }

    fn terminate_processes(tree: Vec<u32>, processes: &[ProcessInfo]) -> Result<()> {
        for process_id in tree.into_iter().rev() {
            let executable_name = processes
                .iter()
                .find(|(candidate_id, _, _)| *candidate_id == process_id)
                .map_or("unknown", |(_, _, name)| name.as_str());
            // SAFETY: the requested access is limited to process termination
            // and exit-state inspection for the PTY-owned process tree.
            let process = match unsafe {
                OpenProcess(
                    PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION,
                    false,
                    process_id,
                )
            } {
                Ok(process) => OwnedHandle(process),
                // A PTY can exit between the portable-pty kill attempt and
                // this fallback. Termination is idempotent in that case.
                Err(error) if is_error(&error, ERROR_INVALID_PARAMETER) => continue,
                Err(error) => {
                    return Err(eyre::eyre!(
                        "failed to open pseudoterminal process {process_id} ({executable_name}) for termination: {error}"
                    ));
                }
            };
            let mut exit_code = 0_u32;
            // SAFETY: `process` is a valid handle with query access and
            // `exit_code` is a writable output value.
            unsafe { GetExitCodeProcess(process.0, &raw mut exit_code) }.wrap_err_with(|| {
                format!("failed to query pseudoterminal process {process_id} ({executable_name})")
            })?;
            if exit_code != STILL_ACTIVE {
                continue;
            }
            // SAFETY: `process` is a valid handle with terminate access.
            if let Err(error) = unsafe { TerminateProcess(process.0, 1) } {
                let mut final_exit_code = 0_u32;
                // SAFETY: `process` remains valid through `OwnedHandle` and
                // `final_exit_code` is a writable output value.
                let process_exited =
                    unsafe { GetExitCodeProcess(process.0, &raw mut final_exit_code) }
                        .is_ok_and(|()| final_exit_code != STILL_ACTIVE);
                if process_exited {
                    continue;
                }
                return Err(eyre::eyre!(
                    "failed to terminate pseudoterminal process {process_id} ({executable_name}): {error}"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn terminate(process_id: Option<u32>) -> Result<()> {
        let process_id = process_id.ok_or_else(|| eyre::eyre!("PTY child has no process id"))?;
        // Terminate the PTY process tree through typed Win32 APIs. The snapshot
        // is intentionally taken once so the fallback is bounded and only
        // follows descendants that existed at close time.
        let processes = snapshot_processes()?;
        terminate_processes(process_tree(process_id, &processes), &processes)
    }
}

#[cfg(windows)]
fn terminate_process_tree(process_id: Option<u32>) -> Result<()> {
    process_cleanup::terminate(process_id)
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
        assert!(test.iter().any(|argument| argument == "-NoExit"));
        assert!(test
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

    #[cfg(windows)]
    #[test]
    fn active_shell_process_tree_can_be_terminated() {
        let session =
            PtySession::spawn(&PtyConfig::test()).expect("pwsh should start in a test PTY");
        session
            .write_input(b"ping.exe 127.0.0.1 -t\r")
            .expect("the long-running command should be accepted by pwsh");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while std::time::Instant::now() < deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Output(bytes) => {
                    output.extend(bytes);
                    if String::from_utf8_lossy(&output).contains("Pinging 127.0.0.1") {
                        break;
                    }
                }
                PtyEvent::Exited(status) => panic!("PowerShell exited before ping: {status:?}"),
                PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("Pinging 127.0.0.1"),
            "the long-running command did not start; output was:\n{}",
            String::from_utf8_lossy(&output)
        );
        session
            .kill()
            .expect("the PTY child should be terminable by its process tree");
        let exit_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < exit_deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Exited(_) => return,
                PtyEvent::Output(_) => {}
                PtyEvent::Error(error) => panic!("test PTY failed after termination: {error}"),
            }
        }
        panic!("the terminated PTY did not report process exit");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the desktop account; the Codex sandbox cannot deliver ConPTY console-control events"]
    fn interactive_shell_ctrl_c_interrupts_a_running_command() {
        let config = PtyConfig::interactive();
        let session = PtySession::spawn(&config).expect("pwsh should start in a test PTY");
        session
            .resize(100, 30, 0, 0)
            .expect("test PTY should accept an initial window resize");
        let respond_to_cursor_queries = |bytes: &[u8]| {
            let queries = bytes
                .windows(4)
                .filter(|window| *window == b"\x1b[6n")
                .count();
            for _ in 0..queries {
                session
                    .write_input(b"\x1b[1;1R")
                    .expect("cursor report should be accepted by the PTY");
            }
            let device_attribute_queries = bytes
                .windows(3)
                .filter(|window| *window == b"\x1b[c")
                .count();
            for _ in 0..device_attribute_queries {
                session
                    .write_input(b"\x1b[?1;2c")
                    .expect("device-attribute report should be accepted by the PTY");
            }
        };
        session
            .write_input(b"Write-Output READY\r")
            .expect("readiness command should be accepted by pwsh");
        let readiness_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while std::time::Instant::now() < readiness_deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Output(bytes) => {
                    respond_to_cursor_queries(&bytes);
                    output.extend(bytes);
                }
                PtyEvent::Exited(status) => {
                    panic!("PowerShell exited before readiness: {status:?}")
                }
                PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
            }
            if String::from_utf8_lossy(&output).contains("READY") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("READY"),
            "PowerShell readiness command did not complete; output was:\n{}",
            String::from_utf8_lossy(&output)
        );
        session
            .write_input(b"ping.exe 127.0.0.1 -t\r")
            .expect("long-running test command should be accepted by pwsh");
        let ping_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < ping_deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Output(bytes) => {
                    respond_to_cursor_queries(&bytes);
                    output.extend(bytes);
                }
                PtyEvent::Exited(status) => {
                    panic!("PowerShell exited before ping started: {status:?}")
                }
                PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
            }
            if String::from_utf8_lossy(&output).contains("Pinging 127.0.0.1") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("Pinging 127.0.0.1"),
            "the native console command did not start; output was:\n{}",
            String::from_utf8_lossy(&output)
        );

        // Mirror the actual interactive winit path: the C key-down/up records
        // carry LEFT_CTRL_PRESSED, without separate Ctrl modifier records.
        for bytes in [
            b"\x1b[67;46;3;1;8;1_".as_slice(),
            b"\x1b[67;46;0;0;8;1_".as_slice(),
        ] {
            session
                .write_input(bytes)
                .expect("Ctrl+C should be accepted by the PTY");
            std::thread::sleep(Duration::from_millis(10));
        }
        let interrupt_observation_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < interrupt_observation_deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Output(bytes) => {
                    respond_to_cursor_queries(&bytes);
                    output.extend(bytes);
                }
                PtyEvent::Exited(status) => {
                    panic!("PowerShell exited after the interrupt: {status:?}")
                }
                PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
            }
        }

        std::thread::sleep(Duration::from_millis(500));

        session
            .write_input(b"Write-Output INTERRUPTED\r")
            .expect("follow-up command should be accepted by pwsh");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let Some(event) = session
                .recv_timeout(Duration::from_millis(100))
                .expect("PTY event receive should remain healthy")
            else {
                continue;
            };
            match event {
                PtyEvent::Output(bytes) => {
                    respond_to_cursor_queries(&bytes);
                    output.extend(bytes);
                }
                PtyEvent::Exited(status) => {
                    panic!("PowerShell exited before the interrupt test completed: {status:?}")
                }
                PtyEvent::Error(error) => panic!("test PTY failed: {error}"),
            }
            if String::from_utf8_lossy(&output).contains("INTERRUPTED") {
                return;
            }
        }

        panic!(
            "Ctrl+C did not return control to PowerShell within the test bound; output was:\n{}",
            String::from_utf8_lossy(&output)
        );
    }
}
