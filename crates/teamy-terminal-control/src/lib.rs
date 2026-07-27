//! Typed cross-platform control-plane service for live Teamy Terminal windows.
//!
//! Vox owns the RPC framing, request correlation, schema, and local transport:
//! Unix sockets on Unix and named pipes on Windows. The application still owns
//! terminal/window mutation; requests are handed to the winit thread through a
//! bounded channel.

use eyre::Context;
use eyre::Result;
use facet::Facet;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;
use std::time::Duration;

/// Version of the Teamy Terminal control service contract.
pub const PROTOCOL_VERSION: u32 = 1;
/// Maximum UTF-8 input accepted through one control request.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Maximum number of key actions accepted through one control request.
pub const MAX_KEY_ACTIONS: usize = 4096;
/// Maximum delay accepted in a scripted key sequence.
pub const MAX_KEY_DELAY_MS: u16 = 5_000;
/// Maximum number of font-size steps accepted by one control request.
pub const MAX_ZOOM_STEPS: i16 = 64;
const CONTROL_DIRECTORY_NAME: &str = "teamy-terminal";
const INSTANCE_DIRECTORY_NAME: &str = "instances";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Metadata used to discover a live interactive application instance.
#[derive(Clone, Debug, Facet, PartialEq, Eq)]
pub struct InstanceInfo {
    /// Stable identifier for this MVP instance. It is the owning process ID.
    pub id: u32,
    /// Owning process ID, retained explicitly for diagnostics.
    pub pid: u32,
    /// Window title exposed by the application.
    pub title: String,
    /// Vox local endpoint address.
    pub endpoint: String,
}

impl InstanceInfo {
    /// Construct the current process's instance metadata.
    #[must_use]
    pub fn current(title: impl Into<String>) -> Self {
        let pid = std::process::id();
        Self {
            id: pid,
            pid,
            title: title.into(),
            endpoint: local_endpoint(pid),
        }
    }
}

/// A physical key that can be pressed or released by a scripted key sequence.
#[derive(Clone, Debug, Facet, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlPhysicalKey {
    /// A single Unicode character key.
    Character { text: String },
    Ctrl,
    Shift,
    Alt,
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
}

/// A key action sent to the control service.
///
/// This type intentionally derives Vox's Facet version, not the CLI's Facet
/// version: Vox owns this type's wire schema and can evolve it independently.
#[derive(Clone, Debug, Facet, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlKey {
    /// A Unicode text fragment.
    Character {
        text: String,
    },
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
    CtrlA,
    CtrlArrowUp,
    CtrlArrowDown,
    CtrlArrowRight,
    CtrlArrowLeft,
    CtrlHome,
    CtrlEnd,
    CtrlDelete,
    CtrlPageUp,
    CtrlPageDown,
    /// A control character not assigned a dedicated named variant.
    CtrlCharacter {
        character: char,
    },
    CtrlC,
    CtrlD,
    CtrlZ,
    CtrlL,
    /// Paste text without requiring the target process to access a clipboard.
    Paste {
        text: String,
    },
    /// Press one physical key. Scripted sequences always release outstanding
    /// keys when the request finishes.
    KeyDown { key: ControlPhysicalKey },
    /// Release one physical key.
    KeyUp { key: ControlPhysicalKey },
    /// Pause a scripted key sequence for a bounded duration.
    Delay { milliseconds: u16 },
}

/// An action sent to the owning application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Verify that the target is alive and speaks this service.
    Ping,
    /// Send UTF-8 text to the target PTY.
    SendText { text: String },
    /// Send already-decoded terminal key actions to the target PTY.
    SendKeys { keys: Vec<ControlKey> },
    /// Request a window resize in physical pixels.
    Resize { width: u32, height: u32 },
    /// Encode the latest terminal frame as a PNG at the caller-resolved path.
    Screenshot { path: String },
    /// Return the latest visible terminal grid as newline-separated text.
    GetContent,
    /// Set a visible-grid selection by its bounded anchor and focus cells.
    Select {
        anchor_column: u16,
        anchor_row: u16,
        focus_column: u16,
        focus_row: u16,
    },
    /// Copy the active selection to the system clipboard and return its text.
    CopySelection,
    /// Adjust the terminal font size by bounded one-pixel steps.
    Zoom { steps: i16 },
    /// Show or hide the native terminal window.
    SetVisible { visible: bool },
    /// Close the terminal window and its shell.
    Close,
}

/// The response returned by the control client and exposed by CLI output.
#[derive(Clone, Debug, Facet, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlResponse {
    /// The request completed successfully.
    Ok,
    /// The latest visible terminal grid.
    Content { text: String },
}

