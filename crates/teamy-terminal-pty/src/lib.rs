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

const TEAMY_PWSH_OSC133_BOOTSTRAP: &str = r#"if (Get-Module -ListAvailable -Name PSReadLine) { Import-Module PSReadLine -ErrorAction SilentlyContinue }; $global:__teamy_prompt_seen = $false; if (Test-Path Function:\prompt) { $function:__teamy_original_prompt = $function:prompt }; function global:prompt { $status = if ($?) { 0 } elseif ($null -ne $LASTEXITCODE) { [int]$LASTEXITCODE } else { 1 }; if ($global:__teamy_prompt_seen) { [Console]::Out.Write("`e]133;D;$status`a") } else { $global:__teamy_prompt_seen = $true }; [Console]::Out.Write("`e]133;A`a"); $promptText = if (Test-Path Function:\__teamy_original_prompt) { & $function:__teamy_original_prompt } else { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }; [Console]::Out.Write("`e]133;B`a"); $promptText }"#;

/// Per-process discriminator for shell readiness, not an authentication secret.
#[derive(Clone, Debug)]
pub struct ShellReadinessToken(String);

impl Default for ShellReadinessToken {
    fn default() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let value = || {
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        Self(format!("{:016x}{:016x}", value(), value()))
    }
}

impl ShellReadinessToken {
    /// The printable, fixed-length discriminator used in the shell's OSC signal.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
        self.command_with_readiness(None)
    }

    fn command_with_readiness(&self, readiness: Option<&ShellReadinessToken>) -> CommandBuilder {
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
            if let Some(token) = readiness {
                command.arg(format!(r#"{TEAMY_PWSH_OSC133_BOOTSTRAP}; if (Test-Path Function:\PSConsoleHostReadLine) {{ $global:__teamy_original_readline = $function:PSConsoleHostReadLine; $global:__teamy_ready_sent = $false; function global:PSConsoleHostReadLine {{ if (-not $global:__teamy_ready_sent) {{ $global:__teamy_ready_sent = $true; $previousControlCMode = [Console]::TreatControlCAsInput; try {{ [Console]::TreatControlCAsInput = $true; [Console]::Out.Write("`e]1337;TeamyShellReady;{}`a"); & $global:__teamy_original_readline @args }} finally {{ [Console]::TreatControlCAsInput = $previousControlCMode }} }} else {{ & $global:__teamy_original_readline @args }} }} }}"#, token.as_str()));
            } else {
                command.arg(TEAMY_PWSH_OSC133_BOOTSTRAP);
            }
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

/// The reason a PTY event-forwarding loop stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtyEventForwardingExit {
    /// Every producer for the PTY event queue has stopped.
    EventSourceDisconnected,
    /// The actor or other destination stopped accepting forwarded events.
    DestinationDisconnected,
}

/// The process-control half of a PTY session.
///
/// This handle can live in a session actor while [`PtyEventReceiver`] is
/// moved to a notification-driven forwarding thread. Dropping the handle
/// terminates the child process, matching the lifetime behavior of
/// [`PtySession`].
pub struct PtySessionControl {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    #[cfg(windows)]
    process_id: Option<u32>,
}

impl std::fmt::Debug for PtySessionControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySessionControl").finish_non_exhaustive()
    }
}

/// The bounded event-consumer half of a PTY session.
pub struct PtyEventReceiver {
    events: Receiver<PtyEvent>,
}

impl std::fmt::Debug for PtyEventReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyEventReceiver").finish_non_exhaustive()
    }
}

