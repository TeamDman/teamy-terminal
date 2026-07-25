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
    CtrlC,
    CtrlD,
    CtrlZ,
    CtrlL,
    /// Paste text without requiring the target process to access a clipboard.
    Paste {
        text: String,
    },
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
    /// Close the terminal window and its shell.
    Close,
}

/// The response returned by the control client and exposed by CLI output.
#[derive(Clone, Debug, Facet, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlResponse {
    /// The request completed successfully.
    Ok,
}

/// A request waiting to be applied by the owning winit application thread.
#[derive(Debug)]
pub struct ControlCall {
    /// The decoded operation.
    pub operation: ControlAction,
    /// One-shot response channel back to the Vox service task.
    pub responder: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// Parse the compact key syntax used by `teamy-terminal invoke ... send-keys`.
///
/// A recognized chord such as `ctrl+c` becomes one control action. Any other
/// string is treated as ordinary Unicode text, so `echo hi` does what a user
/// naturally expects.
///
/// # Errors
///
/// Returns an error when the sequence exceeds the control-plane action bound.
pub fn parse_key_sequence(spec: &str) -> Result<Vec<ControlKey>> {
    if let Some(key) = parse_named_key(spec.trim()) {
        return Ok(vec![key]);
    }
    let keys: Vec<_> = spec
        .chars()
        .map(|character| ControlKey::Character {
            text: character.to_string(),
        })
        .collect();
    if keys.len() > MAX_KEY_ACTIONS {
        eyre::bail!("control key sequence exceeds the {MAX_KEY_ACTIONS}-action limit")
    }
    Ok(keys)
}

fn parse_named_key(spec: &str) -> Option<ControlKey> {
    Some(match spec.to_ascii_lowercase().as_str() {
        "enter" => ControlKey::Enter,
        "tab" => ControlKey::Tab,
        "backspace" => ControlKey::Backspace,
        "escape" | "esc" => ControlKey::Escape,
        "up" | "arrowup" => ControlKey::ArrowUp,
        "down" | "arrowdown" => ControlKey::ArrowDown,
        "right" | "arrowright" => ControlKey::ArrowRight,
        "left" | "arrowleft" => ControlKey::ArrowLeft,
        "home" => ControlKey::Home,
        "end" => ControlKey::End,
        "delete" | "del" => ControlKey::Delete,
        "pageup" => ControlKey::PageUp,
        "pagedown" => ControlKey::PageDown,
        "ctrl+c" => ControlKey::CtrlC,
        "ctrl+d" => ControlKey::CtrlD,
        "ctrl+z" => ControlKey::CtrlZ,
        "ctrl+l" => ControlKey::CtrlL,
        _ => return None,
    })
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
            ControlAction::Close => client.close().await,
        };
        result
            .map(|()| ControlResponse::Ok)
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
        ControlAction::SendKeys { keys } => {
            for key in keys {
                let text = match key {
                    ControlKey::Character { text } | ControlKey::Paste { text } => text,
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
        ControlKey::CtrlC => "ctrl-c".to_owned(),
        ControlKey::CtrlD => "ctrl-d".to_owned(),
        ControlKey::CtrlZ => "ctrl-z".to_owned(),
        ControlKey::CtrlL => "ctrl-l".to_owned(),
        ControlKey::Paste { text } => format!("paste:{text}"),
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
        "ctrl-c" => Ok(ControlKey::CtrlC),
        "ctrl-d" => Ok(ControlKey::CtrlD),
        "ctrl-z" => Ok(ControlKey::CtrlZ),
        "ctrl-l" => Ok(ControlKey::CtrlL),
        _ => Err(format!(
            "unknown Teamy Terminal control key wire value: {wire}"
        )),
    }
}

#[vox::service]
pub trait TerminalControl {
    async fn ping(&self) -> Result<(), String>;
    async fn send_text(&self, text: String) -> Result<(), String>;
    async fn send_keys(&self, keys: Vec<String>) -> Result<(), String>;
    async fn resize(&self, width: u32, height: u32) -> Result<(), String>;
    async fn screenshot(&self, path: String) -> Result<(), String>;
    async fn close(&self) -> Result<(), String>;
}

#[derive(Clone)]
struct TerminalControlService {
    calls: SyncSender<ControlCall>,
}

impl TerminalControlService {
    async fn dispatch(&self, operation: ControlAction) -> Result<(), String> {
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
        self.dispatch(ControlAction::Ping).await
    }

    async fn send_text(&self, text: String) -> Result<(), String> {
        self.dispatch(ControlAction::SendText { text }).await
    }

    async fn send_keys(&self, keys: Vec<String>) -> Result<(), String> {
        let keys = keys
            .into_iter()
            .map(control_key_from_wire)
            .collect::<Result<Vec<_>, _>>()?;
        self.dispatch(ControlAction::SendKeys { keys }).await
    }

    async fn resize(&self, width: u32, height: u32) -> Result<(), String> {
        self.dispatch(ControlAction::Resize { width, height }).await
    }

    async fn screenshot(&self, path: String) -> Result<(), String> {
        self.dispatch(ControlAction::Screenshot { path }).await
    }

    async fn close(&self) -> Result<(), String> {
        self.dispatch(ControlAction::Close).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_limits_reject_unbounded_text() {
        let action = ControlAction::SendText {
            text: "x".repeat(MAX_INPUT_BYTES + 1),
        };
        assert!(validate_action(&action).is_err());
    }

    #[test]
    fn current_instance_uses_process_id_for_discovery() {
        let instance = InstanceInfo::current("test");
        assert_eq!(instance.id, std::process::id());
        assert_eq!(instance.pid, std::process::id());
        assert!(instance.endpoint.contains(&instance.id.to_string()));
    }

    #[test]
    fn key_sequence_parser_distinguishes_text_and_chords() {
        assert_eq!(
            parse_key_sequence("echo hi").expect("text should parse"),
            vec![
                ControlKey::Character {
                    text: "e".to_owned()
                },
                ControlKey::Character {
                    text: "c".to_owned()
                },
                ControlKey::Character {
                    text: "h".to_owned()
                },
                ControlKey::Character {
                    text: "o".to_owned()
                },
                ControlKey::Character {
                    text: " ".to_owned()
                },
                ControlKey::Character {
                    text: "h".to_owned()
                },
                ControlKey::Character {
                    text: "i".to_owned()
                },
            ]
        );
        assert_eq!(
            parse_key_sequence("ctrl+c").expect("control chord should parse"),
            vec![ControlKey::CtrlC]
        );
        assert_eq!(
            parse_key_sequence("ENTER").expect("named key should parse"),
            vec![ControlKey::Enter]
        );
    }

    #[test]
    fn vox_control_round_trip_reaches_application_channel() {
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
            .send(Ok(()))
            .expect("Vox request should still be awaiting a response");
        assert_eq!(
            client.join().expect("client thread should join").unwrap(),
            ControlResponse::Ok
        );
        drop(server);
    }
}