/// A request waiting to be applied by the owning winit application thread.
#[derive(Debug)]
pub struct ControlCall {
    /// The decoded operation.
    pub operation: ControlAction,
    /// One-shot response channel back to the Vox service task.
    pub responder: tokio::sync::oneshot::Sender<Result<ControlResponse, String>>,
}

/// Parse the explicit key-action syntax used by
/// `teamy-terminal invoke ... send-keys`.
///
/// Every token must be one of `down:<key>`, `up:<key>`, or
/// `wait:<milliseconds>`. Raw text belongs to `send-text`; there are no
/// implicit chords, delays, or numeric-key special cases in this grammar.
///
/// # Errors
///
/// Returns an error when the sequence exceeds the control-plane action bound.
pub fn parse_key_sequence(spec: &str) -> Result<Vec<ControlKey>> {
    let tokens: Vec<_> = spec.split_whitespace().collect();
    if tokens.is_empty() {
        eyre::bail!(
            "send-keys requires explicit actions: down:<key>, up:<key>, or wait:<milliseconds>"
        );
    }

    let mut actions = Vec::with_capacity(tokens.len());
    for token in tokens {
        let (operation, value) = token.split_once(':').ok_or_else(|| {
            eyre::eyre!(
                "invalid send-keys action {token:?}; use down:<key>, up:<key>, or wait:<milliseconds>"
            )
        })?;
        if value.is_empty() {
            eyre::bail!("send-keys action {token:?} has an empty value")
        }
        match operation {
            "down" | "up" => {
                let key = parse_physical_key(value)
                    .ok_or_else(|| eyre::eyre!("unknown physical key: {value}"))?;
                actions.push(if operation == "down" {
                    ControlKey::KeyDown { key }
                } else {
                    ControlKey::KeyUp { key }
                });
            }
            "wait" => {
                let milliseconds = value.parse::<u16>().map_err(|_| {
                    eyre::eyre!("invalid send-keys wait duration: {value:?}")
                })?;
                if milliseconds > MAX_KEY_DELAY_MS {
                    eyre::bail!(
                        "key delay exceeds the {MAX_KEY_DELAY_MS}ms control limit"
                    );
                }
                actions.push(ControlKey::Delay { milliseconds });
            }
            _ => eyre::bail!(
                "unknown send-keys action {operation:?}; use down:<key>, up:<key>, or wait:<milliseconds>"
            ),
        }
    }
    if actions.len() > MAX_KEY_ACTIONS {
        eyre::bail!("control key sequence exceeds the {MAX_KEY_ACTIONS}-action limit")
    }
    Ok(actions)
}

fn parse_physical_key(spec: &str) -> Option<ControlPhysicalKey> {
    let normalized = spec.to_ascii_lowercase();
    let key = match normalized.as_str() {
        "ctrl" | "control" => ControlPhysicalKey::Ctrl,
        "shift" => ControlPhysicalKey::Shift,
        "alt" => ControlPhysicalKey::Alt,
        "enter" => ControlPhysicalKey::Enter,
        "tab" => ControlPhysicalKey::Tab,
        "backspace" => ControlPhysicalKey::Backspace,
        "escape" | "esc" => ControlPhysicalKey::Escape,
        "up" | "arrowup" | "arrow-up" => ControlPhysicalKey::ArrowUp,
        "down" | "arrowdown" | "arrow-down" => ControlPhysicalKey::ArrowDown,
        "right" | "arrowright" | "arrow-right" => ControlPhysicalKey::ArrowRight,
        "left" | "arrowleft" | "arrow-left" => ControlPhysicalKey::ArrowLeft,
        "home" => ControlPhysicalKey::Home,
        "end" => ControlPhysicalKey::End,
        "delete" | "del" => ControlPhysicalKey::Delete,
        "pageup" | "page-up" => ControlPhysicalKey::PageUp,
        "pagedown" | "page-down" => ControlPhysicalKey::PageDown,
        _ if spec.chars().count() == 1 => ControlPhysicalKey::Character {
            text: spec.to_owned(),
        },
        _ => return None,
    };
    Some(key)
}

/// Return the per-user discovery directory used by live instances.
///
/// # Errors
///
/// Returns an error if the temporary directory cannot be resolved or created.
pub fn instance_directory() -> Result<PathBuf> {
    let directory = std::env::temp_dir()
        .join(CONTROL_DIRECTORY_NAME)
        .join(INSTANCE_DIRECTORY_NAME);
    std::fs::create_dir_all(&directory)
        .wrap_err("failed to create the Teamy Terminal instance directory")?;
    Ok(directory)
}