/// A bounded PTY session with backwards-compatible control and receive APIs.
pub struct PtySession {
    control: PtySessionControl,
    events: PtyEventReceiver,
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
        Self::spawn_command(config, config.command())
    }

    /// Spawn with a session-specific readiness signal from the PowerShell read hook.
    ///
    /// # Errors
    /// Returns the same PTY allocation and process errors as [`Self::spawn`].
    pub fn spawn_with_readiness(config: &PtyConfig, token: &ShellReadinessToken) -> Result<Self> {
        Self::spawn_command(config, config.command_with_readiness(Some(token)))
    }

    fn spawn_command(config: &PtyConfig, command: CommandBuilder) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(config.size)
            .map_err(|error| eyre::eyre!("failed to allocate a pseudoterminal: {error}"))?;
        let child = pair.slave.spawn_command(command).map_err(|error| {
            eyre::eyre!("failed to spawn the configured shell in the pseudoterminal: {error}")
        })?;
        #[cfg(windows)]
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
            control: PtySessionControl {
                master: pair.master,
                writer: Mutex::new(writer),
                killer,
                #[cfg(windows)]
                process_id,
            },
            events: PtyEventReceiver { events },
        })
    }

    /// Split process control from event consumption.
    ///
    /// The control half can remain in a session actor while the event half is
    /// blocked in [`PtyEventReceiver::forward_into`] or moved to
    /// [`PtyEventReceiver::spawn_forwarder`]. No queue is added by the split.
    #[must_use]
    pub fn split(self) -> (PtySessionControl, PtyEventReceiver) {
        (self.control, self.events)
    }

    /// Send raw terminal input to the child.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer is poisoned or the PTY rejects the
    /// bytes.
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        self.control.write_input(bytes)
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
        self.control
            .resize(columns, rows, pixel_width, pixel_height)
    }

    /// Return the next event without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when the session's synchronization state is poisoned.
    pub fn try_recv(&self) -> Result<Option<PtyEvent>> {
        self.events.try_recv()
    }

    /// Wait briefly for the next event, useful for headless tests.
    ///
    /// # Errors
    ///
    /// Returns an error when the session's synchronization state is poisoned.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<PtyEvent>> {
        self.events.recv_timeout(timeout)
    }

    /// Ask the child process to terminate.
    ///
    /// # Errors
    ///
    /// Returns an error when the killer lock is poisoned or the child rejects
    /// the termination request.
    pub fn kill(&self) -> Result<()> {
        self.control.kill()
    }
}

impl PtySessionControl {
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

impl PtyEventReceiver {
    /// Return the next event without blocking.
    ///
    /// # Errors
    ///
    /// This receiver currently reports queue disconnection as a terminal
    /// [`PtyEvent::Error`] for compatibility with [`PtySession::try_recv`].
    pub fn try_recv(&self) -> Result<Option<PtyEvent>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Ok(Some(PtyEvent::Error(String::from(
                "pseudoterminal event queue disconnected",
            )))),
        }
    }

    /// Wait for the next event up to `timeout`.
    ///
    /// # Errors
    ///
    /// This receiver currently reports queue disconnection as a terminal
    /// [`PtyEvent::Error`] for compatibility with [`PtySession::recv_timeout`].
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<PtyEvent>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(Some(PtyEvent::Error(String::from(
                "pseudoterminal event queue disconnected",
            )))),
        }
    }

    /// Block on PTY events and forward them into an existing bounded actor
    /// queue.
    ///
    /// The destination must be a [`SyncSender`], so this method cannot hide an
    /// unbounded queue. Backpressure propagates to the existing bounded PTY
    /// event queue while the actor is busy. The mapper lets callers wrap an
    /// event in their own actor-command enum.
    pub fn forward_into<T, F>(
        self,
        destination: &SyncSender<T>,
        mut map_event: F,
    ) -> PtyEventForwardingExit
    where
        F: FnMut(PtyEvent) -> T,
    {
        while let Ok(event) = self.events.recv() {
            if destination.send(map_event(event)).is_err() {
                return PtyEventForwardingExit::DestinationDisconnected;
            }
        }
        PtyEventForwardingExit::EventSourceDisconnected
    }

    /// Spawn a notification-driven bridge into an existing bounded actor
    /// queue.
    ///
    /// The thread blocks on the PTY receiver when idle and on the destination
    /// queue when that bounded queue is full. It performs no timeout polling.
    pub fn spawn_forwarder<T, F>(
        self,
        destination: SyncSender<T>,
        map_event: F,
    ) -> std::thread::JoinHandle<PtyEventForwardingExit>
    where
        T: Send + 'static,
        F: FnMut(PtyEvent) -> T + Send + 'static,
    {
        std::thread::spawn(move || self.forward_into(&destination, map_event))
    }
}