/// List instance records currently present in the discovery directory.
///
/// # Errors
///
/// Returns an error if the directory cannot be read.
pub fn list_instances() -> Result<Vec<InstanceInfo>> {
    let directory = instance_directory()?;
    let mut instances = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .wrap_err("failed to enumerate Teamy Terminal instance records")?
    {
        let entry = entry.wrap_err("failed to inspect a Teamy Terminal instance record")?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| facet_json::from_str::<InstanceInfo>(&text).ok())
        {
            Some(instance) => instances.push(instance),
            None => {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    instances.sort_by_key(|instance| instance.id);
    Ok(instances)
}

/// Find an instance by its numeric identifier.
///
/// # Errors
///
/// Returns an error if discovery fails or no matching instance exists.
pub fn find_instance(id: u32) -> Result<InstanceInfo> {
    list_instances()?
        .into_iter()
        .find(|instance| instance.id == id)
        .ok_or_else(|| eyre::eyre!("no Teamy Terminal instance with id {id} is registered"))
}

/// Send a request to a live instance and wait for its response.
///
/// # Errors
///
/// Returns an error if the endpoint cannot be opened, the service does not
/// respond, or the target reports failure.
pub fn invoke(instance: &InstanceInfo, action: ControlAction) -> Result<ControlResponse> {
    validate_action(&action)?;
    let endpoint = instance.endpoint.clone();
    tracing::debug!(%endpoint, "connecting to Teamy Terminal Vox control service");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .wrap_err("failed to create the Teamy Terminal control client runtime")?;
    runtime.block_on(async move {
        let client: TerminalControlClient = vox::connect_lane::<TerminalControlClient>(endpoint)
            .connect_timeout(REQUEST_TIMEOUT)
            .wait_for_service(REQUEST_TIMEOUT)
            .await
            .wrap_err("failed to connect to the Teamy Terminal control service")?;
        let response = match action {
            ControlAction::GetContent => client
                .get_content()
                .await
                .map(|text| ControlResponse::Content { text }),
            ControlAction::CopySelection => client
                .copy_selection()
                .await
                .map(|text| ControlResponse::Content { text }),
            action => {
                let result = match action {
                    ControlAction::Ping => client.ping().await,
                    ControlAction::SendText { text } => client.send_text(text).await,
                    ControlAction::SendKeys { keys } => {
                        client
                            .send_keys(keys.iter().map(control_key_to_wire).collect())
                            .await
                    }
                    ControlAction::Resize { width, height } => client.resize(width, height).await,
                    ControlAction::Screenshot { path } => client.screenshot(path).await,
                    ControlAction::Select {
                        anchor_column,
                        anchor_row,
                        focus_column,
                        focus_row,
                    } => client
                        .select(anchor_column, anchor_row, focus_column, focus_row)
                        .await,
                    ControlAction::Zoom { steps } => client.zoom(steps).await,
                    ControlAction::SetVisible { visible } => client.set_visible(visible).await,
                    ControlAction::Close => client.close().await,
                    ControlAction::GetContent | ControlAction::CopySelection => {
                        unreachable!("content actions were handled above")
                    }
                };
                result.map(|()| ControlResponse::Ok)
            }
        };
        response
            .map_err(|error| eyre::eyre!("target Teamy Terminal rejected the request: {error}"))
    })
}

/// Server handle owned by the interactive application.
#[derive(Debug)]
pub struct ControlServer {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    registry_path: PathBuf,
    join: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Start the Vox local service and register the owning instance.
    ///
    /// Requests are placed on `calls` and must be completed by the application
    /// thread. The control transport never mutates terminal or window state
    /// directly.
    ///
    /// # Errors
    ///
    /// Returns an error if the instance record cannot be written or the server
    /// worker cannot be started.
    pub fn start(instance: &InstanceInfo, calls: SyncSender<ControlCall>) -> Result<Self> {
        instance_directory()?;
        let registry_path = instance_registry_path(instance.id)?;
        let contents = facet_json::to_string_pretty(instance)
            .wrap_err("failed to serialize Teamy Terminal instance metadata")?;
        std::fs::write(&registry_path, contents)
            .wrap_err("failed to publish Teamy Terminal instance metadata")?;

        let (stop, stop_receiver) = tokio::sync::oneshot::channel();
        let endpoint = instance.endpoint.clone();
        let join = std::thread::Builder::new()
            .name(format!("teamy-terminal-control-{}", instance.id))
            .spawn(move || {
                tracing::info!(%endpoint, "starting Teamy Terminal Vox control service");
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "failed to create Teamy Terminal control runtime");
                        return;
                    }
                };
                tracing::info!("Teamy Terminal control runtime ready");
                runtime.block_on(async move {
                    tracing::info!("Teamy Terminal control server entering Vox accept loop");
                    let service = TerminalControlService { calls };
                    let result = tokio::select! {
                        _ = stop_receiver => Ok(()),
                        result = vox::serve(&endpoint, TerminalControlDispatcher::new(service)).run() =>
                            result.map_err(|error| eyre::eyre!("Teamy Terminal control service stopped: {error}")),
                    };
                    if let Err(error) = result {
                        tracing::error!(%error, "Teamy Terminal control service stopped");
                    }
                });
            })
            .wrap_err("failed to start the Teamy Terminal control server")?;

        Ok(Self {
            stop: Some(stop),
            registry_path,
            join: Some(join),
        })
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = std::fs::remove_file(&self.registry_path);
    }
}

fn instance_registry_path(id: u32) -> Result<PathBuf> {
    Ok(instance_directory()?.join(format!("{id}.json")))
}

fn local_endpoint(id: u32) -> String {
    let logical_path = std::env::temp_dir()
        .join(CONTROL_DIRECTORY_NAME)
        .join(INSTANCE_DIRECTORY_NAME)
        .join(format!("{id}.endpoint"));
    #[cfg(windows)]
    {
        let endpoint = vox::transport::local::path_to_pipe_name(logical_path);
        format!("local://{endpoint}")
    }
    #[cfg(not(windows))]
    {
        format!("local://{}", logical_path.display())
    }
}

fn validate_action(action: &ControlAction) -> Result<()> {
    match action {
        ControlAction::SendText { text } if text.len() > MAX_INPUT_BYTES => {
            eyre::bail!("control text exceeds the {MAX_INPUT_BYTES}-byte limit")
        }
        ControlAction::SendKeys { keys } if keys.len() > MAX_KEY_ACTIONS => {
            eyre::bail!("control key sequence exceeds the {MAX_KEY_ACTIONS}-action limit")
        }
        ControlAction::Zoom { steps } if steps.unsigned_abs() > MAX_ZOOM_STEPS.unsigned_abs() => {
            eyre::bail!("zoom request exceeds the {MAX_ZOOM_STEPS}-step control limit")
        }
        ControlAction::SendKeys { keys } => {
            for key in keys {
                let text = match key {
                    ControlKey::Character { text } | ControlKey::Paste { text } => text,
                    ControlKey::KeyDown { key } | ControlKey::KeyUp { key } => match key {
                        ControlPhysicalKey::Character { text } => {
                            if text.chars().count() != 1 {
                                eyre::bail!(
                                    "physical character key must contain one Unicode scalar"
                                );
                            }
                            text
                        }
                        _ => continue,
                    },
                    ControlKey::Delay { milliseconds } => {
                        if *milliseconds > MAX_KEY_DELAY_MS {
                            eyre::bail!(
                                "key delay exceeds the {MAX_KEY_DELAY_MS}ms control limit"
                            );
                        }
                        continue;
                    }
                    _ => continue,
                };
                if text.len() > MAX_INPUT_BYTES {
                    eyre::bail!("control key text exceeds the {MAX_INPUT_BYTES}-byte limit")
                }
            }
        }
        ControlAction::Screenshot { path } if path.is_empty() => {
            eyre::bail!("control screenshot path cannot be empty")
        }
        _ => {}
    }
    Ok(())
}

fn physical_key_to_wire(key: &ControlPhysicalKey) -> String {
    match key {
        ControlPhysicalKey::Character { text } => format!("character:{text}"),
        ControlPhysicalKey::Ctrl => "ctrl".to_owned(),
        ControlPhysicalKey::Shift => "shift".to_owned(),
        ControlPhysicalKey::Alt => "alt".to_owned(),
        ControlPhysicalKey::Enter => "enter".to_owned(),
        ControlPhysicalKey::Tab => "tab".to_owned(),
        ControlPhysicalKey::Backspace => "backspace".to_owned(),
        ControlPhysicalKey::Escape => "escape".to_owned(),
        ControlPhysicalKey::ArrowUp => "arrow-up".to_owned(),
        ControlPhysicalKey::ArrowDown => "arrow-down".to_owned(),
        ControlPhysicalKey::ArrowRight => "arrow-right".to_owned(),
        ControlPhysicalKey::ArrowLeft => "arrow-left".to_owned(),
        ControlPhysicalKey::Home => "home".to_owned(),
        ControlPhysicalKey::End => "end".to_owned(),
        ControlPhysicalKey::Delete => "delete".to_owned(),
        ControlPhysicalKey::PageUp => "page-up".to_owned(),
        ControlPhysicalKey::PageDown => "page-down".to_owned(),
    }
}