#[cfg(windows)]
mod process_cleanup {
    use eyre::Context;
    use eyre::Result;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows::Win32::Foundation::ERROR_NO_MORE_FILES;
    use windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
    use windows::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
    use windows::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
    use windows::Win32::System::Diagnostics::ToolHelp::Process32NextW;
    use windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
    use windows::Win32::System::Threading::GetExitCodeProcess;
    use windows::Win32::System::Threading::OpenProcess;
    use windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
    use windows::Win32::System::Threading::PROCESS_TERMINATE;
    use windows::Win32::System::Threading::TerminateProcess;
    use windows::core::HRESULT;

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

impl Drop for PtySessionControl {
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

    #[derive(Debug)]
    enum ActorMessage {
        Command(&'static str),
        Pty(PtyEvent),
    }

    fn test_event_queue(capacity: usize) -> (SyncSender<PtyEvent>, PtyEventReceiver) {
        let (sender, events) = mpsc::sync_channel(capacity);
        (sender, PtyEventReceiver { events })
    }

    #[cfg(unix)]
    #[test]
    fn input_before_delayed_readline_delegate_is_preserved() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || sender.send(listener.accept()).unwrap());
        let config = PtyConfig::test();
        let token = ShellReadinessToken::default();
        let generated = config.command_with_readiness(Some(&token));
        let bootstrap = generated.get_argv().last().unwrap().to_str().unwrap();
        // Importing again re-exports the module hook and would erase the test delegate.
        let bootstrap = bootstrap.split_once("; ").unwrap().1;
        // The delegate blocks on an explicit socket barrier before PSReadLine starts.
        // Input is written before releasing the barrier; there are no timing sleeps.
        let program = format!(
            r"Import-Module PSReadLine; $global:__test_reader = $function:PSConsoleHostReadLine; function global:PSConsoleHostReadLine {{ $c = [System.Net.Sockets.TcpClient]::new('127.0.0.1', {port}); $null = $c.GetStream().ReadByte(); $c.Dispose(); & $global:__test_reader @args }}; {bootstrap}"
        );
        let mut command = CommandBuilder::new(&config.shell);
        command.args(["-NoLogo", "-NoProfile", "-NoExit", "-Command", &program]);
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "");
        let session = PtySession::spawn_command(&config, command).unwrap();
        let (control, events) = session.split();
        let marker = format!("TeamyShellReady;{}", token.as_str());
        let mut output = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut released = false;
        let mut answered = 0;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let event = events
                .events
                .recv_timeout(remaining)
                .expect("bounded reader qualification");
            if let PtyEvent::Output(bytes) = event {
                output.push_str(&String::from_utf8_lossy(&bytes));
                let queries = output.matches("\x1b[6n").count();
                for _ in answered..queries {
                    control.write_input(b"\x1b[1;1R").unwrap();
                }
                answered = queries;
                if !released && output.contains(&marker) {
                    let (mut barrier, _) = receiver
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    control
                        .write_input(b"Write-Output ([string]::Concat('DELAYED_', 'READER_OK'))\r")
                        .unwrap();
                    barrier.write_all(&[1]).unwrap();
                    released = true;
                }
                if output.contains("DELAYED_READER_OK") {
                    break;
                }
            } else {
                panic!("reader exited before command execution: {event:?}; {output}");
            }
        }
        control.kill().unwrap();
    }

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
        assert!(
            interactive
                .iter()
                .any(|argument| argument == TEAMY_PWSH_OSC133_BOOTSTRAP)
        );
        assert!(test.iter().any(|argument| argument == "-NoExit"));
        assert!(
            test.iter()
                .any(|argument| argument == TEAMY_PWSH_OSC133_BOOTSTRAP)
        );
    }

    #[test]
    fn event_receiver_preserves_nonblocking_and_timeout_behavior() {
        let (sender, events) = test_event_queue(1);

        assert!(
            events
                .try_recv()
                .expect("empty queue should be healthy")
                .is_none()
        );
        assert!(
            events
                .recv_timeout(Duration::from_millis(1))
                .expect("idle queue should be healthy")
                .is_none()
        );

        sender
            .send(PtyEvent::Output(vec![1, 2, 3]))
            .expect("bounded test queue should accept one event");
        let event = events
            .try_recv()
            .expect("queued event should be readable")
            .expect("queued event should be present");
        assert!(matches!(event, PtyEvent::Output(bytes) if bytes == [1, 2, 3]));

        drop(sender);
        let event = events
            .recv_timeout(Duration::from_millis(1))
            .expect("disconnect remains an event for compatibility")
            .expect("disconnect should produce the compatibility error event");
        assert!(matches!(
            event,
            PtyEvent::Error(error) if error == "pseudoterminal event queue disconnected"
        ));
    }

    #[test]
    fn forwarder_is_notification_driven_and_shares_the_actor_inbox() {
        let (event_sender, events) = test_event_queue(1);
        let (actor_sender, actor_receiver) = mpsc::sync_channel(2);
        let forwarded = events.spawn_forwarder(actor_sender.clone(), ActorMessage::Pty);

        actor_sender
            .send(ActorMessage::Command("resize"))
            .expect("actor command should be accepted");
        assert!(matches!(
            actor_receiver
                .recv_timeout(Duration::from_millis(25))
                .expect("actor command should wake the shared inbox"),
            ActorMessage::Command("resize")
        ));
        assert!(
            actor_receiver
                .recv_timeout(Duration::from_millis(10))
                .is_err(),
            "idle PTY forwarder must not synthesize periodic messages"
        );

        event_sender
            .send(PtyEvent::Output(vec![4, 5, 6]))
            .expect("PTY event should enter its bounded queue");
        let message = actor_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("PTY notification should wake the shared actor inbox");
        assert!(
            matches!(message, ActorMessage::Pty(PtyEvent::Output(bytes)) if bytes == [4, 5, 6])
        );

        drop(actor_receiver);
        event_sender
            .send(PtyEvent::Exited(PtyExit {
                code: Some(0),
                success: true,
            }))
            .expect("source should remain live until forwarding observes actor shutdown");
        assert_eq!(
            forwarded
                .join()
                .expect("forwarding thread should not panic"),
            PtyEventForwardingExit::DestinationDisconnected
        );
    }

    #[test]
    fn forwarder_propagates_bounded_actor_backpressure() {
        let (event_sender, events) = test_event_queue(1);
        let (actor_sender, actor_receiver) = mpsc::sync_channel(1);
        let (mapped_sender, mapped_receiver) = mpsc::sync_channel(0);

        actor_sender
            .send(ActorMessage::Command("occupied"))
            .expect("actor queue should accept its one command");
        let forwarded = events.spawn_forwarder(actor_sender, move |event| {
            mapped_sender
                .send(())
                .expect("test should observe the event before actor delivery");
            ActorMessage::Pty(event)
        });

        event_sender
            .send(PtyEvent::Output(vec![1]))
            .expect("first PTY event should be accepted");
        mapped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder should wake for the first PTY event");
        event_sender
            .send(PtyEvent::Output(vec![2]))
            .expect("one event may remain pending in the bounded PTY queue");
        assert!(matches!(
            event_sender.try_send(PtyEvent::Output(vec![3])),
            Err(mpsc::TrySendError::Full(PtyEvent::Output(bytes))) if bytes == [3]
        ));

        assert!(matches!(
            actor_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("original actor command should remain first"),
            ActorMessage::Command("occupied")
        ));
        assert!(matches!(
            actor_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("first PTY event should resume after actor capacity returns"),
            ActorMessage::Pty(PtyEvent::Output(bytes)) if bytes == [1]
        ));
        mapped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder should then consume the one pending PTY event");
        assert!(matches!(
            actor_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("pending PTY event should reach the actor"),
            ActorMessage::Pty(PtyEvent::Output(bytes)) if bytes == [2]
        ));

        drop(event_sender);
        assert_eq!(
            forwarded
                .join()
                .expect("forwarding thread should not panic"),
            PtyEventForwardingExit::EventSourceDisconnected
        );
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
    #[expect(
        clippy::too_many_lines,
        reason = "the ignored live ConPTY witness keeps its complete setup, interaction, and teardown sequence auditable in one test"
    )]
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