fn physical_key_from_wire(wire: &str) -> Result<ControlPhysicalKey, String> {
    if let Some(text) = wire.strip_prefix("character:") {
        return Ok(ControlPhysicalKey::Character {
            text: text.to_owned(),
        });
    }
    parse_physical_key(wire)
        .ok_or_else(|| format!("unknown Teamy Terminal physical key wire value: {wire}"))
}

fn control_key_to_wire(key: &ControlKey) -> String {
    match key {
        ControlKey::Character { text } => format!("character:{text}"),
        ControlKey::Enter => "enter".to_owned(),
        ControlKey::Tab => "tab".to_owned(),
        ControlKey::Backspace => "backspace".to_owned(),
        ControlKey::Escape => "escape".to_owned(),
        ControlKey::ArrowUp => "arrow-up".to_owned(),
        ControlKey::ArrowDown => "arrow-down".to_owned(),
        ControlKey::ArrowRight => "arrow-right".to_owned(),
        ControlKey::ArrowLeft => "arrow-left".to_owned(),
        ControlKey::Home => "home".to_owned(),
        ControlKey::End => "end".to_owned(),
        ControlKey::Delete => "delete".to_owned(),
        ControlKey::PageUp => "page-up".to_owned(),
        ControlKey::PageDown => "page-down".to_owned(),
        ControlKey::CtrlA => "ctrl-a".to_owned(),
        ControlKey::CtrlArrowUp => "ctrl-arrow-up".to_owned(),
        ControlKey::CtrlArrowDown => "ctrl-arrow-down".to_owned(),
        ControlKey::CtrlArrowRight => "ctrl-arrow-right".to_owned(),
        ControlKey::CtrlArrowLeft => "ctrl-arrow-left".to_owned(),
        ControlKey::CtrlHome => "ctrl-home".to_owned(),
        ControlKey::CtrlEnd => "ctrl-end".to_owned(),
        ControlKey::CtrlDelete => "ctrl-delete".to_owned(),
        ControlKey::CtrlPageUp => "ctrl-page-up".to_owned(),
        ControlKey::CtrlPageDown => "ctrl-page-down".to_owned(),
        ControlKey::CtrlCharacter { character } => format!("ctrl-{character}"),
        ControlKey::CtrlC => "ctrl-c".to_owned(),
        ControlKey::CtrlD => "ctrl-d".to_owned(),
        ControlKey::CtrlZ => "ctrl-z".to_owned(),
        ControlKey::CtrlL => "ctrl-l".to_owned(),
        ControlKey::Paste { text } => format!("paste:{text}"),
        ControlKey::KeyDown { key } => format!("key-down:{}", physical_key_to_wire(key)),
        ControlKey::KeyUp { key } => format!("key-up:{}", physical_key_to_wire(key)),
        ControlKey::Delay { milliseconds } => format!("delay:{milliseconds}"),
    }
}

fn control_key_from_wire(wire: String) -> Result<ControlKey, String> {
    if let Some(text) = wire.strip_prefix("character:") {
        return Ok(ControlKey::Character {
            text: text.to_owned(),
        });
    }
    if let Some(text) = wire.strip_prefix("paste:") {
        return Ok(ControlKey::Paste {
            text: text.to_owned(),
        });
    }
    if let Some(key) = wire.strip_prefix("key-down:") {
        return Ok(ControlKey::KeyDown {
            key: physical_key_from_wire(key)?,
        });
    }
    if let Some(key) = wire.strip_prefix("key-up:") {
        return Ok(ControlKey::KeyUp {
            key: physical_key_from_wire(key)?,
        });
    }
    if let Some(milliseconds) = wire.strip_prefix("delay:") {
        let milliseconds = milliseconds
            .parse::<u16>()
            .map_err(|_| format!("invalid Teamy Terminal key delay wire value: {wire}"))?;
        if milliseconds > MAX_KEY_DELAY_MS {
            return Err(format!(
                "key delay exceeds the {MAX_KEY_DELAY_MS}ms control limit"
            ));
        }
        return Ok(ControlKey::Delay { milliseconds });
    }
    match wire.as_str() {
        "enter" => Ok(ControlKey::Enter),
        "tab" => Ok(ControlKey::Tab),
        "backspace" => Ok(ControlKey::Backspace),
        "escape" => Ok(ControlKey::Escape),
        "arrow-up" => Ok(ControlKey::ArrowUp),
        "arrow-down" => Ok(ControlKey::ArrowDown),
        "arrow-right" => Ok(ControlKey::ArrowRight),
        "arrow-left" => Ok(ControlKey::ArrowLeft),
        "home" => Ok(ControlKey::Home),
        "end" => Ok(ControlKey::End),
        "delete" => Ok(ControlKey::Delete),
        "page-up" => Ok(ControlKey::PageUp),
        "page-down" => Ok(ControlKey::PageDown),
        "ctrl-a" => Ok(ControlKey::CtrlA),
        "ctrl-arrow-up" => Ok(ControlKey::CtrlArrowUp),
        "ctrl-arrow-down" => Ok(ControlKey::CtrlArrowDown),
        "ctrl-arrow-right" => Ok(ControlKey::CtrlArrowRight),
        "ctrl-arrow-left" => Ok(ControlKey::CtrlArrowLeft),
        "ctrl-home" => Ok(ControlKey::CtrlHome),
        "ctrl-end" => Ok(ControlKey::CtrlEnd),
        "ctrl-delete" => Ok(ControlKey::CtrlDelete),
        "ctrl-page-up" => Ok(ControlKey::CtrlPageUp),
        "ctrl-page-down" => Ok(ControlKey::CtrlPageDown),
        "ctrl-c" => Ok(ControlKey::CtrlC),
        "ctrl-d" => Ok(ControlKey::CtrlD),
        "ctrl-z" => Ok(ControlKey::CtrlZ),
        "ctrl-l" => Ok(ControlKey::CtrlL),
        _ => {
            let character = wire
                .strip_prefix("ctrl-")
                .ok_or_else(|| format!("unknown Teamy Terminal control key wire value: {wire}"))?;
            let mut characters = character.chars();
            let character = characters
                .next()
                .ok_or_else(|| format!("unknown Teamy Terminal control key wire value: {wire}"))?;
            if characters.next().is_some()
                || !(character.is_ascii_lowercase()
                    || matches!(character, '@' | ' ' | '[' | '\\' | ']' | '^' | '_' | '?'))
            {
                return Err(format!(
                    "unknown Teamy Terminal control key wire value: {wire}"
                ));
            }
            Ok(ControlKey::CtrlCharacter { character })
        }
    }
}

#[vox::service]
pub trait TerminalControl {
    async fn ping(&self) -> Result<(), String>;
    async fn send_text(&self, text: String) -> Result<(), String>;
    async fn send_keys(&self, keys: Vec<String>) -> Result<(), String>;
    async fn resize(&self, width: u32, height: u32) -> Result<(), String>;
    async fn screenshot(&self, path: String) -> Result<(), String>;
    async fn get_content(&self) -> Result<String, String>;
    async fn select(
        &self,
        anchor_column: u16,
        anchor_row: u16,
        focus_column: u16,
        focus_row: u16,
    ) -> Result<(), String>;
    async fn copy_selection(&self) -> Result<String, String>;
    async fn zoom(&self, steps: i16) -> Result<(), String>;
    async fn set_visible(&self, visible: bool) -> Result<(), String>;
    async fn close(&self) -> Result<(), String>;
}

#[derive(Clone)]
struct TerminalControlService {
    calls: SyncSender<ControlCall>,
}

impl TerminalControlService {
    async fn dispatch(&self, operation: ControlAction) -> Result<ControlResponse, String> {
        tracing::debug!("Teamy Terminal Vox request reached the generated service dispatcher");
        let (responder, response) = tokio::sync::oneshot::channel();
        self.calls
            .send(ControlCall {
                operation,
                responder,
            })
            .map_err(|_| String::from("Teamy Terminal application stopped accepting requests"))?;
        response
            .await
            .map_err(|_| String::from("Teamy Terminal application dropped the response"))?
    }
}

impl TerminalControl for TerminalControlService {
    async fn ping(&self) -> Result<(), String> {
        self.dispatch(ControlAction::Ping).await.map(|_| ())
    }

    async fn send_text(&self, text: String) -> Result<(), String> {
        self.dispatch(ControlAction::SendText { text }).await.map(|_| ())
    }

    async fn send_keys(&self, keys: Vec<String>) -> Result<(), String> {
        let keys = keys
            .into_iter()
            .map(control_key_from_wire)
            .collect::<Result<Vec<_>, _>>()?;
        self.dispatch(ControlAction::SendKeys { keys }).await.map(|_| ())
    }

    async fn resize(&self, width: u32, height: u32) -> Result<(), String> {
        self.dispatch(ControlAction::Resize { width, height }).await.map(|_| ())
    }

    async fn screenshot(&self, path: String) -> Result<(), String> {
        self.dispatch(ControlAction::Screenshot { path }).await.map(|_| ())
    }

    async fn get_content(&self) -> Result<String, String> {
        match self.dispatch(ControlAction::GetContent).await? {
            ControlResponse::Content { text } => Ok(text),
            ControlResponse::Ok => Err(String::from(
                "Teamy Terminal returned an empty get-content response",
            )),
        }
    }

    async fn select(
        &self,
        anchor_column: u16,
        anchor_row: u16,
        focus_column: u16,
        focus_row: u16,
    ) -> Result<(), String> {
        self.dispatch(ControlAction::Select {
            anchor_column,
            anchor_row,
            focus_column,
            focus_row,
        })
        .await
        .map(|_| ())
    }

    async fn copy_selection(&self) -> Result<String, String> {
        match self.dispatch(ControlAction::CopySelection).await? {
            ControlResponse::Content { text } => Ok(text),
            ControlResponse::Ok => Err(String::from(
                "Teamy Terminal returned an empty copy-selection response",
            )),
        }
    }

    async fn zoom(&self, steps: i16) -> Result<(), String> {
        self.dispatch(ControlAction::Zoom { steps }).await.map(|_| ())
    }

    async fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.dispatch(ControlAction::SetVisible { visible })
            .await
            .map(|_| ())
    }

    async fn close(&self) -> Result<(), String> {
        self.dispatch(ControlAction::Close).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CONTROL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn request_limits_reject_unbounded_text() {
        let action = ControlAction::SendText {
            text: "x".repeat(MAX_INPUT_BYTES + 1),
        };
        assert!(validate_action(&action).is_err());
    }

    #[test]
    fn zoom_requests_are_bounded() {
        assert!(validate_action(&ControlAction::Zoom {
            steps: MAX_ZOOM_STEPS,
        })
        .is_ok());
        assert!(validate_action(&ControlAction::Zoom {
            steps: -MAX_ZOOM_STEPS,
        })
        .is_ok());
        assert!(validate_action(&ControlAction::Zoom {
            steps: MAX_ZOOM_STEPS + 1,
        })
        .is_err());
        assert!(validate_action(&ControlAction::Zoom {
            steps: -(MAX_ZOOM_STEPS + 1),
        })
        .is_err());
    }

    #[test]
    fn current_instance_uses_process_id_for_discovery() {
        let instance = InstanceInfo::current("test");
        assert_eq!(instance.id, std::process::id());
        assert_eq!(instance.pid, std::process::id());
        // Vox maps the logical endpoint to a generated Windows named pipe, so
        // the transport endpoint does not necessarily retain the PID text.
        assert!(instance.endpoint.starts_with("local://"));
        assert!(instance.endpoint.len() > "local://".len());
    }

    #[test]
    fn key_sequence_parser_requires_explicit_actions() {
        let ctrl_c = vec![
            ControlKey::KeyDown {
                key: ControlPhysicalKey::Ctrl,
            },
            ControlKey::Delay { milliseconds: 10 },
            ControlKey::KeyDown {
                key: ControlPhysicalKey::Character {
                    text: "c".to_owned(),
                },
            },
            ControlKey::Delay { milliseconds: 10 },
            ControlKey::KeyUp {
                key: ControlPhysicalKey::Character {
                    text: "c".to_owned(),
                },
            },
            ControlKey::Delay { milliseconds: 10 },
            ControlKey::KeyUp {
                key: ControlPhysicalKey::Ctrl,
            },
        ];
        assert_eq!(
            parse_key_sequence("down:ctrl wait:10 down:c wait:10 up:c wait:10 up:ctrl")
                .expect("explicit Ctrl+C should parse"),
            ctrl_c
        );
        assert_eq!(
            parse_key_sequence("down:1 wait:1 up:1")
                .expect("explicit numeric key should parse"),
            vec![
                ControlKey::KeyDown {
                    key: ControlPhysicalKey::Character {
                        text: "1".to_owned(),
                    },
                },
                ControlKey::Delay { milliseconds: 1 },
                ControlKey::KeyUp {
                    key: ControlPhysicalKey::Character {
                        text: "1".to_owned(),
                    },
                },
            ]
        );
        assert_eq!(
            parse_key_sequence("down:a up:a").expect("adjacent actions should parse"),
            vec![
                ControlKey::KeyDown {
                    key: ControlPhysicalKey::Character {
                        text: "a".to_owned(),
                    },
                },
                ControlKey::KeyUp {
                    key: ControlPhysicalKey::Character {
                        text: "a".to_owned(),
                    },
                },
            ]
        );
        assert_eq!(
            control_key_to_wire(&ControlKey::CtrlCharacter { character: 'b' }),
            "ctrl-b"
        );
        assert_eq!(
            control_key_from_wire("ctrl-b".to_owned())
                .expect("Ctrl+B wire value should parse"),
            ControlKey::CtrlCharacter { character: 'b' }
        );

        for invalid in [
            "123",
            "ctrl+c",
            "+1 10 -1",
            "down:c 10 up:c",
            "enter",
            "DOWN:c",
            "down:C WAIT:1 up:C",
        ] {
            assert!(
                parse_key_sequence(invalid).is_err(),
                "implicit syntax should be rejected: {invalid}"
            );
        }
        assert!(parse_key_sequence("").is_err());
        assert!(parse_key_sequence("wait:5001").is_err());
        assert!(parse_key_sequence("hold:c").is_err());
        assert!(parse_key_sequence("down:").is_err());
        assert!(parse_key_sequence("wait:one").is_err());
    }

    #[test]
    fn wire_format_keeps_numeric_key_actions_distinct_from_delays() {
        let numeric_key_down = ControlKey::KeyDown {
            key: ControlPhysicalKey::Character {
                text: "1".to_owned(),
            },
        };
        assert_eq!(
            control_key_to_wire(&numeric_key_down),
            "key-down:character:1"
        );
        assert_eq!(
            control_key_from_wire("key-down:character:1".to_owned())
                .expect("numeric key wire value should parse"),
            numeric_key_down
        );
        assert_eq!(
            control_key_from_wire("delay:1".to_owned())
                .expect("delay wire value should parse"),
            ControlKey::Delay { milliseconds: 1 }
        );
    }

    #[test]
    fn physical_navigation_keys_round_trip_through_wire_names() {
        for key in [
            ControlPhysicalKey::ArrowUp,
            ControlPhysicalKey::ArrowDown,
            ControlPhysicalKey::ArrowLeft,
            ControlPhysicalKey::ArrowRight,
            ControlPhysicalKey::PageUp,
            ControlPhysicalKey::PageDown,
        ] {
            let wire = physical_key_to_wire(&key);
            assert_eq!(
                physical_key_from_wire(&wire).expect("wire navigation key should parse"),
                key
            );
        }
    }

    #[test]
    fn vox_control_round_trip_reaches_application_channel() {
        let _guard = CONTROL_TEST_LOCK
            .lock()
            .expect("control test lock should not be poisoned");
        let instance = InstanceInfo::current("control-test");
        let (calls, receiver) = std::sync::mpsc::sync_channel(1);
        let server = ControlServer::start(&instance, calls).expect("control server should start");
        let client_instance = instance.clone();
        let client = std::thread::spawn(move || invoke(&client_instance, ControlAction::Ping));
        let call = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("Vox should deliver a request to the application channel");
        assert_eq!(call.operation, ControlAction::Ping);
        call.responder
            .send(Ok(ControlResponse::Ok))
            .expect("Vox request should still be awaiting a response");
        assert_eq!(
            client.join().expect("client thread should join").unwrap(),
            ControlResponse::Ok
        );
        drop(server);
    }

    #[test]
    fn vox_get_content_round_trip_returns_visible_text() {
        let _guard = CONTROL_TEST_LOCK
            .lock()
            .expect("control test lock should not be poisoned");
        let instance = InstanceInfo::current("content-test");
        let (calls, receiver) = std::sync::mpsc::sync_channel(1);
        let server = ControlServer::start(&instance, calls).expect("control server should start");
        let client_instance = instance.clone();
        let client =
            std::thread::spawn(move || invoke(&client_instance, ControlAction::GetContent));
        let call = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("Vox should deliver the get-content request");
        assert_eq!(call.operation, ControlAction::GetContent);
        call.responder
            .send(Ok(ControlResponse::Content {
                text: String::from("visible text"),
            }))
            .expect("get-content request should still be awaiting a response");
        assert_eq!(
            client.join().expect("client thread should join").unwrap(),
            ControlResponse::Content {
                text: String::from("visible text")
            }
        );
        drop(server);
    }
}
