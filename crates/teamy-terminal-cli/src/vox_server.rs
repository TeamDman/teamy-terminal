//! Rust-authoritative Vox terminal endpoint.
//!
//! The endpoint owns the PowerShell PTY, VT parsing, and negotiated Rust-side
//! rasterization. CPU Fontdue/full-PNG remains the compatibility default, while
//! CPU Fontdue and no-window GPU Slug can each publish full PNG, full raw RGBA,
//! or packed dirty raw RGBA through the generated Vox contract.

use eyre::{Context, Result};
use facet::Facet;
use spec_proto::terminal::{
    Terminal, TerminalAlphaMode, TerminalCancelRequest, TerminalCapabilities,
    TerminalCapabilitiesRequest, TerminalCapabilitiesResult, TerminalColorSpace,
    TerminalConnectRequest, TerminalConnectResult, TerminalContentRequest, TerminalContentResult,
    TerminalCursor, TerminalDisconnectRequest, TerminalDispatcher, TerminalError,
    TerminalErrorCode, TerminalFrameEncoding, TerminalFrameEvent, TerminalFrameKind,
    TerminalFrameOrigin, TerminalInputResult, TerminalKeyInput, TerminalMouseInput,
    TerminalOperationResult, TerminalPresentationCapabilitiesRequest,
    TerminalPresentationCapabilitiesResult, TerminalPresentationMode,
    TerminalPresentationUnavailable, TerminalPromptMetadata, TerminalPublicationTelemetry,
    TerminalRange, TerminalRasterFrame, TerminalRasterFrameEvent, TerminalRasterFrameKind,
    TerminalRasterRegion, TerminalRasterSnapshotTiming, TerminalRasterSubscribeRequest,
    TerminalRasterizationOwner, TerminalResizeRequest, TerminalResizeResult, TerminalSelection,
    TerminalSnapshot, TerminalSnapshotRequest, TerminalSnapshotTiming, TerminalState,
    TerminalSubscribeRequest, TerminalSurfaceMetrics, TerminalTextInput,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use teamy_terminal_control::ControlPhysicalKey;
use teamy_terminal_core::{TerminalLimits, TerminalSession};
use teamy_terminal_font::{
    DEFAULT_PIXEL_SIZE, EMBEDDED_FONT_ID, EMBEDDED_FONT_SHA256, RasterizedFrame, TerminalFont,
    TerminalFontCatalog, damage_regions,
};
use teamy_terminal_frame::{
    FrameCompression, MAX_DIRTY_REGIONS, PackedRasterRegion, RASTER_FRAME_CONTRACT_VERSION,
    RasterFrameUpdate, RasterRect, RasterUpdateKind,
};
use teamy_terminal_pty::{PtyConfig, PtyEvent, PtySession, PtySessionControl};
use teamy_terminal_renderer::{
    GpuSlugCacheStats, GpuSlugRenderIdentity, GpuSlugRenderRequest, GpuSlugRenderStats,
    GpuSlugRenderer, GpuSlugRendererError, GpuSlugRuntime, VulkanOffscreenCancellation,
    VulkanOffscreenContext, VulkanOffscreenError,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use vox_terminal as vox;

const MAX_COLUMNS: u16 = 240;
const MAX_ROWS: u16 = 120;
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
const MAX_CONTENT_CHARS: u32 = 256 * 1024;
const MAX_PIXEL_WIDTH: u16 = 4096;
const MAX_PIXEL_HEIGHT: u16 = 4096;
// The Vox bridge must preserve the requested logical grid even when a panel is
// slightly shorter than the minimum interactive-window font would allow.  A
// bridge renderer therefore has a lower fitting floor than the user-facing
// standalone zoom control; impossible sub-cell targets are still rejected.
const MIN_FONT_PIXEL_SIZE: f32 = 1.0;
const MAX_FONT_PIXEL_SIZE: f32 = 64.0;
const MAX_CACHED_FONT_SIZES: usize = 4;
const SESSION_ACTOR_CAPACITY: usize = 64;
const MAX_CONCURRENT_RASTER_PRESENTATIONS_PER_SESSION: usize = 2;
const RASTER_TRANSPORT_VERSION: u16 = 1;
const CPU_RENDERER_ID: &str = "rust-cpu-fontdue";
const GPU_RENDERER_ID: &str = "rust-gpu-slug";
const DAMAGE_FULL: &str = "full";
const DAMAGE_DIRTY: &str = "dirty";
const TRANSPORT_FULL_PNG: &str = "full-png";
const TRANSPORT_FULL_RAW_RGBA: &str = "full-raw-rgba";
const TRANSPORT_DIRTY_RAW_RGBA: &str = "dirty-raw-rgba";
pub const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:63946";

static PROCESS_EPOCH: OnceLock<String> = OnceLock::new();
static PROCESS_GPU_REGISTRY: OnceLock<Arc<GpuRendererRegistry>> = OnceLock::new();
static NEXT_CONNECTION_EPOCH: AtomicU64 = AtomicU64::new(1);
static NEXT_SESSION_EPOCH: AtomicU64 = AtomicU64::new(1);

const GLFW_MOD_SHIFT: u32 = 0x0001;
const GLFW_MOD_CONTROL: u32 = 0x0002;
const GLFW_MOD_ALT: u32 = 0x0004;
const GLFW_MOD_SUPER: u32 = 0x0008;
const GLFW_KEY_ESCAPE: u32 = 256;
const GLFW_KEY_ENTER: u32 = 257;
const GLFW_KEY_TAB: u32 = 258;
const GLFW_KEY_BACKSPACE: u32 = 259;
const GLFW_KEY_INSERT: u32 = 260;
const GLFW_KEY_DELETE: u32 = 261;
const GLFW_KEY_RIGHT: u32 = 262;
const GLFW_KEY_LEFT: u32 = 263;
const GLFW_KEY_DOWN: u32 = 264;
const GLFW_KEY_UP: u32 = 265;
const GLFW_KEY_PAGE_UP: u32 = 266;
const GLFW_KEY_PAGE_DOWN: u32 = 267;
const GLFW_KEY_HOME: u32 = 268;
const GLFW_KEY_END: u32 = 269;

fn modified_navigation_bytes(key_code: u32, modifiers: u32) -> Option<Vec<u8>> {
    let has_modifier =
        modifiers & (GLFW_MOD_SHIFT | GLFW_MOD_CONTROL | GLFW_MOD_ALT | GLFW_MOD_SUPER) != 0;
    if !has_modifier {
        return None;
    }
    let modifier = 1
        + u32::from(modifiers & GLFW_MOD_SHIFT != 0)
        + 2 * u32::from(modifiers & GLFW_MOD_ALT != 0)
        + 4 * u32::from(modifiers & GLFW_MOD_CONTROL != 0)
        + 8 * u32::from(modifiers & GLFW_MOD_SUPER != 0);
    if key_code == GLFW_KEY_TAB
        && modifiers & GLFW_MOD_SHIFT != 0
        && modifiers & (GLFW_MOD_CONTROL | GLFW_MOD_ALT | GLFW_MOD_SUPER) == 0
    {
        return Some(b"\x1b[Z".to_vec());
    }
    let final_byte = match key_code {
        GLFW_KEY_UP => Some(b'A'),
        GLFW_KEY_DOWN => Some(b'B'),
        GLFW_KEY_RIGHT => Some(b'C'),
        GLFW_KEY_LEFT => Some(b'D'),
        GLFW_KEY_HOME => Some(b'H'),
        GLFW_KEY_END => Some(b'F'),
        _ => None,
    }?;
    Some(format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes())
}

fn control_byte(key_code: u32) -> Option<u8> {
    if (b'A' as u32..=b'Z' as u32).contains(&key_code) {
        return u8::try_from(key_code - u32::from(b'A') + 1).ok();
    }
    match key_code {
        32 | 64 => Some(0),
        91 => Some(0x1b),
        92 => Some(0x1c),
        93 => Some(0x1d),
        94 => Some(0x1e),
        95 => Some(0x1f),
        _ => None,
    }
}

fn key_input_bytes(key_code: u32, modifiers: u32, pressed: bool) -> Vec<u8> {
    if modifiers & GLFW_MOD_SUPER == 0 {
        if let Some(key) = control_physical_key(key_code, modifiers) {
            let mut pressed_keys = Vec::new();
            if modifiers & GLFW_MOD_CONTROL != 0 {
                pressed_keys.push(ControlPhysicalKey::Ctrl);
            }
            if modifiers & GLFW_MOD_SHIFT != 0 {
                pressed_keys.push(ControlPhysicalKey::Shift);
            }
            if modifiers & GLFW_MOD_ALT != 0 {
                pressed_keys.push(ControlPhysicalKey::Alt);
            }
            return crate::interactive::win32_physical_key_event_bytes(
                &key,
                pressed,
                &pressed_keys,
            )
            .expect("GLFW key should encode as a Win32 physical event");
        }
    }
    if !pressed {
        return Vec::new();
    }
    if modifiers & GLFW_MOD_CONTROL != 0 {
        if key_code == GLFW_KEY_BACKSPACE {
            return vec![0x17];
        }
        if let Some(byte) = control_byte(key_code) {
            return vec![byte];
        }
    }
    if let Some(sequence) = modified_navigation_bytes(key_code, modifiers) {
        return sequence;
    }
    match key_code {
        GLFW_KEY_ENTER => b"\r".to_vec(),
        GLFW_KEY_TAB => b"\t".to_vec(),
        GLFW_KEY_BACKSPACE => vec![0x7f],
        GLFW_KEY_ESCAPE => vec![0x1b],
        GLFW_KEY_UP => b"\x1b[A".to_vec(),
        GLFW_KEY_DOWN => b"\x1b[B".to_vec(),
        GLFW_KEY_RIGHT => b"\x1b[C".to_vec(),
        GLFW_KEY_LEFT => b"\x1b[D".to_vec(),
        GLFW_KEY_HOME => b"\x1b[H".to_vec(),
        GLFW_KEY_END => b"\x1b[F".to_vec(),
        GLFW_KEY_DELETE => b"\x1b[3~".to_vec(),
        GLFW_KEY_INSERT => b"\x1b[2~".to_vec(),
        GLFW_KEY_PAGE_UP => b"\x1b[5~".to_vec(),
        GLFW_KEY_PAGE_DOWN => b"\x1b[6~".to_vec(),
        32 => vec![b' '],
        48..=57 | 65..=90 => {
            let mut byte = u8::try_from(key_code).expect("GLFW ASCII key code fits in u8");
            if (65..=90).contains(&key_code) && modifiers & GLFW_MOD_SHIFT == 0 {
                byte = byte.to_ascii_lowercase();
            }
            vec![byte]
        }
        _ => Vec::new(),
    }
}

fn control_physical_key(key_code: u32, modifiers: u32) -> Option<ControlPhysicalKey> {
    let named = match key_code {
        GLFW_KEY_ENTER => Some(ControlPhysicalKey::Enter),
        GLFW_KEY_TAB => Some(ControlPhysicalKey::Tab),
        GLFW_KEY_BACKSPACE => Some(ControlPhysicalKey::Backspace),
        GLFW_KEY_ESCAPE => Some(ControlPhysicalKey::Escape),
        GLFW_KEY_UP => Some(ControlPhysicalKey::ArrowUp),
        GLFW_KEY_DOWN => Some(ControlPhysicalKey::ArrowDown),
        GLFW_KEY_RIGHT => Some(ControlPhysicalKey::ArrowRight),
        GLFW_KEY_LEFT => Some(ControlPhysicalKey::ArrowLeft),
        GLFW_KEY_HOME => Some(ControlPhysicalKey::Home),
        GLFW_KEY_END => Some(ControlPhysicalKey::End),
        GLFW_KEY_DELETE => Some(ControlPhysicalKey::Delete),
        GLFW_KEY_PAGE_UP => Some(ControlPhysicalKey::PageUp),
        GLFW_KEY_PAGE_DOWN => Some(ControlPhysicalKey::PageDown),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let character = match key_code {
        32 => Some(' '),
        48..=57 | 65..=90 => char::from_u32(key_code).map(|character| {
            if modifiers & GLFW_MOD_SHIFT != 0 {
                character.to_ascii_uppercase()
            } else {
                character.to_ascii_lowercase()
            }
        }),
        _ => None,
    }?;
    Some(ControlPhysicalKey::Character {
        text: character.to_string(),
    })
}

fn mouse_input_bytes(
    terminal: &TerminalSession,
    x: u16,
    y: u16,
    buttons: u8,
    button: u8,
    pressed: bool,
    motion: bool,
    wheel_x: i32,
    wheel_y: i32,
) -> Vec<u8> {
    if !terminal.mouse_reporting_enabled() {
        return Vec::new();
    }
    let motion_button = if motion {
        buttons.trailing_zeros().min(2) as u16
    } else {
        u16::from(button.min(2))
    };
    let mut code = if wheel_y > 0 {
        64
    } else if wheel_y < 0 {
        65
    } else if wheel_x > 0 {
        66
    } else if wheel_x < 0 {
        67
    } else if motion {
        32 + motion_button
    } else {
        motion_button
    };
    if !pressed && wheel_x == 0 && wheel_y == 0 {
        code = if buttons == 0 { 3 } else { code };
    }
    let x = u32::from(x).saturating_add(1);
    let y = u32::from(y).saturating_add(1);
    if terminal.sgr_mouse_mode_enabled() {
        let suffix = if pressed || wheel_x != 0 || wheel_y != 0 {
            'M'
        } else {
            'm'
        };
        return format!("\x1b[<{code};{x};{y}{suffix}").into_bytes();
    }
    let legacy_code = u8::try_from(code.min(223) + 32).expect("legacy mouse code fits in u8");
    let legacy_x = u8::try_from(x.min(223) + 32).expect("legacy mouse x fits in u8");
    let legacy_y = u8::try_from(y.min(223) + 32).expect("legacy mouse y fits in u8");
    vec![0x1b, b'[', b'M', legacy_code, legacy_x, legacy_y]
}

/// Normalize the public `serve` address syntax to a TCP bind address.
pub fn normalize_listen_address(raw: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        eyre::bail!("listen address must not be empty");
    }
    if let Some(port) = value.strip_prefix(':') {
        if port.is_empty() {
            eyre::bail!("listen address is missing its port: {value}");
        }
        return Ok(format!("127.0.0.1:{port}"));
    }
    if value.chars().all(|character| character.is_ascii_digit()) {
        return Ok(format!("127.0.0.1:{value}"));
    }
    if value.parse::<std::net::SocketAddr>().is_err() && !value.contains(':') {
        eyre::bail!("listen address must be HOST:PORT or :PORT: {value}");
    }
    Ok(value.to_string())
}

#[derive(Clone)]
pub struct VoxTerminalService {
    sessions: Arc<Mutex<BTreeMap<String, Arc<TerminalSessionRuntime>>>>,
    process_epoch: Arc<str>,
    connection_epoch: Arc<str>,
    gpu_registry: Arc<GpuRendererRegistry>,
    latest_snapshot_timing: Arc<Mutex<Option<TerminalSnapshotTimingWitness>>>,
}

#[derive(Clone, Debug)]
struct CachedGpuError {
    code: TerminalErrorCode,
    message: String,
    retryable: bool,
}

enum GpuRendererState {
    Available(GpuSlugRuntime),
    Unavailable(CachedGpuError),
}

#[derive(Clone, Debug)]
enum GpuCapabilitySnapshot {
    Available,
    Unavailable(CachedGpuError),
}

struct GpuRendererRegistry {
    state: Mutex<GpuRendererState>,
}

impl GpuRendererRegistry {
    fn initialize() -> Self {
        let state = match VulkanOffscreenContext::new() {
            Ok(context) => match GpuSlugRuntime::new(context) {
                Ok(runtime) => {
                    tracing::info!(
                        renderer_id = GPU_RENDERER_ID,
                        device = ?runtime.device_facts(),
                        "cached no-window GPU terminal renderer is available"
                    );
                    GpuRendererState::Available(runtime)
                }
                Err(error) => GpuRendererState::Unavailable(CachedGpuError {
                    code: TerminalErrorCode::UnsupportedCapability,
                    message: format!("{GPU_RENDERER_ID} unavailable: {error}"),
                    retryable: false,
                }),
            },
            Err(error) => GpuRendererState::Unavailable(CachedGpuError {
                code: TerminalErrorCode::UnsupportedCapability,
                message: format!("{GPU_RENDERER_ID} unavailable: {error}"),
                retryable: false,
            }),
        };
        if let GpuRendererState::Unavailable(error) = &state {
            tracing::warn!(
                renderer_id = GPU_RENDERER_ID,
                reason = error.message,
                "cached no-window GPU terminal renderer is unavailable"
            );
        }
        Self {
            state: Mutex::new(state),
        }
    }

    fn runtime(&self, server_sequence: i64) -> Result<GpuSlugRuntime, TerminalError> {
        let state = self.state.lock().map_err(|_| {
            VoxTerminalService::error(
                TerminalErrorCode::Internal,
                "GPU terminal renderer registry lock poisoned",
                server_sequence,
            )
        })?;
        match &*state {
            GpuRendererState::Available(runtime) => Ok(runtime.clone()),
            GpuRendererState::Unavailable(error) => Err(TerminalError {
                code: error.code,
                message: error.message.clone(),
                retryable: error.retryable,
                server_sequence,
            }),
        }
    }

    fn capability_snapshot(
        &self,
        server_sequence: i64,
    ) -> Result<GpuCapabilitySnapshot, TerminalError> {
        let state = self.state.lock().map_err(|_| {
            VoxTerminalService::error(
                TerminalErrorCode::Internal,
                "GPU terminal renderer registry lock poisoned",
                server_sequence,
            )
        })?;
        Ok(match &*state {
            GpuRendererState::Available(_) => GpuCapabilitySnapshot::Available,
            GpuRendererState::Unavailable(error) => {
                GpuCapabilitySnapshot::Unavailable(error.clone())
            }
        })
    }

    fn retire_on_device_loss(
        &self,
        error: &eyre::Report,
        server_sequence: i64,
    ) -> Option<TerminalError> {
        let Some(GpuSlugRendererError::Vulkan(VulkanOffscreenError::DeviceLost { operation })) =
            error.downcast_ref::<GpuSlugRendererError>()
        else {
            return None;
        };
        let cached = CachedGpuError {
            code: TerminalErrorCode::UnsupportedCapability,
            message: format!(
                "{GPU_RENDERER_ID} unavailable after Vulkan device loss during {operation}; restart the server after repairing the GPU/driver"
            ),
            retryable: false,
        };
        if let Ok(mut state) = self.state.lock() {
            *state = GpuRendererState::Unavailable(cached.clone());
        }
        Some(TerminalError {
            code: cached.code,
            message: cached.message,
            retryable: cached.retryable,
            server_sequence,
        })
    }
}

fn process_gpu_registry() -> Arc<GpuRendererRegistry> {
    Arc::clone(PROCESS_GPU_REGISTRY.get_or_init(|| Arc::new(GpuRendererRegistry::initialize())))
}

struct TerminalSessionState {
    pty: PtySessionControl,
    terminal: TerminalSession,
    sequence: i64,
    surface: TerminalSurfaceMetrics,
    closed: bool,
    debug_output: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct PublicationSignal {
    terminal_sequence: i64,
    mutations: i64,
    mutation_at: Instant,
    full_resync: bool,
}

#[derive(Default)]
struct PublicationHubState {
    subscribers: BTreeMap<u64, watch::Sender<PublicationSignal>>,
}

struct PublicationHub {
    state: Mutex<PublicationHubState>,
    next_subscriber: AtomicU64,
    mutations: AtomicI64,
}

impl PublicationHub {
    fn new() -> Self {
        Self {
            state: Mutex::new(PublicationHubState::default()),
            next_subscriber: AtomicU64::new(1),
            mutations: AtomicI64::new(0),
        }
    }

    fn register(&self, terminal_sequence: i64) -> (u64, watch::Receiver<PublicationSignal>) {
        let subscriber_id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        let signal = PublicationSignal {
            terminal_sequence,
            mutations: self.mutations.load(Ordering::Acquire),
            mutation_at: Instant::now(),
            full_resync: true,
        };
        let (sender, receiver) = watch::channel(signal);
        self.state
            .lock()
            .expect("terminal publication hub lock poisoned")
            .subscribers
            .insert(subscriber_id, sender);
        (subscriber_id, receiver)
    }

    fn publish(&self, terminal_sequence: i64, full_resync: bool) {
        let previous = self
            .mutations
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.saturating_add(1))
            })
            .unwrap_or(i64::MAX);
        let mutations = previous.saturating_add(1);
        let signal = PublicationSignal {
            terminal_sequence,
            mutations,
            mutation_at: Instant::now(),
            full_resync,
        };
        let mut state = self
            .state
            .lock()
            .expect("terminal publication hub lock poisoned");
        state.subscribers.retain(|_, subscriber| {
            if subscriber.receiver_count() == 0 {
                return false;
            }
            subscriber.send_replace(signal);
            true
        });
    }

    fn unregister(&self, subscriber_id: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.subscribers.remove(&subscriber_id);
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.subscribers.clear();
        }
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.state.lock().map_or(0, |state| state.subscribers.len())
    }
}

enum SessionActorMessage {
    Pty(PtyEvent),
    Shutdown,
}

struct TerminalSessionRuntime {
    session_id: String,
    session_sequence: u64,
    connection_epoch: Arc<str>,
    session_epoch: Arc<str>,
    gpu_registry: Arc<GpuRendererRegistry>,
    state: Mutex<TerminalSessionState>,
    publication: PublicationHub,
    explicit_render_resources: Mutex<Option<TerminalRenderResources>>,
    raster_render_resources: Mutex<Vec<TerminalRenderResources>>,
    raster_render_cancellations: Mutex<BTreeMap<u64, VulkanOffscreenCancellation>>,
    raster_render_permits: Arc<Semaphore>,
    actor_sender: SyncSender<SessionActorMessage>,
    terminal_sequence: AtomicI64,
    closed: AtomicBool,
}

impl TerminalSessionRuntime {
    fn sequence(&self) -> i64 {
        self.terminal_sequence.load(Ordering::Acquire)
    }

    fn publish_mutation(&self, sequence: i64, full_resync: bool) {
        self.terminal_sequence.store(sequence, Ordering::Release);
        self.publication.publish(sequence, full_resync);
    }

    fn take_explicit_render_resources(&self) -> TerminalRenderResources {
        self.explicit_render_resources
            .lock()
            .ok()
            .and_then(|mut resources| resources.take())
            .unwrap_or_default()
    }

    fn return_explicit_render_resources(&self, resources: TerminalRenderResources) {
        if let Ok(mut slot) = self.explicit_render_resources.lock()
            && slot.is_none()
        {
            *slot = Some(resources);
        }
    }

    fn explicit_render_resource_stats(&self) -> TerminalRenderResourceStats {
        self.explicit_render_resources
            .lock()
            .ok()
            .and_then(|resources| {
                resources
                    .as_ref()
                    .map(TerminalRenderResources::snapshot_stats)
            })
            .unwrap_or_default()
    }

    async fn lease_raster_render_resources(
        self: &Arc<Self>,
    ) -> Result<TerminalRenderResourceLease, TerminalError> {
        let permit = Arc::clone(&self.raster_render_permits)
            .acquire_owned()
            .await
            .map_err(|_| {
                VoxTerminalService::error(
                    TerminalErrorCode::Disconnected,
                    "terminal session closed while waiting for a raster renderer",
                    self.sequence(),
                )
            })?;
        let resources = self
            .raster_render_resources
            .lock()
            .ok()
            .and_then(|mut resources| resources.pop())
            .unwrap_or_else(|| {
                TerminalRenderResources::with_gpu_runtime(
                    self.gpu_registry.runtime(self.sequence()).ok(),
                )
            });
        Ok(TerminalRenderResourceLease {
            runtime: Arc::clone(self),
            resources: Some(resources),
            _permit: permit,
        })
    }

    fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.raster_render_permits.close();
        self.publication.close();
        if let Ok(mut cancellations) = self.raster_render_cancellations.lock() {
            for cancellation in cancellations.values() {
                cancellation.cancel();
            }
            cancellations.clear();
        }
        let _ = self.actor_sender.try_send(SessionActorMessage::Shutdown);
        if let Ok(state) = self.state.lock() {
            let _ = state.pty.kill();
        }
    }
}

struct TerminalRenderResourceLease {
    runtime: Arc<TerminalSessionRuntime>,
    resources: Option<TerminalRenderResources>,
    _permit: OwnedSemaphorePermit,
}

impl TerminalRenderResourceLease {
    fn take(&mut self) -> TerminalRenderResources {
        self.resources
            .take()
            .expect("raster render resource lease must contain resources")
    }

    fn replace(&mut self, resources: TerminalRenderResources) {
        assert!(
            self.resources.replace(resources).is_none(),
            "raster render resource lease must be empty before replacement"
        );
    }
}

impl Drop for TerminalRenderResourceLease {
    fn drop(&mut self) {
        let Some(resources) = self.resources.take() else {
            return;
        };
        if let Ok(mut pool) = self.runtime.raster_render_resources.lock()
            && pool.len() < MAX_CONCURRENT_RASTER_PRESENTATIONS_PER_SESSION
        {
            pool.push(resources);
        }
    }
}

impl Drop for TerminalSessionRuntime {
    fn drop(&mut self) {
        self.publication.close();
        if let Ok(cancellations) = self.raster_render_cancellations.get_mut() {
            for cancellation in cancellations.values() {
                cancellation.cancel();
            }
            cancellations.clear();
        }
        let _ = self.actor_sender.try_send(SessionActorMessage::Shutdown);
    }
}

struct SubscriptionGuard {
    runtime: Weak<TerminalSessionRuntime>,
    subscriber_id: u64,
    cancellation: Option<VulkanOffscreenCancellation>,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.publication.unregister(self.subscriber_id);
            if let Ok(mut cancellations) = runtime.raster_render_cancellations.lock() {
                cancellations.remove(&self.subscriber_id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TerminalRenderResourceStats {
    font_catalog_acquisitions: u64,
    process_font_catalog_loads: u64,
    font_renderer_cache_hits: u64,
    font_renderer_cache_misses: u64,
    font_renderer_constructions: u64,
    font_renderer_evictions: u64,
    font_renderer_cache_len: u64,
    font_renderer_cache_capacity: u64,
    glyph_cache_hits: u64,
    glyph_cache_misses: u64,
    glyph_cache_evictions: u64,
    cached_glyphs: u64,
    glyph_cache_capacity: u64,
    frame_buffer_grows: u64,
    frame_buffer_reuses: u64,
    frame_buffer_capacity_bytes: u64,
    png_buffer_grows: u64,
    png_buffer_reuses: u64,
    png_buffer_capacity_bytes: u64,
    transport_payload_copies: u64,
}

struct CachedTerminalFont {
    pixel_size_bits: u32,
    last_used: u64,
    font: TerminalFont,
}

struct TerminalRenderResources {
    catalog: Option<Arc<TerminalFontCatalog>>,
    fonts: Vec<CachedTerminalFont>,
    use_sequence: u64,
    frame: RasterizedFrame,
    png_scratch: Vec<u8>,
    gpu_runtime: Option<GpuSlugRuntime>,
    gpu_renderer: Option<GpuSlugRenderer>,
    gpu_surface: Option<(u32, u32)>,
    stats: TerminalRenderResourceStats,
}

impl Default for TerminalRenderResources {
    fn default() -> Self {
        Self {
            catalog: None,
            fonts: Vec::with_capacity(MAX_CACHED_FONT_SIZES),
            use_sequence: 0,
            frame: RasterizedFrame {
                width: 0,
                height: 0,
                pixels: Vec::new(),
            },
            png_scratch: Vec::new(),
            gpu_runtime: None,
            gpu_renderer: None,
            gpu_surface: None,
            stats: TerminalRenderResourceStats {
                font_renderer_cache_capacity: MAX_CACHED_FONT_SIZES as u64,
                ..TerminalRenderResourceStats::default()
            },
        }
    }
}

impl TerminalRenderResources {
    fn with_gpu_runtime(gpu_runtime: Option<GpuSlugRuntime>) -> Self {
        Self {
            gpu_runtime,
            ..Self::default()
        }
    }

    fn ensure_gpu_renderer(&mut self) -> Result<&mut GpuSlugRenderer> {
        if self.gpu_renderer.is_none() {
            let runtime = self
                .gpu_runtime
                .as_ref()
                .ok_or_else(|| eyre::eyre!("cached {GPU_RENDERER_ID} runtime is unavailable"))?;
            self.gpu_renderer = Some(runtime.create_renderer()?);
        }
        Ok(self
            .gpu_renderer
            .as_mut()
            .expect("GPU renderer was initialized above"))
    }

    fn ensure_font(&mut self, pixel_size: f32) -> Result<usize> {
        let pixel_size = pixel_size.max(MIN_FONT_PIXEL_SIZE);
        let pixel_size_bits = pixel_size.to_bits();
        self.use_sequence = self.use_sequence.saturating_add(1);
        if let Some(index) = self
            .fonts
            .iter()
            .position(|entry| entry.pixel_size_bits == pixel_size_bits)
        {
            self.fonts[index].last_used = self.use_sequence;
            self.stats.font_renderer_cache_hits =
                self.stats.font_renderer_cache_hits.saturating_add(1);
            return Ok(index);
        }

        self.stats.font_renderer_cache_misses =
            self.stats.font_renderer_cache_misses.saturating_add(1);
        let catalog = if let Some(catalog) = self.catalog.as_ref() {
            Arc::clone(catalog)
        } else {
            let catalog = TerminalFontCatalog::shared_system()?;
            self.catalog = Some(Arc::clone(&catalog));
            self.stats.font_catalog_acquisitions =
                self.stats.font_catalog_acquisitions.saturating_add(1);
            catalog
        };
        let font = TerminalFont::from_catalog(catalog, pixel_size)?;
        self.stats.font_renderer_constructions =
            self.stats.font_renderer_constructions.saturating_add(1);
        if self.fonts.len() == MAX_CACHED_FONT_SIZES {
            let eviction_index = self
                .fonts
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map_or(0, |(index, _)| index);
            self.fonts.swap_remove(eviction_index);
            self.stats.font_renderer_evictions =
                self.stats.font_renderer_evictions.saturating_add(1);
        }
        self.fonts.push(CachedTerminalFont {
            pixel_size_bits,
            last_used: self.use_sequence,
            font,
        });
        Ok(self.fonts.len() - 1)
    }

    fn fit_font(
        &mut self,
        columns: u16,
        rows: u16,
        requested_pixel_width: u16,
        requested_pixel_height: u16,
    ) -> Result<(usize, f32)> {
        let default_font_index = self.ensure_font(DEFAULT_PIXEL_SIZE)?;
        let default_font = &self.fonts[default_font_index].font;
        let default_width = u32::from(columns).saturating_mul(default_font.cell_width());
        let default_height = u32::from(rows).saturating_mul(default_font.cell_height());
        let target_width = if requested_pixel_width == 0 {
            default_width
        } else {
            u32::from(requested_pixel_width)
        };
        let target_height = if requested_pixel_height == 0 {
            default_height
        } else {
            u32::from(requested_pixel_height)
        };
        let width_scale = target_width as f32 / default_width.max(1) as f32;
        let height_scale = target_height as f32 / default_height.max(1) as f32;
        let mut pixel_size = (DEFAULT_PIXEL_SIZE * width_scale.min(height_scale))
            .clamp(MIN_FONT_PIXEL_SIZE, MAX_FONT_PIXEL_SIZE);

        // Cell metrics round upward.  The continuous first estimate can
        // therefore exceed the panel by one row or column.  Re-measure and
        // shrink against the discrete result until the complete grid fits.
        for _ in 0..8 {
            let font_index = self.ensure_font(pixel_size)?;
            let font = &self.fonts[font_index].font;
            let width = u32::from(columns).saturating_mul(font.cell_width());
            let height = u32::from(rows).saturating_mul(font.cell_height());
            if width <= target_width && height <= target_height {
                return Ok((font_index, pixel_size));
            }
            let shrink = (target_width as f32 / width.max(1) as f32)
                .min(target_height as f32 / height.max(1) as f32);
            let next = (pixel_size * shrink * 0.999).max(MIN_FONT_PIXEL_SIZE);
            if next >= pixel_size || (pixel_size - next) < f32::EPSILON {
                break;
            }
            pixel_size = next;
        }

        Err(eyre::eyre!(
            "logical terminal grid {columns}x{rows} cannot fit panel {target_width}x{target_height}"
        ))
    }

    fn snapshot_stats(&self) -> TerminalRenderResourceStats {
        let (
            glyph_cache_hits,
            glyph_cache_misses,
            glyph_cache_evictions,
            cached_glyphs,
            glyph_cache_capacity,
        ) = self
            .fonts
            .iter()
            .fold((0_u64, 0_u64, 0_u64, 0_u64, 0_u64), |total, entry| {
                let stats = entry.font.cache_stats();
                (
                    total.0.saturating_add(stats.hits),
                    total.1.saturating_add(stats.misses),
                    total.2.saturating_add(stats.evictions),
                    total
                        .3
                        .saturating_add(u64::try_from(stats.cached_glyphs).unwrap_or(u64::MAX)),
                    total
                        .4
                        .saturating_add(u64::try_from(stats.capacity).unwrap_or(u64::MAX)),
                )
            });
        TerminalRenderResourceStats {
            process_font_catalog_loads: TerminalFontCatalog::shared_system_loads(),
            font_renderer_cache_len: u64::try_from(self.fonts.len()).unwrap_or(u64::MAX),
            glyph_cache_hits,
            glyph_cache_misses,
            glyph_cache_evictions,
            cached_glyphs,
            glyph_cache_capacity,
            frame_buffer_capacity_bytes: u64::try_from(self.frame.pixels.capacity())
                .unwrap_or(u64::MAX),
            png_buffer_capacity_bytes: u64::try_from(self.png_scratch.capacity())
                .unwrap_or(u64::MAX),
            ..self.stats
        }
    }

    fn encode_png(
        &mut self,
        snapshot: &teamy_terminal_core::TerminalSnapshot,
        requested_pixel_width: u16,
        requested_pixel_height: u16,
    ) -> Result<EncodedPng> {
        let font_started = Instant::now();
        let constructions_before = self.stats.font_renderer_constructions;
        let (font_index, font_pixel_size) = self.fit_font(
            snapshot.columns,
            snapshot.rows,
            requested_pixel_width,
            requested_pixel_height,
        )?;
        let font_load_micros = if self.stats.font_renderer_constructions > constructions_before {
            font_started.elapsed().as_micros() as u64
        } else {
            0
        };
        let font = &self.fonts[font_index].font;
        let width = u32::from(snapshot.columns).saturating_mul(font.cell_width());
        let height = u32::from(snapshot.rows).saturating_mul(font.cell_height());
        let cell_width = font.cell_width();
        let cell_height = font.cell_height();

        let raster_started = Instant::now();
        let frame_capacity_before = self.frame.pixels.capacity();
        self.fonts[font_index]
            .font
            .render_into(snapshot, &mut self.frame, width, height);
        if self.frame.pixels.capacity() > frame_capacity_before {
            self.stats.frame_buffer_grows = self.stats.frame_buffer_grows.saturating_add(1);
        } else {
            self.stats.frame_buffer_reuses = self.stats.frame_buffer_reuses.saturating_add(1);
        }
        let raster_micros = raster_started.elapsed().as_micros() as u64;
        let stride = width.saturating_mul(4);

        let png_encode_started = Instant::now();
        let png_capacity_before = self.png_scratch.capacity();
        self.png_scratch.clear();
        {
            let mut encoder = png::Encoder::new(Cursor::new(&mut self.png_scratch), width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&self.frame.pixels)?;
        }
        if self.png_scratch.capacity() > png_capacity_before {
            self.stats.png_buffer_grows = self.stats.png_buffer_grows.saturating_add(1);
        } else {
            self.stats.png_buffer_reuses = self.stats.png_buffer_reuses.saturating_add(1);
        }
        let payload = self.png_scratch.clone();
        self.stats.transport_payload_copies = self.stats.transport_payload_copies.saturating_add(1);
        let png_encode_micros = png_encode_started.elapsed().as_micros() as u64;

        Ok(EncodedPng {
            width: u16::try_from(width)?,
            height: u16::try_from(height)?,
            stride,
            payload,
            font_load_micros,
            raster_micros,
            png_encode_micros,
            cell_width: u16::try_from(cell_width)?,
            cell_height: u16::try_from(cell_height)?,
            font_pixel_size: font_pixel_size.round() as u16,
            resource_stats: self.snapshot_stats(),
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the renderer-neutral bridge carries the negotiated presentation and frame identity"
    )]
    fn render_raster_update(
        &mut self,
        snapshot: &teamy_terminal_core::TerminalSnapshot,
        requested_pixel_width: u16,
        requested_pixel_height: u16,
        presentation: RasterPresentation,
        previous_snapshot: Option<&teamy_terminal_core::TerminalSnapshot>,
        frame_sequence: u64,
        base_frame_sequence: u64,
        force_full: bool,
        max_regions: u16,
        gpu_identity: GpuSlugRenderIdentity,
        gpu_cancellation: Option<&VulkanOffscreenCancellation>,
    ) -> Result<Option<RenderedRasterUpdate>> {
        match presentation.renderer {
            RasterRenderer::CpuFontdue => self.render_cpu_raster_update(
                snapshot,
                requested_pixel_width,
                requested_pixel_height,
                presentation.transport,
                previous_snapshot,
                frame_sequence,
                base_frame_sequence,
                force_full,
                max_regions,
            ),
            RasterRenderer::GpuSlug => self.render_gpu_raster_update(
                snapshot,
                requested_pixel_width,
                requested_pixel_height,
                presentation.transport,
                previous_snapshot,
                frame_sequence,
                base_frame_sequence,
                force_full,
                max_regions,
                gpu_identity,
                gpu_cancellation,
            ),
        }
    }

    fn render_cpu_raster_update(
        &mut self,
        snapshot: &teamy_terminal_core::TerminalSnapshot,
        requested_pixel_width: u16,
        requested_pixel_height: u16,
        transport: RasterTransport,
        previous_snapshot: Option<&teamy_terminal_core::TerminalSnapshot>,
        frame_sequence: u64,
        base_frame_sequence: u64,
        force_full: bool,
        max_regions: u16,
    ) -> Result<Option<RenderedRasterUpdate>> {
        let font_started = Instant::now();
        let constructions_before = self.stats.font_renderer_constructions;
        let (font_index, font_pixel_size) = self.fit_font(
            snapshot.columns,
            snapshot.rows,
            requested_pixel_width,
            requested_pixel_height,
        )?;
        let font_load_micros = if self.stats.font_renderer_constructions > constructions_before {
            font_started.elapsed().as_micros() as u64
        } else {
            0
        };
        let font = &self.fonts[font_index].font;
        let cell_width = font.cell_width();
        let cell_height = font.cell_height();
        let width = u32::from(snapshot.columns).saturating_mul(cell_width);
        let height = u32::from(snapshot.rows).saturating_mul(cell_height);
        let dimensions_changed = self.frame.width != width || self.frame.height != height;
        let damage_started = Instant::now();
        let (cell_regions, damage_forces_full) =
            if transport == RasterTransport::DirtyRawRgba && !force_full && !dimensions_changed {
                damage_regions(previous_snapshot, snapshot)
            } else {
                (Vec::new(), true)
            };
        let damage_micros = damage_started.elapsed().as_micros() as u64;
        let negotiated_region_fallback = transport == RasterTransport::DirtyRawRgba
            && cell_regions.len() > usize::from(max_regions);
        let render_full = transport != RasterTransport::DirtyRawRgba
            || force_full
            || dimensions_changed
            || damage_forces_full
            || negotiated_region_fallback;
        let full_recovery = force_full
            || (transport == RasterTransport::DirtyRawRgba
                && (dimensions_changed || damage_forces_full || negotiated_region_fallback));
        if !render_full && cell_regions.is_empty() {
            return Ok(None);
        }

        let raster_started = Instant::now();
        let frame_capacity_before = self.frame.pixels.capacity();
        if render_full {
            self.fonts[font_index]
                .font
                .render_into(snapshot, &mut self.frame, width, height);
        } else {
            self.fonts[font_index]
                .font
                .render_cells(snapshot, &mut self.frame, &cell_regions);
        }
        if self.frame.pixels.capacity() > frame_capacity_before {
            self.stats.frame_buffer_grows = self.stats.frame_buffer_grows.saturating_add(1);
        } else {
            self.stats.frame_buffer_reuses = self.stats.frame_buffer_reuses.saturating_add(1);
        }
        let raster_micros = raster_started.elapsed().as_micros() as u64;

        let encode_started = Instant::now();
        let update = if render_full && transport == RasterTransport::FullPng {
            let png_capacity_before = self.png_scratch.capacity();
            self.png_scratch.clear();
            {
                let mut encoder =
                    png::Encoder::new(Cursor::new(&mut self.png_scratch), width, height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header()?;
                writer.write_image_data(&self.frame.pixels)?;
            }
            if self.png_scratch.capacity() > png_capacity_before {
                self.stats.png_buffer_grows = self.stats.png_buffer_grows.saturating_add(1);
            } else {
                self.stats.png_buffer_reuses = self.stats.png_buffer_reuses.saturating_add(1);
            }
            self.stats.transport_payload_copies =
                self.stats.transport_payload_copies.saturating_add(1);
            RasterFrameUpdate::full_png(frame_sequence, width, height, self.png_scratch.clone())?
        } else if render_full {
            self.stats.transport_payload_copies =
                self.stats.transport_payload_copies.saturating_add(1);
            RasterFrameUpdate::full_rgba(
                frame_sequence,
                width,
                height,
                &self.frame.pixels,
                FrameCompression::Raw,
            )?
        } else {
            let rects = cell_regions
                .iter()
                .map(|region| RasterRect {
                    x: u32::from(region.column).saturating_mul(cell_width),
                    y: u32::from(region.row).saturating_mul(cell_height),
                    width: u32::from(region.columns).saturating_mul(cell_width),
                    height: u32::from(region.rows).saturating_mul(cell_height),
                })
                .collect::<Vec<_>>();
            self.stats.transport_payload_copies =
                self.stats.transport_payload_copies.saturating_add(1);
            RasterFrameUpdate::dirty_rgba(
                frame_sequence,
                base_frame_sequence,
                width,
                height,
                &self.frame.pixels,
                &rects,
            )?
        };
        let encode_micros = encode_started.elapsed().as_micros() as u64;
        let (png_encode_micros, payload_pack_micros) = if transport == RasterTransport::FullPng {
            (encode_micros, 0)
        } else {
            (0, encode_micros)
        };

        Ok(Some(RenderedRasterUpdate {
            update,
            full_recovery,
            cell_width: u16::try_from(cell_width)?,
            cell_height: u16::try_from(cell_height)?,
            font_pixel_size: font_pixel_size.round() as u16,
            font_load_micros,
            damage_micros,
            raster_micros,
            png_encode_micros,
            payload_pack_micros,
            gpu_evidence: None,
        }))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the GPU render transaction mirrors the complete negotiated frame contract"
    )]
    fn render_gpu_raster_update(
        &mut self,
        snapshot: &teamy_terminal_core::TerminalSnapshot,
        requested_pixel_width: u16,
        requested_pixel_height: u16,
        transport: RasterTransport,
        previous_snapshot: Option<&teamy_terminal_core::TerminalSnapshot>,
        frame_sequence: u64,
        base_frame_sequence: u64,
        force_full: bool,
        max_regions: u16,
        gpu_identity: GpuSlugRenderIdentity,
        gpu_cancellation: Option<&VulkanOffscreenCancellation>,
    ) -> Result<Option<RenderedRasterUpdate>> {
        let runtime = self
            .gpu_runtime
            .as_ref()
            .cloned()
            .ok_or_else(|| eyre::eyre!("cached {GPU_RENDERER_ID} runtime is unavailable"))?;
        let metrics = runtime.fit_surface_metrics(
            snapshot.columns,
            snapshot.rows,
            u32::from(requested_pixel_width),
            u32::from(requested_pixel_height),
        )?;
        let width = metrics.native_width(snapshot.columns);
        let height = metrics.native_height(snapshot.rows);
        let dimensions_changed = self.gpu_surface != Some((width, height));
        let damage_started = Instant::now();
        let (cell_regions, damage_forces_full) =
            if transport == RasterTransport::DirtyRawRgba && !force_full && !dimensions_changed {
                damage_regions(previous_snapshot, snapshot)
            } else {
                (Vec::new(), true)
            };
        let damage_micros = damage_started.elapsed().as_micros() as u64;
        let negotiated_region_fallback = transport == RasterTransport::DirtyRawRgba
            && cell_regions.len() > usize::from(max_regions);
        let render_full = transport != RasterTransport::DirtyRawRgba
            || force_full
            || dimensions_changed
            || damage_forces_full
            || negotiated_region_fallback;
        let full_recovery = force_full
            || (transport == RasterTransport::DirtyRawRgba
                && (dimensions_changed || damage_forces_full || negotiated_region_fallback));
        if !render_full && cell_regions.is_empty() {
            return Ok(None);
        }

        let request = GpuSlugRenderRequest {
            snapshot,
            width,
            height,
            font_pixel_size: metrics.font_pixel_size,
            damage: if render_full { &[] } else { &cell_regions },
            force_full_refresh: render_full,
            identity: gpu_identity,
            timeout: Duration::from_secs(5),
            cancellation: gpu_cancellation,
        };
        let rendered = self.ensure_gpu_renderer()?.render(request)?;
        self.gpu_surface = Some((width, height));
        let gpu_evidence = GpuRasterEvidence {
            identity: rendered.identity.clone(),
            visual_hash: rendered.visual_hash,
            stats: rendered.stats,
            cache: rendered.cache,
        };
        let render_stats = gpu_evidence.stats;

        let encode_started = Instant::now();
        let update = if render_full && transport == RasterTransport::FullPng {
            let pixels = rendered.full_rgba.as_deref().ok_or_else(|| {
                eyre::eyre!("{GPU_RENDERER_ID} full PNG render omitted complete RGBA readback")
            })?;
            let png_capacity_before = self.png_scratch.capacity();
            self.png_scratch.clear();
            {
                let mut encoder =
                    png::Encoder::new(Cursor::new(&mut self.png_scratch), width, height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header()?;
                writer.write_image_data(pixels)?;
            }
            if self.png_scratch.capacity() > png_capacity_before {
                self.stats.png_buffer_grows = self.stats.png_buffer_grows.saturating_add(1);
            } else {
                self.stats.png_buffer_reuses = self.stats.png_buffer_reuses.saturating_add(1);
            }
            self.stats.transport_payload_copies =
                self.stats.transport_payload_copies.saturating_add(1);
            RasterFrameUpdate::full_png(frame_sequence, width, height, self.png_scratch.clone())?
        } else if render_full {
            let pixels = rendered.full_rgba.as_deref().ok_or_else(|| {
                eyre::eyre!("{GPU_RENDERER_ID} full raw render omitted complete RGBA readback")
            })?;
            self.stats.transport_payload_copies =
                self.stats.transport_payload_copies.saturating_add(1);
            RasterFrameUpdate::full_rgba(
                frame_sequence,
                width,
                height,
                pixels,
                FrameCompression::Raw,
            )?
        } else {
            let packed = rendered
                .dirty_regions
                .iter()
                .map(|region| PackedRasterRegion {
                    rect: RasterRect {
                        x: region.x,
                        y: region.y,
                        width: region.width,
                        height: region.height,
                    },
                    rgba: &region.rgba,
                })
                .collect::<Vec<_>>();
            self.stats.transport_payload_copies =
                self.stats.transport_payload_copies.saturating_add(1);
            RasterFrameUpdate::dirty_rgba_packed(
                frame_sequence,
                base_frame_sequence,
                width,
                height,
                &packed,
            )?
        };
        let packet_micros = encode_started.elapsed().as_micros() as u64;
        let (png_encode_micros, payload_pack_micros) = if transport == RasterTransport::FullPng {
            (packet_micros, render_stats.payload_pack_us)
        } else {
            (
                0,
                render_stats.payload_pack_us.saturating_add(packet_micros),
            )
        };

        Ok(Some(RenderedRasterUpdate {
            update,
            full_recovery,
            cell_width: u16::try_from(metrics.cell_width)?,
            cell_height: u16::try_from(metrics.cell_height)?,
            font_pixel_size: metrics.font_pixel_size.round() as u16,
            font_load_micros: render_stats
                .outline_extraction_us
                .saturating_add(render_stats.directional_band_build_us),
            damage_micros,
            raster_micros: render_stats.total_us,
            png_encode_micros,
            payload_pack_micros,
            gpu_evidence: Some(gpu_evidence),
        }))
    }
}

struct EncodedPng {
    width: u16,
    height: u16,
    stride: u32,
    payload: Vec<u8>,
    font_load_micros: u64,
    raster_micros: u64,
    png_encode_micros: u64,
    cell_width: u16,
    cell_height: u16,
    font_pixel_size: u16,
    resource_stats: TerminalRenderResourceStats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RasterRenderer {
    CpuFontdue,
    GpuSlug,
}

impl RasterRenderer {
    const fn renderer_id(self) -> &'static str {
        match self {
            Self::CpuFontdue => CPU_RENDERER_ID,
            Self::GpuSlug => GPU_RENDERER_ID,
        }
    }

    fn from_request(
        requested_renderer_id: &str,
        gpu_registry: &GpuRendererRegistry,
        server_sequence: i64,
    ) -> Result<Self, TerminalError> {
        match requested_renderer_id {
            CPU_RENDERER_ID => Ok(Self::CpuFontdue),
            GPU_RENDERER_ID => {
                gpu_registry.runtime(server_sequence)?;
                Ok(Self::GpuSlug)
            }
            unsupported => Err(VoxTerminalService::error(
                TerminalErrorCode::UnsupportedCapability,
                format!("unsupported terminal renderer: {unsupported}"),
                server_sequence,
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RasterTransport {
    FullPng,
    FullRawRgba,
    DirtyRawRgba,
}

impl RasterTransport {
    const fn transport_id(self) -> &'static str {
        match self {
            Self::FullPng => TRANSPORT_FULL_PNG,
            Self::FullRawRgba => TRANSPORT_FULL_RAW_RGBA,
            Self::DirtyRawRgba => TRANSPORT_DIRTY_RAW_RGBA,
        }
    }

    const fn damage_mode_id(self) -> &'static str {
        match self {
            Self::FullPng | Self::FullRawRgba => DAMAGE_FULL,
            Self::DirtyRawRgba => DAMAGE_DIRTY,
        }
    }

    const fn encoding(self) -> TerminalFrameEncoding {
        match self {
            Self::FullPng => TerminalFrameEncoding::Png,
            Self::FullRawRgba | Self::DirtyRawRgba => TerminalFrameEncoding::Rgba8,
        }
    }

    const fn steady_frame_kind(self) -> TerminalRasterFrameKind {
        match self {
            Self::FullPng | Self::FullRawRgba => TerminalRasterFrameKind::Full,
            Self::DirtyRawRgba => TerminalRasterFrameKind::DirtyRegions,
        }
    }

    fn from_request(request: &TerminalRasterSubscribeRequest) -> Result<Self, TerminalError> {
        if request.requested_transport_version != RASTER_TRANSPORT_VERSION {
            return Err(VoxTerminalService::error(
                TerminalErrorCode::UnsupportedCapability,
                format!(
                    "unsupported terminal transport version: {}",
                    request.requested_transport_version
                ),
                0,
            ));
        }
        let transport = match request.requested_transport_id.as_str() {
            TRANSPORT_FULL_PNG => Self::FullPng,
            TRANSPORT_FULL_RAW_RGBA => Self::FullRawRgba,
            TRANSPORT_DIRTY_RAW_RGBA => Self::DirtyRawRgba,
            unsupported => {
                return Err(VoxTerminalService::error(
                    TerminalErrorCode::UnsupportedCapability,
                    format!("unsupported terminal transport: {unsupported}"),
                    0,
                ));
            }
        };
        if request.requested_damage_mode_id != transport.damage_mode_id() {
            return Err(VoxTerminalService::error(
                TerminalErrorCode::UnsupportedCapability,
                format!(
                    "transport {} requires damage mode {} instead of {}",
                    transport.transport_id(),
                    transport.damage_mode_id(),
                    request.requested_damage_mode_id
                ),
                0,
            ));
        }
        Ok(transport)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RasterPresentation {
    renderer: RasterRenderer,
    transport: RasterTransport,
}

impl RasterPresentation {
    fn from_request(
        request: &TerminalRasterSubscribeRequest,
        gpu_registry: &GpuRendererRegistry,
        server_sequence: i64,
    ) -> Result<Self, TerminalError> {
        Ok(Self {
            renderer: RasterRenderer::from_request(
                &request.requested_renderer_id,
                gpu_registry,
                server_sequence,
            )?,
            transport: RasterTransport::from_request(request)?,
        })
    }
}

struct RenderedRasterUpdate {
    update: RasterFrameUpdate,
    full_recovery: bool,
    cell_width: u16,
    cell_height: u16,
    font_pixel_size: u16,
    font_load_micros: u64,
    damage_micros: u64,
    raster_micros: u64,
    png_encode_micros: u64,
    payload_pack_micros: u64,
    gpu_evidence: Option<GpuRasterEvidence>,
}

#[derive(Clone, Debug)]
struct GpuRasterEvidence {
    identity: GpuSlugRenderIdentity,
    visual_hash: Option<u64>,
    stats: GpuSlugRenderStats,
    cache: GpuSlugCacheStats,
}

struct CapturedTerminal {
    snapshot: teamy_terminal_core::TerminalSnapshot,
    surface: TerminalSurfaceMetrics,
    terminal_sequence: i64,
    mutations: i64,
}

#[derive(Default)]
struct PublicationCounters {
    renders_started: i64,
    renders_completed: i64,
    pre_render_coalesced: i64,
    credit_blocked_sends: i64,
    frames_pushed: i64,
    pending_depth_max: u8,
    mutation_to_send_us: i64,
    credit_wait_us: i64,
}

impl PublicationCounters {
    fn telemetry(&self, mutations: i64, pending_depth: u8) -> TerminalPublicationTelemetry {
        TerminalPublicationTelemetry {
            mutations,
            renders_started: self.renders_started,
            renders_completed: self.renders_completed,
            pre_render_coalesced: self.pre_render_coalesced,
            credit_blocked_sends: self.credit_blocked_sends,
            frames_pushed: self.frames_pushed.saturating_add(1),
            pending_depth,
            pending_depth_max: self.pending_depth_max.max(pending_depth),
            mutation_to_send_us: self.mutation_to_send_us,
            credit_wait_us: self.credit_wait_us,
        }
    }
}

/// Machine-readable Rust-side evidence for one full-PNG snapshot.
///
/// This stays on the diagnostic surface instead of enlarging every Vox
/// response. Java joins it to its own presentation measurements with the
/// correlation ID and request sequence.
#[derive(Clone, Debug, Facet)]
pub struct TerminalSnapshotTimingWitness {
    pub request_sequence: i64,
    pub server_sequence: i64,
    pub correlation_id: String,
    pub logical_columns: u16,
    pub logical_rows: u16,
    pub panel_width: u16,
    pub panel_height: u16,
    pub frame_width: u16,
    pub frame_height: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub font_pixel_size: u16,
    pub payload_bytes: u64,
    pub pty_drain_us: u64,
    pub terminal_snapshot_us: u64,
    pub font_load_us: u64,
    pub raster_us: u64,
    pub png_encode_us: u64,
    pub total_us: u64,
    pub font_catalog_acquisitions: u64,
    pub process_font_catalog_loads: u64,
    pub font_renderer_cache_hits: u64,
    pub font_renderer_cache_misses: u64,
    pub font_renderer_constructions: u64,
    pub font_renderer_evictions: u64,
    pub font_renderer_cache_len: u64,
    pub font_renderer_cache_capacity: u64,
    pub glyph_cache_hits: u64,
    pub glyph_cache_misses: u64,
    pub glyph_cache_evictions: u64,
    pub cached_glyphs: u64,
    pub glyph_cache_capacity: u64,
    pub frame_buffer_grows: u64,
    pub frame_buffer_reuses: u64,
    pub frame_buffer_capacity_bytes: u64,
    pub png_buffer_grows: u64,
    pub png_buffer_reuses: u64,
    pub png_buffer_capacity_bytes: u64,
    pub transport_payload_copies: u64,
}

fn process_epoch() -> Arc<str> {
    Arc::from(
        PROCESS_EPOCH
            .get_or_init(|| {
                let started = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO)
                    .as_nanos();
                format!("process-{}-{started}", std::process::id())
            })
            .as_str(),
    )
}

fn next_connection_epoch(process_epoch: &str) -> Arc<str> {
    Arc::from(format!(
        "{process_epoch}:connection:{}",
        NEXT_CONNECTION_EPOCH.fetch_add(1, Ordering::Relaxed)
    ))
}

impl VoxTerminalService {
    #[must_use]
    pub fn new() -> Self {
        let process_epoch = process_epoch();
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            connection_epoch: next_connection_epoch(&process_epoch),
            process_epoch,
            gpu_registry: process_gpu_registry(),
            latest_snapshot_timing: Arc::new(Mutex::new(None)),
        }
    }

    fn for_connection(&self) -> Self {
        Self {
            sessions: Arc::clone(&self.sessions),
            process_epoch: Arc::clone(&self.process_epoch),
            connection_epoch: next_connection_epoch(&self.process_epoch),
            gpu_registry: Arc::clone(&self.gpu_registry),
            latest_snapshot_timing: Arc::clone(&self.latest_snapshot_timing),
        }
    }

    /// Returns the most recent Rust-side full-PNG timing witness.
    #[must_use]
    #[cfg(test)]
    pub fn latest_snapshot_timing(&self) -> Option<TerminalSnapshotTimingWitness> {
        self.latest_snapshot_timing
            .lock()
            .ok()
            .and_then(|timing| timing.clone())
    }

    fn record_snapshot_timing(&self, timing: TerminalSnapshotTimingWitness) {
        if let Ok(mut latest) = self.latest_snapshot_timing.lock() {
            *latest = Some(timing);
        }
    }

    fn session(&self, session_id: &str) -> Result<Arc<TerminalSessionRuntime>, TerminalError> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| {
                Self::error(
                    TerminalErrorCode::Internal,
                    "terminal session registry lock poisoned",
                    0,
                )
            })?
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                Self::error(
                    TerminalErrorCode::SessionNotFound,
                    "terminal session not found",
                    0,
                )
            })?;
        if session.connection_epoch != self.connection_epoch {
            return Err(Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found for this connection",
                0,
            ));
        }
        Ok(session)
    }

    fn lock_session(
        session: &TerminalSessionRuntime,
    ) -> Result<std::sync::MutexGuard<'_, TerminalSessionState>, TerminalError> {
        session.state.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })
    }

    fn capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            text_input: true,
            key_input: true,
            mouse_input: true,
            structured_cells: false,
            raster_frames: true,
            selection: false,
            scrollback: true,
            dirty_frames: true,
            max_width: MAX_COLUMNS,
            max_height: MAX_ROWS,
            max_frame_bytes: MAX_FRAME_BYTES,
            backend_id: CPU_RENDERER_ID.to_string(),
            transport_id: TRANSPORT_FULL_PNG.to_string(),
        }
    }

    fn presentation_modes(gpu_available: bool) -> Vec<TerminalPresentationMode> {
        let mut renderers = vec![RasterRenderer::CpuFontdue];
        if gpu_available {
            renderers.push(RasterRenderer::GpuSlug);
        }
        let transports = [
            RasterTransport::FullPng,
            RasterTransport::FullRawRgba,
            RasterTransport::DirtyRawRgba,
        ];
        renderers
            .into_iter()
            .flat_map(|renderer| {
                transports
                    .into_iter()
                    .map(move |transport| TerminalPresentationMode {
                        renderer_id: renderer.renderer_id().to_string(),
                        rasterization_owner: TerminalRasterizationOwner::Server,
                        damage_mode_id: transport.damage_mode_id().to_string(),
                        transport_id: transport.transport_id().to_string(),
                        transport_version: RASTER_TRANSPORT_VERSION,
                        encoding: transport.encoding(),
                        steady_frame_kind: transport.steady_frame_kind(),
                        frame_contract_version: RASTER_FRAME_CONTRACT_VERSION,
                        origin: TerminalFrameOrigin::TopLeft,
                        alpha_mode: TerminalAlphaMode::Straight,
                        color_space: TerminalColorSpace::Srgb,
                        max_pixel_width: MAX_PIXEL_WIDTH,
                        max_pixel_height: MAX_PIXEL_HEIGHT,
                        max_frame_bytes: MAX_FRAME_BYTES,
                        max_regions: MAX_DIRTY_REGIONS as u16,
                    })
            })
            .collect()
    }

    fn unavailable_presentations(
        error: Option<TerminalError>,
    ) -> Vec<TerminalPresentationUnavailable> {
        let Some(error) = error else {
            return Vec::new();
        };
        [
            RasterTransport::FullPng,
            RasterTransport::FullRawRgba,
            RasterTransport::DirtyRawRgba,
        ]
        .into_iter()
        .map(|transport| TerminalPresentationUnavailable {
            renderer_id: GPU_RENDERER_ID.to_string(),
            rasterization_owner: TerminalRasterizationOwner::Server,
            damage_mode_id: transport.damage_mode_id().to_string(),
            transport_id: transport.transport_id().to_string(),
            transport_version: RASTER_TRANSPORT_VERSION,
            error: error.clone(),
        })
        .collect()
    }

    fn presentation_capability_sets(
        &self,
        server_sequence: i64,
    ) -> Result<
        (
            Vec<TerminalPresentationMode>,
            Vec<TerminalPresentationUnavailable>,
        ),
        TerminalError,
    > {
        let snapshot = self.gpu_registry.capability_snapshot(server_sequence)?;
        Ok(match snapshot {
            GpuCapabilitySnapshot::Available => (Self::presentation_modes(true), Vec::new()),
            GpuCapabilitySnapshot::Unavailable(error) => {
                let error = TerminalError {
                    code: error.code,
                    message: error.message,
                    retryable: error.retryable,
                    server_sequence,
                };
                (
                    Self::presentation_modes(false),
                    Self::unavailable_presentations(Some(error)),
                )
            }
        })
    }

    fn next_sequence(sequence: &mut i64) -> i64 {
        *sequence = sequence.saturating_add(1).max(1);
        *sequence
    }

    fn error(code: TerminalErrorCode, message: impl Into<String>, sequence: i64) -> TerminalError {
        TerminalError {
            code,
            message: message.into(),
            retryable: matches!(
                code,
                TerminalErrorCode::Internal | TerminalErrorCode::Cancelled
            ),
            server_sequence: sequence,
        }
    }

    fn raster_render_error(&self, error: &eyre::Report, server_sequence: i64) -> TerminalError {
        if let Some(device_lost) = self
            .gpu_registry
            .retire_on_device_loss(error, server_sequence)
        {
            return device_lost;
        }
        let code = if matches!(
            error.downcast_ref::<GpuSlugRendererError>(),
            Some(GpuSlugRendererError::Vulkan(
                VulkanOffscreenError::Cancelled
            ))
        ) {
            TerminalErrorCode::Cancelled
        } else {
            TerminalErrorCode::Internal
        };
        Self::error(code, error.to_string(), server_sequence)
    }

    fn validate_and_record_gpu_evidence(
        rendered: &RenderedRasterUpdate,
        presentation: RasterPresentation,
        request: &TerminalRasterSubscribeRequest,
        runtime: &TerminalSessionRuntime,
        terminal_sequence: i64,
        frame_sequence: i64,
    ) -> Result<(), TerminalError> {
        let Some(evidence) = &rendered.gpu_evidence else {
            if presentation.renderer == RasterRenderer::GpuSlug {
                return Err(Self::error(
                    TerminalErrorCode::Internal,
                    "GPU raster result omitted renderer evidence",
                    terminal_sequence,
                ));
            }
            return Ok(());
        };
        let expected_terminal_sequence =
            u64::try_from(terminal_sequence).unwrap_or(u64::MAX);
        let expected_frame_sequence = u64::try_from(frame_sequence).unwrap_or(u64::MAX);
        if evidence.identity.session_sequence != runtime.session_sequence
            || evidence.identity.terminal_sequence != expected_terminal_sequence
            || evidence.identity.frame_sequence != expected_frame_sequence
            || evidence.identity.presentation_generation != request.presentation_generation
            || evidence.identity.correlation_id != request.correlation_id
        {
            return Err(Self::error(
                TerminalErrorCode::Internal,
                format!(
                    "GPU raster result identity mismatch: expected session={} terminal={} frame={} generation={} correlation={}, got {:?}",
                    runtime.session_sequence,
                    expected_terminal_sequence,
                    expected_frame_sequence,
                    request.presentation_generation,
                    request.correlation_id,
                    evidence.identity
                ),
                terminal_sequence,
            ));
        }
        tracing::info!(
            target: "teamy_terminal::vox_gpu_raster",
            renderer_id = GPU_RENDERER_ID,
            transport_id = presentation.transport.transport_id(),
            session_sequence = evidence.identity.session_sequence,
            terminal_sequence = evidence.identity.terminal_sequence,
            frame_sequence = evidence.identity.frame_sequence,
            presentation_generation = evidence.identity.presentation_generation,
            correlation_id = evidence.identity.correlation_id,
            visual_hash = ?evidence.visual_hash,
            geometry_build_us = evidence.stats.geometry_build_us,
            outline_extraction_us = evidence.stats.outline_extraction_us,
            directional_band_build_us = evidence.stats.directional_band_build_us,
            upload_bytes = evidence.stats.upload_bytes,
            upload_us = evidence.stats.upload_us,
            command_record_us = evidence.stats.command_record_us,
            queue_submit_us = evidence.stats.queue_submit_us,
            gpu_completion_wait_us = evidence.stats.gpu_completion_wait_us,
            readback_map_copy_us = evidence.stats.readback_map_copy_us,
            payload_pack_us = evidence.stats.payload_pack_us,
            full_rgba_pack_us = ?evidence.stats.full_rgba_pack_us,
            dirty_region_pack_us = ?evidence.stats.dirty_region_pack_us,
            requested_pixels = evidence.stats.requested_pixels,
            target_allocation_id = evidence.stats.target_allocation_id,
            geometry_hits = evidence.cache.geometry_hits,
            geometry_misses = evidence.cache.geometry_misses,
            geometry_evictions = evidence.cache.geometry_evictions,
            retained_glyphs = evidence.cache.retained_glyphs,
            buffer_allocations = evidence.cache.buffer_allocations,
            target_allocations = evidence.cache.targets.allocations,
            target_reuses = evidence.cache.targets.reuses,
            target_evictions = evidence.cache.targets.evictions,
            "validated correlated GPU raster evidence before Vox publication"
        );
        Ok(())
    }

    fn validate_dimensions(width: u16, height: u16) -> Result<(), TerminalError> {
        if width == 0 || height == 0 || width > MAX_COLUMNS || height > MAX_ROWS {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                format!("terminal dimensions must be within 1..={MAX_COLUMNS} × 1..={MAX_ROWS}"),
                0,
            ));
        }
        Ok(())
    }

    fn validate_pixel_dimensions(width: u16, height: u16) -> Result<(), TerminalError> {
        if width > MAX_PIXEL_WIDTH || height > MAX_PIXEL_HEIGHT {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                format!(
                    "terminal pixel dimensions must be within 0..={MAX_PIXEL_WIDTH} × 0..={MAX_PIXEL_HEIGHT}"
                ),
                0,
            ));
        }
        Ok(())
    }

    fn process_pty_event(runtime: &TerminalSessionRuntime, event: PtyEvent) {
        let mut state = match runtime.state.lock() {
            Ok(state) => state,
            Err(_) => {
                runtime.closed.store(true, Ordering::Release);
                runtime.publication.close();
                return;
            }
        };
        match event {
            PtyEvent::Output(bytes) => {
                if let Err(error) = Self::answer_terminal_queries(&state.pty, &bytes) {
                    tracing::warn!(session_id = %runtime.session_id, ?error, "terminal query response failed");
                    state.closed = true;
                    runtime.closed.store(true, Ordering::Release);
                }
                state.debug_output.extend_from_slice(&bytes);
                if state.debug_output.len() > 64 * 1024 {
                    let remove = state.debug_output.len() - 64 * 1024;
                    state.debug_output.drain(..remove);
                }
                state.terminal.process_bytes(&bytes);
            }
            PtyEvent::Exited(status) => {
                state.closed = true;
                runtime.closed.store(true, Ordering::Release);
                if !status.success {
                    tracing::warn!(
                        session_id = %runtime.session_id,
                        exit_code = ?status.code,
                        "terminal process exited unsuccessfully"
                    );
                }
            }
            PtyEvent::Error(message) => {
                state.closed = true;
                runtime.closed.store(true, Ordering::Release);
                tracing::warn!(session_id = %runtime.session_id, %message, "terminal PTY failed");
            }
        }
        let sequence = Self::next_sequence(&mut state.sequence);
        drop(state);
        runtime.publish_mutation(sequence, false);
    }

    fn spawn_session_actor(
        runtime: &Arc<TerminalSessionRuntime>,
        events: teamy_terminal_pty::PtyEventReceiver,
        actor_receiver: mpsc::Receiver<SessionActorMessage>,
    ) {
        let forwarder =
            events.spawn_forwarder(runtime.actor_sender.clone(), SessionActorMessage::Pty);
        let runtime = Arc::downgrade(runtime);
        std::thread::spawn(move || {
            while let Ok(message) = actor_receiver.recv() {
                let Some(runtime) = runtime.upgrade() else {
                    break;
                };
                match message {
                    SessionActorMessage::Pty(event) => Self::process_pty_event(&runtime, event),
                    SessionActorMessage::Shutdown => break,
                }
            }
            drop(runtime);
            let _ = forwarder.join();
        });
    }

    fn answer_terminal_queries(pty: &PtySessionControl, bytes: &[u8]) -> Result<(), TerminalError> {
        let cursor_queries = bytes
            .windows(4)
            .filter(|window| *window == b"\x1b[6n")
            .count();
        for _ in 0..cursor_queries {
            pty.write_input(b"\x1b[1;1R")
                .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        }
        let device_attribute_queries = bytes
            .windows(3)
            .filter(|window| *window == b"\x1b[c")
            .count();
        for _ in 0..device_attribute_queries {
            pty.write_input(b"\x1b[?1;2c")
                .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        }
        Ok(())
    }

    fn empty_range() -> TerminalRange {
        TerminalRange {
            start_x: 0,
            start_y: 0,
            end_x: 0,
            end_y: 0,
        }
    }

    fn empty_prompt() -> TerminalPromptMetadata {
        TerminalPromptMetadata {
            prompt_present: false,
            prompt: Self::empty_range(),
            command_present: false,
            command: Self::empty_range(),
            command_status_present: false,
            command_status: 0,
        }
    }

    fn prompt_metadata(prompt: &teamy_terminal_core::PromptMetadata) -> TerminalPromptMetadata {
        let range = |range: Option<teamy_terminal_core::TerminalRange>| {
            range.map_or((false, Self::empty_range()), |range| {
                (
                    true,
                    TerminalRange {
                        start_x: range.start.column,
                        start_y: range.start.row,
                        end_x: range.end.column,
                        end_y: range.end.row,
                    },
                )
            })
        };
        let (prompt_present, prompt_range) = range(prompt.prompt_range);
        let (command_present, command_range) = range(prompt.command_range);
        let (command_status_present, command_status) = prompt
            .command_status
            .map_or((false, 0), |status| (true, status));
        TerminalPromptMetadata {
            prompt_present,
            prompt: prompt_range,
            command_present,
            command: command_range,
            command_status_present,
            command_status,
        }
    }

    fn close_connection_sessions(&self) {
        let removed = self
            .sessions
            .lock()
            .map(|mut sessions| {
                let session_ids: Vec<_> = sessions
                    .iter()
                    .filter(|(_, session)| session.connection_epoch == self.connection_epoch)
                    .map(|(session_id, _)| session_id.clone())
                    .collect();
                session_ids
                    .into_iter()
                    .filter_map(|session_id| sessions.remove(&session_id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for session in removed {
            session.shutdown();
        }
    }

    fn capture_terminal(
        runtime: &TerminalSessionRuntime,
    ) -> Result<CapturedTerminal, TerminalError> {
        let state = Self::lock_session(runtime)?;
        Ok(CapturedTerminal {
            snapshot: state.terminal.snapshot(),
            surface: state.surface.clone(),
            terminal_sequence: state.sequence,
            mutations: runtime.publication.mutations.load(Ordering::Acquire),
        })
    }

    fn render_captured(
        resources: &mut TerminalRenderResources,
        session_id: &str,
        capture: &CapturedTerminal,
        request_sequence: i64,
        correlation_id: &str,
        max_frame_bytes: u32,
    ) -> Result<TerminalSnapshot, TerminalError> {
        let started = Instant::now();
        let encoded = resources
            .encode_png(
                &capture.snapshot,
                capture.surface.panel_width,
                capture.surface.panel_height,
            )
            .map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    capture.terminal_sequence,
                )
            })?;
        let max_frame_bytes = max_frame_bytes.min(MAX_FRAME_BYTES);
        if max_frame_bytes == 0 || encoded.payload.len() > max_frame_bytes as usize {
            return Err(Self::error(
                TerminalErrorCode::CapacityExceeded,
                "PNG snapshot exceeds requested frame bound",
                capture.terminal_sequence,
            ));
        }
        let total_us = i64::try_from(started.elapsed().as_micros()).unwrap_or(i64::MAX);
        let width = encoded.width;
        let height = encoded.height;
        Ok(TerminalSnapshot {
            session_id: session_id.to_string(),
            sequence: capture.terminal_sequence,
            request_sequence,
            logical_columns: capture.snapshot.columns,
            logical_rows: capture.snapshot.rows,
            width,
            height,
            panel_width: capture.surface.panel_width,
            panel_height: capture.surface.panel_height,
            cell_width: encoded.cell_width,
            cell_height: encoded.cell_height,
            font_pixel_size: encoded.font_pixel_size,
            correlation_id: correlation_id.to_string(),
            encoding: TerminalFrameEncoding::Png,
            kind: TerminalFrameKind::Full,
            stride: encoded.stride,
            tile_x: 0,
            tile_y: 0,
            tile_width: width,
            tile_height: height,
            payload: encoded.payload,
            complete: true,
            cursor: TerminalCursor {
                x: capture.snapshot.cursor.column,
                y: capture.snapshot.cursor.row,
                visible: capture.snapshot.cursor_visible,
            },
            selection_present: false,
            selection: TerminalSelection {
                anchor_x: 0,
                anchor_y: 0,
                focus_x: 0,
                focus_y: 0,
            },
            prompt: Self::prompt_metadata(&capture.snapshot.prompt),
            timing: TerminalSnapshotTiming {
                pty_drain_us: 0,
                terminal_snapshot_us: 0,
                font_load_us: i64::try_from(encoded.font_load_micros).unwrap_or(i64::MAX),
                raster_us: i64::try_from(encoded.raster_micros).unwrap_or(i64::MAX),
                png_encode_us: i64::try_from(encoded.png_encode_micros).unwrap_or(i64::MAX),
                total_us,
            },
        })
    }

    fn raster_frame(
        capture: &CapturedTerminal,
        rendered: RenderedRasterUpdate,
        total_us: i64,
    ) -> Result<TerminalRasterFrame, TerminalError> {
        let descriptor = rendered.update.descriptor;
        let encoding = match descriptor.compression {
            FrameCompression::Raw => TerminalFrameEncoding::Rgba8,
            FrameCompression::Png => TerminalFrameEncoding::Png,
        };
        let kind = match descriptor.kind {
            RasterUpdateKind::Full => TerminalRasterFrameKind::Full,
            RasterUpdateKind::DirtyRegions => TerminalRasterFrameKind::DirtyRegions,
        };
        let regions = rendered
            .update
            .regions
            .iter()
            .map(|region| {
                Ok(TerminalRasterRegion {
                    x: u16::try_from(region.x).map_err(|error| {
                        Self::error(
                            TerminalErrorCode::Internal,
                            error.to_string(),
                            capture.terminal_sequence,
                        )
                    })?,
                    y: u16::try_from(region.y).map_err(|error| {
                        Self::error(
                            TerminalErrorCode::Internal,
                            error.to_string(),
                            capture.terminal_sequence,
                        )
                    })?,
                    width: u16::try_from(region.width).map_err(|error| {
                        Self::error(
                            TerminalErrorCode::Internal,
                            error.to_string(),
                            capture.terminal_sequence,
                        )
                    })?,
                    height: u16::try_from(region.height).map_err(|error| {
                        Self::error(
                            TerminalErrorCode::Internal,
                            error.to_string(),
                            capture.terminal_sequence,
                        )
                    })?,
                    stride: region.stride,
                    payload_offset: region.payload_offset,
                    payload_length: region.payload_length,
                })
            })
            .collect::<Result<Vec<_>, TerminalError>>()?;
        Ok(TerminalRasterFrame {
            logical_columns: capture.snapshot.columns,
            logical_rows: capture.snapshot.rows,
            width: u16::try_from(descriptor.width).map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    capture.terminal_sequence,
                )
            })?,
            height: u16::try_from(descriptor.height).map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    capture.terminal_sequence,
                )
            })?,
            surface: TerminalSurfaceMetrics {
                columns: capture.snapshot.columns,
                rows: capture.snapshot.rows,
                panel_width: capture.surface.panel_width,
                panel_height: capture.surface.panel_height,
                cell_width: rendered.cell_width,
                cell_height: rendered.cell_height,
                font_pixel_size: rendered.font_pixel_size,
            },
            encoding,
            kind,
            // Encoded PNG bytes do not expose a row stride. The decoded
            // RGBA8 layout is described by dimensions/origin/alpha/color
            // metadata, while raw full frames carry width * 4 and packed
            // dirty frames carry per-region strides.
            stride: if encoding == TerminalFrameEncoding::Png {
                0
            } else {
                descriptor.stride
            },
            origin: TerminalFrameOrigin::TopLeft,
            alpha_mode: TerminalAlphaMode::Straight,
            color_space: TerminalColorSpace::Srgb,
            font_id: EMBEDDED_FONT_ID.to_string(),
            font_sha256: EMBEDDED_FONT_SHA256.to_string(),
            payload: rendered.update.bytes,
            regions,
            complete: true,
            cursor: TerminalCursor {
                x: capture.snapshot.cursor.column,
                y: capture.snapshot.cursor.row,
                visible: capture.snapshot.cursor_visible,
            },
            selection_present: false,
            selection: TerminalSelection {
                anchor_x: 0,
                anchor_y: 0,
                focus_x: 0,
                focus_y: 0,
            },
            prompt: Self::prompt_metadata(&capture.snapshot.prompt),
            timing: TerminalRasterSnapshotTiming {
                pty_drain_us: 0,
                terminal_snapshot_us: 0,
                font_load_us: i64::try_from(rendered.font_load_micros).unwrap_or(i64::MAX),
                damage_us: i64::try_from(rendered.damage_micros).unwrap_or(i64::MAX),
                raster_us: i64::try_from(rendered.raster_micros).unwrap_or(i64::MAX),
                png_encode_us: i64::try_from(rendered.png_encode_micros).unwrap_or(i64::MAX),
                payload_pack_us: i64::try_from(rendered.payload_pack_micros).unwrap_or(i64::MAX),
                total_us,
            },
        })
    }
}

impl Default for VoxTerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal for VoxTerminalService {
    async fn connect(
        &self,
        request: TerminalConnectRequest,
    ) -> Result<TerminalConnectResult, TerminalError> {
        Self::validate_dimensions(request.requested_width, request.requested_height)?;
        Self::validate_pixel_dimensions(request.surface.panel_width, request.surface.panel_height)?;
        let epoch_sequence = NEXT_SESSION_EPOCH.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("sfm-terminal-{epoch_sequence}");
        let session_epoch: Arc<str> =
            Arc::from(format!("{}:session:{epoch_sequence}", self.process_epoch));
        let pty = PtySession::spawn(&PtyConfig::interactive())
            .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        pty.resize(request.requested_width, request.requested_height, 0, 0)
            .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        let (pty, events) = pty.split();
        let surface = TerminalSurfaceMetrics {
            columns: request.requested_width,
            rows: request.requested_height,
            panel_width: request.surface.panel_width,
            panel_height: request.surface.panel_height,
            cell_width: 0,
            cell_height: 0,
            font_pixel_size: 0,
        };
        let state = TerminalSessionState {
            pty,
            terminal: TerminalSession::new(
                TerminalLimits::default(),
                request.requested_width,
                request.requested_height,
            ),
            sequence: 1,
            surface,
            closed: false,
            debug_output: Vec::new(),
        };
        let sequence = state.sequence;
        let connected_surface = state.surface.clone();
        let (actor_sender, actor_receiver) = mpsc::sync_channel(SESSION_ACTOR_CAPACITY);
        let runtime = Arc::new(TerminalSessionRuntime {
            session_id: session_id.clone(),
            session_sequence: epoch_sequence,
            connection_epoch: Arc::clone(&self.connection_epoch),
            session_epoch,
            gpu_registry: Arc::clone(&self.gpu_registry),
            state: Mutex::new(state),
            publication: PublicationHub::new(),
            explicit_render_resources: Mutex::new(Some(TerminalRenderResources::default())),
            raster_render_resources: Mutex::new(Vec::with_capacity(
                MAX_CONCURRENT_RASTER_PRESENTATIONS_PER_SESSION,
            )),
            raster_render_cancellations: Mutex::new(BTreeMap::new()),
            raster_render_permits: Arc::new(Semaphore::new(
                MAX_CONCURRENT_RASTER_PRESENTATIONS_PER_SESSION,
            )),
            actor_sender,
            terminal_sequence: AtomicI64::new(sequence),
            closed: AtomicBool::new(false),
        });
        Self::spawn_session_actor(&runtime, events, actor_receiver);
        self.sessions
            .lock()
            .map_err(|_| {
                Self::error(
                    TerminalErrorCode::Internal,
                    "terminal session lock poisoned",
                    sequence,
                )
            })?
            .insert(session_id.clone(), runtime);
        Ok(TerminalConnectResult {
            session_id,
            capabilities: Self::capabilities(),
            surface: connected_surface,
            server_sequence: sequence,
            state: TerminalState::Ready,
            correlation_id: request.correlation_id,
        })
    }

    async fn capabilities(
        &self,
        request: TerminalCapabilitiesRequest,
    ) -> Result<TerminalCapabilitiesResult, TerminalError> {
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        Ok(TerminalCapabilitiesResult {
            session_id: request.session_id,
            capabilities: Self::capabilities(),
            server_sequence: session.sequence,
        })
    }

    async fn presentation_capabilities(
        &self,
        request: TerminalPresentationCapabilitiesRequest,
    ) -> Result<TerminalPresentationCapabilitiesResult, TerminalError> {
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        let (modes, unavailable_presentations) =
            self.presentation_capability_sets(session.sequence)?;
        Ok(TerminalPresentationCapabilitiesResult {
            session_id: request.session_id,
            default_renderer_id: CPU_RENDERER_ID.to_string(),
            default_transport_id: TRANSPORT_FULL_PNG.to_string(),
            modes,
            unavailable_presentations,
            server_sequence: session.sequence,
        })
    }

    async fn resize(
        &self,
        request: TerminalResizeRequest,
    ) -> Result<TerminalResizeResult, TerminalError> {
        Self::validate_dimensions(request.width, request.height)?;
        Self::validate_pixel_dimensions(request.surface.panel_width, request.surface.panel_height)?;
        let runtime = self.session(&request.session_id)?;
        let mut session = Self::lock_session(&runtime)?;
        session.surface.columns = request.width;
        session.surface.rows = request.height;
        session.surface.panel_width = request.surface.panel_width;
        session.surface.panel_height = request.surface.panel_height;
        session.surface.cell_width = request.surface.cell_width;
        session.surface.cell_height = request.surface.cell_height;
        session.surface.font_pixel_size = request.surface.font_pixel_size;
        session
            .pty
            .resize(request.width, request.height, 0, 0)
            .map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    session.sequence,
                )
            })?;
        session.terminal.resize(request.width, request.height);
        let sequence = Self::next_sequence(&mut session.sequence);
        let surface = session.surface.clone();
        drop(session);
        runtime.publish_mutation(sequence, true);
        Ok(TerminalResizeResult {
            session_id: request.session_id,
            width: request.width,
            height: request.height,
            surface,
            server_sequence: sequence,
            correlation_id: request.correlation_id,
        })
    }

    async fn send_text(
        &self,
        request: TerminalTextInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        if request.text.len() > 16 * 1024 {
            return Err(Self::error(
                TerminalErrorCode::CapacityExceeded,
                "terminal input exceeds 16 KiB",
                0,
            ));
        }
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        let text = request.text.into_bytes();
        session.pty.write_input(&text).map_err(|error| {
            Self::error(
                TerminalErrorCode::Internal,
                error.to_string(),
                session.sequence,
            )
        })?;
        let sequence = session.sequence;
        Ok(TerminalInputResult {
            session_id: request.session_id,
            server_sequence: sequence,
            frame_sequence: sequence,
            correlation_id: request.correlation_id,
        })
    }

    async fn send_key(
        &self,
        request: TerminalKeyInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        let bytes = key_input_bytes(request.key_code, request.modifiers, request.pressed);
        if !bytes.is_empty() {
            session.pty.write_input(&bytes).map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    session.sequence,
                )
            })?;
        }
        let sequence = session.sequence;
        Ok(TerminalInputResult {
            session_id: request.session_id,
            server_sequence: sequence,
            frame_sequence: sequence,
            correlation_id: request.correlation_id,
        })
    }

    async fn send_mouse(
        &self,
        request: TerminalMouseInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        let bytes = mouse_input_bytes(
            &session.terminal,
            request.x,
            request.y,
            request.buttons,
            request.button,
            request.pressed,
            request.motion,
            request.wheel_x,
            request.wheel_y,
        );
        if !bytes.is_empty() {
            session.pty.write_input(&bytes).map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    session.sequence,
                )
            })?;
        }
        let sequence = session.sequence;
        Ok(TerminalInputResult {
            session_id: request.session_id,
            server_sequence: sequence,
            frame_sequence: sequence,
            correlation_id: request.correlation_id,
        })
    }

    async fn snapshot(
        &self,
        request: TerminalSnapshotRequest,
    ) -> Result<TerminalSnapshot, TerminalError> {
        let snapshot_started = Instant::now();
        let runtime = self.session(&request.session_id)?;
        let session = Self::lock_session(&runtime)?;
        let pty_drain_micros = 0;
        let sequence = session.sequence;
        if request.after_sequence > 0 && request.after_sequence >= sequence {
            let snapshot_total_micros = snapshot_started.elapsed().as_micros() as u64;
            let resources = runtime.explicit_render_resource_stats();
            tracing::debug!(
                terminal_event = "snapshot_empty",
                session_id = %request.session_id,
                client_sequence = request.client_sequence,
                correlation_id = %request.correlation_id,
                after_sequence = request.after_sequence,
                server_sequence = sequence,
                pty_drain_micros,
                snapshot_total_micros,
                "terminal snapshot has no newer sequence"
            );
            self.record_snapshot_timing(TerminalSnapshotTimingWitness {
                request_sequence: request.client_sequence,
                server_sequence: sequence,
                correlation_id: request.correlation_id.clone(),
                logical_columns: session.surface.columns,
                logical_rows: session.surface.rows,
                panel_width: session.surface.panel_width,
                panel_height: session.surface.panel_height,
                frame_width: 0,
                frame_height: 0,
                cell_width: 0,
                cell_height: 0,
                font_pixel_size: 0,
                payload_bytes: 0,
                pty_drain_us: pty_drain_micros,
                terminal_snapshot_us: 0,
                font_load_us: 0,
                raster_us: 0,
                png_encode_us: 0,
                total_us: snapshot_total_micros,
                font_catalog_acquisitions: resources.font_catalog_acquisitions,
                process_font_catalog_loads: resources.process_font_catalog_loads,
                font_renderer_cache_hits: resources.font_renderer_cache_hits,
                font_renderer_cache_misses: resources.font_renderer_cache_misses,
                font_renderer_constructions: resources.font_renderer_constructions,
                font_renderer_evictions: resources.font_renderer_evictions,
                font_renderer_cache_len: resources.font_renderer_cache_len,
                font_renderer_cache_capacity: resources.font_renderer_cache_capacity,
                glyph_cache_hits: resources.glyph_cache_hits,
                glyph_cache_misses: resources.glyph_cache_misses,
                glyph_cache_evictions: resources.glyph_cache_evictions,
                cached_glyphs: resources.cached_glyphs,
                glyph_cache_capacity: resources.glyph_cache_capacity,
                frame_buffer_grows: resources.frame_buffer_grows,
                frame_buffer_reuses: resources.frame_buffer_reuses,
                frame_buffer_capacity_bytes: resources.frame_buffer_capacity_bytes,
                png_buffer_grows: resources.png_buffer_grows,
                png_buffer_reuses: resources.png_buffer_reuses,
                png_buffer_capacity_bytes: resources.png_buffer_capacity_bytes,
                transport_payload_copies: resources.transport_payload_copies,
            });
            return Ok(TerminalSnapshot {
                session_id: request.session_id,
                sequence,
                request_sequence: request.client_sequence,
                logical_columns: session.surface.columns,
                logical_rows: session.surface.rows,
                width: 0,
                height: 0,
                panel_width: session.surface.panel_width,
                panel_height: session.surface.panel_height,
                cell_width: 0,
                cell_height: 0,
                font_pixel_size: 0,
                correlation_id: request.correlation_id,
                encoding: TerminalFrameEncoding::Png,
                kind: TerminalFrameKind::Full,
                stride: 0,
                tile_x: 0,
                tile_y: 0,
                tile_width: 0,
                tile_height: 0,
                payload: Vec::new(),
                complete: false,
                cursor: TerminalCursor {
                    x: 0,
                    y: 0,
                    visible: false,
                },
                selection_present: false,
                selection: TerminalSelection {
                    anchor_x: 0,
                    anchor_y: 0,
                    focus_x: 0,
                    focus_y: 0,
                },
                prompt: TerminalPromptMetadata {
                    prompt_present: false,
                    prompt: Self::empty_range(),
                    command_present: false,
                    command: Self::empty_range(),
                    command_status_present: false,
                    command_status: 0,
                },
                timing: TerminalSnapshotTiming {
                    pty_drain_us: pty_drain_micros as i64,
                    terminal_snapshot_us: 0,
                    font_load_us: 0,
                    raster_us: 0,
                    png_encode_us: 0,
                    total_us: snapshot_total_micros as i64,
                },
            });
        }
        let terminal_snapshot_started = Instant::now();
        let terminal_snapshot = session.terminal.snapshot();
        let terminal_snapshot_micros = terminal_snapshot_started.elapsed().as_micros() as u64;
        let surface = session.surface.clone();
        let panel_width = surface.panel_width;
        let panel_height = surface.panel_height;
        drop(session);
        let mut render_resources = runtime.take_explicit_render_resources();
        let encoded = render_resources.encode_png(&terminal_snapshot, panel_width, panel_height);
        runtime.return_explicit_render_resources(render_resources);
        let encoded = encoded.map_err(|error| {
            Self::error(TerminalErrorCode::Internal, error.to_string(), sequence)
        })?;
        let EncodedPng {
            width,
            height,
            stride,
            payload,
            font_load_micros,
            raster_micros,
            png_encode_micros,
            cell_width,
            cell_height,
            font_pixel_size,
            resource_stats,
        } = encoded;
        let max_bytes = request.max_frame_bytes.min(MAX_FRAME_BYTES);
        if max_bytes == 0 || payload.len() > max_bytes as usize {
            return Err(Self::error(
                TerminalErrorCode::CapacityExceeded,
                "PNG snapshot exceeds requested frame bound",
                sequence,
            ));
        }
        let snapshot_total_micros = snapshot_started.elapsed().as_micros() as u64;
        tracing::debug!(
            terminal_event = "snapshot_timing",
            session_id = %request.session_id,
            client_sequence = request.client_sequence,
            correlation_id = %request.correlation_id,
            after_sequence = request.after_sequence,
            server_sequence = sequence,
            logical_columns = terminal_snapshot.columns,
            logical_rows = terminal_snapshot.rows,
            frame_width = width,
            frame_height = height,
            stride,
            payload_bytes = payload.len(),
            pty_drain_micros,
            terminal_snapshot_micros,
            font_load_micros,
            raster_micros,
            png_encode_micros,
            snapshot_total_micros,
            font_catalog_acquisitions = resource_stats.font_catalog_acquisitions,
            process_font_catalog_loads = resource_stats.process_font_catalog_loads,
            font_renderer_cache_hits = resource_stats.font_renderer_cache_hits,
            font_renderer_cache_misses = resource_stats.font_renderer_cache_misses,
            font_renderer_constructions = resource_stats.font_renderer_constructions,
            font_renderer_evictions = resource_stats.font_renderer_evictions,
            font_renderer_cache_len = resource_stats.font_renderer_cache_len,
            glyph_cache_hits = resource_stats.glyph_cache_hits,
            glyph_cache_misses = resource_stats.glyph_cache_misses,
            glyph_cache_evictions = resource_stats.glyph_cache_evictions,
            cached_glyphs = resource_stats.cached_glyphs,
            glyph_cache_capacity = resource_stats.glyph_cache_capacity,
            frame_buffer_grows = resource_stats.frame_buffer_grows,
            frame_buffer_reuses = resource_stats.frame_buffer_reuses,
            frame_buffer_capacity_bytes = resource_stats.frame_buffer_capacity_bytes,
            png_buffer_grows = resource_stats.png_buffer_grows,
            png_buffer_reuses = resource_stats.png_buffer_reuses,
            png_buffer_capacity_bytes = resource_stats.png_buffer_capacity_bytes,
            transport_payload_copies = resource_stats.transport_payload_copies,
            "terminal snapshot timing"
        );
        self.record_snapshot_timing(TerminalSnapshotTimingWitness {
            request_sequence: request.client_sequence,
            server_sequence: sequence,
            correlation_id: request.correlation_id.clone(),
            logical_columns: terminal_snapshot.columns,
            logical_rows: terminal_snapshot.rows,
            panel_width: surface.panel_width,
            panel_height: surface.panel_height,
            frame_width: width,
            frame_height: height,
            cell_width,
            cell_height,
            font_pixel_size,
            payload_bytes: payload.len() as u64,
            pty_drain_us: pty_drain_micros,
            terminal_snapshot_us: terminal_snapshot_micros,
            font_load_us: font_load_micros,
            raster_us: raster_micros,
            png_encode_us: png_encode_micros,
            total_us: snapshot_total_micros,
            font_catalog_acquisitions: resource_stats.font_catalog_acquisitions,
            process_font_catalog_loads: resource_stats.process_font_catalog_loads,
            font_renderer_cache_hits: resource_stats.font_renderer_cache_hits,
            font_renderer_cache_misses: resource_stats.font_renderer_cache_misses,
            font_renderer_constructions: resource_stats.font_renderer_constructions,
            font_renderer_evictions: resource_stats.font_renderer_evictions,
            font_renderer_cache_len: resource_stats.font_renderer_cache_len,
            font_renderer_cache_capacity: resource_stats.font_renderer_cache_capacity,
            glyph_cache_hits: resource_stats.glyph_cache_hits,
            glyph_cache_misses: resource_stats.glyph_cache_misses,
            glyph_cache_evictions: resource_stats.glyph_cache_evictions,
            cached_glyphs: resource_stats.cached_glyphs,
            glyph_cache_capacity: resource_stats.glyph_cache_capacity,
            frame_buffer_grows: resource_stats.frame_buffer_grows,
            frame_buffer_reuses: resource_stats.frame_buffer_reuses,
            frame_buffer_capacity_bytes: resource_stats.frame_buffer_capacity_bytes,
            png_buffer_grows: resource_stats.png_buffer_grows,
            png_buffer_reuses: resource_stats.png_buffer_reuses,
            png_buffer_capacity_bytes: resource_stats.png_buffer_capacity_bytes,
            transport_payload_copies: resource_stats.transport_payload_copies,
        });
        Ok(TerminalSnapshot {
            session_id: request.session_id,
            sequence,
            request_sequence: request.client_sequence,
            logical_columns: terminal_snapshot.columns,
            logical_rows: terminal_snapshot.rows,
            width,
            height,
            panel_width: surface.panel_width,
            panel_height: surface.panel_height,
            cell_width,
            cell_height,
            font_pixel_size,
            correlation_id: request.correlation_id,
            encoding: TerminalFrameEncoding::Png,
            kind: TerminalFrameKind::Full,
            stride,
            tile_x: 0,
            tile_y: 0,
            tile_width: width,
            tile_height: height,
            payload,
            complete: true,
            cursor: TerminalCursor {
                x: 0,
                y: 0,
                visible: false,
            },
            selection_present: false,
            selection: TerminalSelection {
                anchor_x: 0,
                anchor_y: 0,
                focus_x: 0,
                focus_y: 0,
            },
            prompt: Self::prompt_metadata(&terminal_snapshot.prompt),
            timing: TerminalSnapshotTiming {
                pty_drain_us: pty_drain_micros as i64,
                terminal_snapshot_us: terminal_snapshot_micros as i64,
                font_load_us: font_load_micros as i64,
                raster_us: raster_micros as i64,
                png_encode_us: png_encode_micros as i64,
                total_us: snapshot_total_micros as i64,
            },
        })
    }

    async fn subscribe_frames(
        &self,
        request: TerminalSubscribeRequest,
        frames: vox::Tx<TerminalFrameEvent>,
    ) -> Result<TerminalOperationResult, TerminalError> {
        if request.max_frame_bytes == 0 {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                "subscription max_frame_bytes must be non-zero",
                0,
            ));
        }
        let runtime = self.session(&request.session_id)?;
        if runtime.closed.load(Ordering::Acquire) {
            return Err(Self::error(
                TerminalErrorCode::Disconnected,
                "terminal session is closed",
                runtime.sequence(),
            ));
        }
        let (subscriber_id, mut signals) = runtime.publication.register(runtime.sequence());
        let _subscription = SubscriptionGuard {
            runtime: Arc::downgrade(&runtime),
            subscriber_id,
            cancellation: None,
        };
        if runtime.closed.load(Ordering::Acquire) {
            return Err(Self::error(
                TerminalErrorCode::Disconnected,
                "terminal session closed while the subscription was opening",
                runtime.sequence(),
            ));
        }
        let mut resources = TerminalRenderResources::default();
        let mut counters = PublicationCounters::default();
        let mut first = true;
        let mut last_terminal_sequence = request.after_terminal_sequence;
        let mut last_mutations = 0_i64;
        let mut frame_sequence = 0_i64;

        loop {
            let signal = if first {
                *signals.borrow_and_update()
            } else {
                let changed = tokio::select! {
                    _ = frames.closed() => break,
                    changed = signals.changed() => changed,
                };
                if changed.is_err() {
                    break;
                }
                *signals.borrow_and_update()
            };
            if !first && signal.terminal_sequence <= last_terminal_sequence {
                continue;
            }

            let capture = Self::capture_terminal(&runtime)?;
            let coalesced_this_frame = if first {
                0
            } else {
                capture
                    .mutations
                    .max(signal.mutations)
                    .saturating_sub(last_mutations)
                    .saturating_sub(1)
            };
            counters.pre_render_coalesced = counters
                .pre_render_coalesced
                .saturating_add(coalesced_this_frame);
            counters.renders_started = counters.renders_started.saturating_add(1);
            let session_id = request.session_id.clone();
            let correlation_id = request.correlation_id.clone();
            let request_sequence = request.client_sequence;
            let max_frame_bytes = request.max_frame_bytes;
            let mut render_worker = tokio::task::spawn_blocking(move || {
                let result = Self::render_captured(
                    &mut resources,
                    &session_id,
                    &capture,
                    request_sequence,
                    &correlation_id,
                    max_frame_bytes,
                );
                (resources, capture, result)
            });
            let (render, receiver_closed) = tokio::select! {
                result = &mut render_worker => (result, false),
                _ = frames.closed() => (render_worker.await, true),
            };
            let render = render.map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    format!("terminal render worker failed: {error}"),
                    runtime.sequence(),
                )
            })?;
            resources = render.0;
            let capture = render.1;
            if receiver_closed {
                break;
            }
            let frame = render.2?;
            counters.renders_completed = counters.renders_completed.saturating_add(1);
            let pending_depth = u8::from(signals.has_changed().unwrap_or(false));
            counters.pending_depth_max = counters.pending_depth_max.max(pending_depth);
            counters.mutation_to_send_us =
                i64::try_from(signal.mutation_at.elapsed().as_micros()).unwrap_or(i64::MAX);
            frame_sequence = frame_sequence.saturating_add(1).max(1);
            let full_resync = first || signal.full_resync || coalesced_this_frame > 0;
            let mut event = TerminalFrameEvent {
                session_id: request.session_id.clone(),
                connection_epoch: runtime.connection_epoch.to_string(),
                session_epoch: runtime.session_epoch.to_string(),
                terminal_sequence: capture.terminal_sequence,
                frame_sequence,
                full_resync,
                max_frame_bytes: request.max_frame_bytes.min(MAX_FRAME_BYTES),
                backend_id: "rust.cpu.fontdue".to_string(),
                transport_id: "vox.txrx".to_string(),
                correlation_id: request.correlation_id.clone(),
                frame,
                publication: counters.telemetry(capture.mutations, pending_depth),
            };

            match frames.try_send(event) {
                Ok(()) => {
                    counters.credit_wait_us = 0;
                }
                Err(vox::TrySendError::Full(returned)) => {
                    event = returned;
                    // Facet's typed Tx deliberately collapses exhausted credit
                    // and a full bounded runtime conduit into `Full`; either
                    // outcome requires this publication to await Vox
                    // backpressure rather than growing another frame queue.
                    counters.credit_blocked_sends = counters.credit_blocked_sends.saturating_add(1);
                    event.publication.credit_blocked_sends = counters.credit_blocked_sends;
                    let credit_wait_started = Instant::now();
                    if frames.send(event).await.is_err() {
                        break;
                    }
                    counters.credit_wait_us =
                        i64::try_from(credit_wait_started.elapsed().as_micros())
                            .unwrap_or(i64::MAX);
                }
                Err(vox::TrySendError::Closed(_)) => break,
            }
            counters.frames_pushed = counters.frames_pushed.saturating_add(1);
            last_terminal_sequence = capture.terminal_sequence;
            last_mutations = capture.mutations;
            first = false;
            if runtime.closed.load(Ordering::Acquire) {
                break;
            }
        }

        let _ = frames.close(vox::Metadata::default()).await;
        Ok(TerminalOperationResult {
            session_id: request.session_id,
            state: if runtime.closed.load(Ordering::Acquire) {
                TerminalState::Closed
            } else {
                TerminalState::Ready
            },
            server_sequence: runtime.sequence(),
        })
    }

    async fn subscribe_raster_frames(
        &self,
        request: TerminalRasterSubscribeRequest,
        frames: vox::Tx<TerminalRasterFrameEvent>,
    ) -> Result<TerminalOperationResult, TerminalError> {
        if request.presentation_generation.trim().is_empty() {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                "raster subscription presentation_generation must not be blank",
                0,
            ));
        }
        if request.max_frame_bytes == 0 {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                "raster subscription max_frame_bytes must be non-zero",
                0,
            ));
        }
        if request.max_regions == 0 {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                "raster subscription max_regions must be non-zero",
                0,
            ));
        }
        let runtime = self.session(&request.session_id)?;
        let presentation =
            RasterPresentation::from_request(&request, &self.gpu_registry, runtime.sequence())?;
        let transport = presentation.transport;
        if runtime.closed.load(Ordering::Acquire) {
            return Err(Self::error(
                TerminalErrorCode::Disconnected,
                "terminal session is closed",
                runtime.sequence(),
            ));
        }
        let max_frame_bytes = request.max_frame_bytes.min(MAX_FRAME_BYTES);
        let max_regions = request.max_regions.min(MAX_DIRTY_REGIONS as u16);
        let (subscriber_id, mut signals) = runtime.publication.register(runtime.sequence());
        let render_cancellation = VulkanOffscreenCancellation::default();
        let _subscription = SubscriptionGuard {
            runtime: Arc::downgrade(&runtime),
            subscriber_id,
            cancellation: Some(render_cancellation.clone()),
        };
        runtime
            .raster_render_cancellations
            .lock()
            .map_err(|_| {
                Self::error(
                    TerminalErrorCode::Internal,
                    "terminal raster cancellation registry lock poisoned",
                    runtime.sequence(),
                )
            })?
            .insert(subscriber_id, render_cancellation.clone());
        if runtime.closed.load(Ordering::Acquire) {
            return Err(Self::error(
                TerminalErrorCode::Disconnected,
                "terminal session closed while the raster subscription was opening",
                runtime.sequence(),
            ));
        }

        let lease = tokio::select! {
            lease = runtime.lease_raster_render_resources() => Some(lease?),
            _ = frames.closed() => None,
        };
        let Some(mut resource_lease) = lease else {
            return Ok(TerminalOperationResult {
                session_id: request.session_id,
                state: TerminalState::Ready,
                server_sequence: runtime.sequence(),
            });
        };
        let mut counters = PublicationCounters::default();
        let mut first = true;
        let mut last_terminal_sequence = request.after_terminal_sequence;
        let mut last_mutations = 0_i64;
        let mut frame_sequence = request.after_frame_sequence.max(0);
        let mut last_delivered_snapshot: Option<teamy_terminal_core::TerminalSnapshot> = None;
        let mut last_delivered_frame_sequence = 0_i64;

        loop {
            let signal = if first {
                *signals.borrow_and_update()
            } else {
                let changed = tokio::select! {
                    _ = frames.closed() => break,
                    changed = signals.changed() => changed,
                };
                if changed.is_err() {
                    break;
                }
                *signals.borrow_and_update()
            };
            if !first && signal.terminal_sequence <= last_terminal_sequence {
                continue;
            }

            let capture = Self::capture_terminal(&runtime)?;
            let coalesced_this_frame = if first {
                0
            } else {
                capture
                    .mutations
                    .max(signal.mutations)
                    .saturating_sub(last_mutations)
                    .saturating_sub(1)
            };
            counters.pre_render_coalesced = counters
                .pre_render_coalesced
                .saturating_add(coalesced_this_frame);
            counters.renders_started = counters.renders_started.saturating_add(1);
            let next_frame_sequence = frame_sequence.saturating_add(1).max(1);
            let previous_snapshot = last_delivered_snapshot.clone();
            let force_full = first || signal.full_resync || last_delivered_frame_sequence == 0;
            let base_frame_sequence = if force_full {
                0
            } else {
                last_delivered_frame_sequence
            };
            let panel_width = capture.surface.panel_width;
            let panel_height = capture.surface.panel_height;
            let gpu_identity = GpuSlugRenderIdentity {
                session_sequence: runtime.session_sequence,
                terminal_sequence: u64::try_from(capture.terminal_sequence).unwrap_or(u64::MAX),
                frame_sequence: u64::try_from(next_frame_sequence).unwrap_or(u64::MAX),
                presentation_generation: request.presentation_generation.clone(),
                correlation_id: request.correlation_id.clone(),
            };
            let render_started = Instant::now();
            let mut resources = resource_lease.take();
            let worker_cancellation = render_cancellation.clone();
            let mut render_worker = tokio::task::spawn_blocking(move || {
                let result = resources.render_raster_update(
                    &capture.snapshot,
                    panel_width,
                    panel_height,
                    presentation,
                    previous_snapshot.as_ref(),
                    u64::try_from(next_frame_sequence).unwrap_or(u64::MAX),
                    u64::try_from(base_frame_sequence).unwrap_or(0),
                    force_full,
                    max_regions,
                    gpu_identity,
                    Some(&worker_cancellation),
                );
                (resources, capture, result)
            });
            let (render, receiver_closed) = tokio::select! {
                result = &mut render_worker => (result, false),
                _ = frames.closed() => {
                    render_cancellation.cancel();
                    (render_worker.await, true)
                }
            };
            let render = render.map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    format!("terminal raster worker failed: {error}"),
                    runtime.sequence(),
                )
            })?;
            resource_lease.replace(render.0);
            let capture = render.1;
            if receiver_closed {
                break;
            }
            let Some(rendered) = render
                .2
                .map_err(|error| self.raster_render_error(&error, capture.terminal_sequence))?
            else {
                last_terminal_sequence = capture.terminal_sequence;
                last_mutations = capture.mutations;
                last_delivered_snapshot = Some(capture.snapshot);
                first = false;
                continue;
            };
            counters.renders_completed = counters.renders_completed.saturating_add(1);
            Self::validate_and_record_gpu_evidence(
                &rendered,
                presentation,
                &request,
                &runtime,
                capture.terminal_sequence,
                next_frame_sequence,
            )?;
            if rendered.update.bytes.len() > max_frame_bytes as usize {
                return Err(Self::error(
                    TerminalErrorCode::CapacityExceeded,
                    format!(
                        "{} raster payload has {} bytes, exceeding the negotiated {} byte bound",
                        transport.transport_id(),
                        rendered.update.bytes.len(),
                        max_frame_bytes
                    ),
                    capture.terminal_sequence,
                ));
            }
            let descriptor = rendered.update.descriptor;
            let full_resync = descriptor.kind == RasterUpdateKind::Full && rendered.full_recovery;
            let total_us = i64::try_from(render_started.elapsed().as_micros()).unwrap_or(i64::MAX);
            let frame = Self::raster_frame(&capture, rendered, total_us)?;
            let pending_depth = u8::from(signals.has_changed().unwrap_or(false));
            counters.pending_depth_max = counters.pending_depth_max.max(pending_depth);
            counters.mutation_to_send_us =
                i64::try_from(signal.mutation_at.elapsed().as_micros()).unwrap_or(i64::MAX);
            let mut event = TerminalRasterFrameEvent {
                session_id: request.session_id.clone(),
                connection_epoch: runtime.connection_epoch.to_string(),
                session_epoch: runtime.session_epoch.to_string(),
                presentation_generation: request.presentation_generation.clone(),
                terminal_sequence: capture.terminal_sequence,
                frame_sequence: i64::try_from(descriptor.sequence).unwrap_or(i64::MAX),
                base_frame_sequence: i64::try_from(descriptor.base_sequence).unwrap_or(i64::MAX),
                full_resync,
                renderer_id: presentation.renderer.renderer_id().to_string(),
                damage_mode_id: transport.damage_mode_id().to_string(),
                transport_id: transport.transport_id().to_string(),
                transport_version: RASTER_TRANSPORT_VERSION,
                frame_contract_version: RASTER_FRAME_CONTRACT_VERSION,
                max_frame_bytes,
                max_regions,
                correlation_id: request.correlation_id.clone(),
                frame,
                publication: counters.telemetry(capture.mutations, pending_depth),
            };

            match frames.try_send(event) {
                Ok(()) => {
                    counters.credit_wait_us = 0;
                }
                Err(vox::TrySendError::Full(returned)) => {
                    event = returned;
                    counters.credit_blocked_sends = counters.credit_blocked_sends.saturating_add(1);
                    event.publication.credit_blocked_sends = counters.credit_blocked_sends;
                    let credit_wait_started = Instant::now();
                    if frames.send(event).await.is_err() {
                        break;
                    }
                    counters.credit_wait_us =
                        i64::try_from(credit_wait_started.elapsed().as_micros())
                            .unwrap_or(i64::MAX);
                }
                Err(vox::TrySendError::Closed(_)) => break,
            }
            counters.frames_pushed = counters.frames_pushed.saturating_add(1);
            frame_sequence = next_frame_sequence;
            last_delivered_frame_sequence = next_frame_sequence;
            last_terminal_sequence = capture.terminal_sequence;
            last_mutations = capture.mutations;
            last_delivered_snapshot = Some(capture.snapshot);
            first = false;
            if runtime.closed.load(Ordering::Acquire) {
                break;
            }
        }

        let _ = frames.close(vox::Metadata::default()).await;
        Ok(TerminalOperationResult {
            session_id: request.session_id,
            state: if runtime.closed.load(Ordering::Acquire) {
                TerminalState::Closed
            } else {
                TerminalState::Ready
            },
            server_sequence: runtime.sequence(),
        })
    }

    async fn get_content(
        &self,
        request: TerminalContentRequest,
    ) -> Result<TerminalContentResult, TerminalError> {
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        if request.max_chars == 0 {
            return Err(Self::error(
                TerminalErrorCode::InvalidRequest,
                "content max_chars must be non-zero",
                session.sequence,
            ));
        }
        let sequence = session.sequence;
        if request.after_sequence > 0 && request.after_sequence >= sequence {
            return Ok(TerminalContentResult {
                session_id: request.session_id,
                sequence,
                text: String::new(),
                complete: false,
                truncated: false,
                prompt: Self::empty_prompt(),
            });
        }
        let terminal_snapshot = session.terminal.snapshot();
        let text = terminal_snapshot.visible_text();
        let max_chars = request.max_chars.min(MAX_CONTENT_CHARS) as usize;
        let truncated = text.chars().count() > max_chars;
        let text = text.chars().take(max_chars).collect();
        Ok(TerminalContentResult {
            session_id: request.session_id,
            sequence,
            text,
            complete: true,
            truncated,
            prompt: Self::prompt_metadata(&terminal_snapshot.prompt),
        })
    }

    async fn cancel(
        &self,
        request: TerminalCancelRequest,
    ) -> Result<TerminalOperationResult, TerminalError> {
        let session = self.session(&request.session_id)?;
        let session = Self::lock_session(&session)?;
        // Cancellation is a terminal input operation, not merely a state
        // acknowledgement. Ctrl+C is deliberately encoded through the same
        // generic key path as a client key event so every terminal control
        // sequence retains one source of truth.
        let interrupt_down = key_input_bytes(67, GLFW_MOD_CONTROL, true);
        session.pty.write_input(&interrupt_down).map_err(|error| {
            Self::error(
                TerminalErrorCode::Internal,
                error.to_string(),
                session.sequence,
            )
        })?;
        std::thread::sleep(Duration::from_millis(10));
        let interrupt_up = key_input_bytes(67, GLFW_MOD_CONTROL, false);
        session.pty.write_input(&interrupt_up).map_err(|error| {
            Self::error(
                TerminalErrorCode::Internal,
                error.to_string(),
                session.sequence,
            )
        })?;
        let sequence = session.sequence;
        Ok(TerminalOperationResult {
            session_id: request.session_id,
            state: TerminalState::Ready,
            server_sequence: sequence,
        })
    }

    async fn disconnect(
        &self,
        request: TerminalDisconnectRequest,
    ) -> Result<TerminalOperationResult, TerminalError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session registry lock poisoned",
                0,
            )
        })?;
        let session = sessions.get(&request.session_id).cloned().ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        if session.connection_epoch != self.connection_epoch {
            return Err(Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found for this connection",
                0,
            ));
        }
        sessions.remove(&request.session_id);
        drop(sessions);
        let sequence = {
            let mut state = Self::lock_session(&session)?;
            state.closed = true;
            Self::next_sequence(&mut state.sequence)
        };
        session.terminal_sequence.store(sequence, Ordering::Release);
        session.shutdown();
        Ok(TerminalOperationResult {
            session_id: request.session_id,
            state: TerminalState::Closed,
            server_sequence: sequence,
        })
    }
}

/// Serve the Rust-authoritative terminal endpoint until the process exits.
pub async fn serve(listen: &str) -> Result<()> {
    let listen = normalize_listen_address(listen)?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("failed to bind Vox terminal endpoint at {listen}"))?;
    let address = listener
        .local_addr()
        .context("failed to read Vox endpoint address")?;
    println!("VOX_TERMINAL_LISTEN_ADDR={address}");
    let service = VoxTerminalService::new();
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("failed to accept Vox terminal connection")?;
        eprintln!("VOX_TERMINAL_CONNECTION_ACCEPTED peer={peer}");
        stream.set_nodelay(true).ok();
        let service = service.for_connection();
        tokio::spawn(async move {
            let dispatcher_service = service.clone();
            let result = vox::acceptor_on(vox::transport::tcp::StreamLink::tcp(stream))
                .on_lane(TerminalDispatcher::new(dispatcher_service))
                .establish_connection()
                .await;
            match result {
                Ok(connection) => {
                    eprintln!("VOX_TERMINAL_CONNECTION_ESTABLISHED peer={peer}");
                    connection.closed().await;
                    eprintln!("VOX_TERMINAL_CONNECTION_CLOSED peer={peer}");
                    connection.shutdown().ok();
                    service.close_connection_sessions();
                }
                Err(error) => {
                    eprintln!("VOX_TERMINAL_CONNECTION_FAILED peer={peer} error={error:?}");
                    tracing::warn!(?peer, ?error, "Vox terminal connection failed");
                    service.close_connection_sessions();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec_proto::terminal::TerminalClient;
    use teamy_terminal_core::Color;
    use teamy_terminal_frame::ComposedRasterFrame;

    fn own_frame(event: vox::SelfRef<TerminalFrameEvent>) -> TerminalFrameEvent {
        let json = event.map(|event| {
            facet_json::to_string(&event).expect("pushed frame should serialize in its own test")
        });
        facet_json::from_str(json.get()).expect("pushed frame test JSON should decode")
    }

    fn own_raster_frame(event: vox::SelfRef<TerminalRasterFrameEvent>) -> TerminalRasterFrameEvent {
        let json = event.map(|event| {
            facet_json::to_string(&event)
                .expect("pushed raster frame should serialize in its own test")
        });
        facet_json::from_str(json.get()).expect("pushed raster frame test JSON should decode")
    }

    fn test_connect_request(endpoint: &str, client_sequence: i64) -> TerminalConnectRequest {
        TerminalConnectRequest {
            endpoint: endpoint.to_string(),
            requested_width: 80,
            requested_height: 24,
            surface: TerminalSurfaceMetrics {
                columns: 80,
                rows: 24,
                panel_width: 800,
                panel_height: 480,
                ..TerminalSurfaceMetrics::default()
            },
            capabilities: TerminalCapabilities {
                text_input: true,
                key_input: true,
                mouse_input: true,
                structured_cells: false,
                raster_frames: true,
                selection: false,
                scrollback: true,
                dirty_frames: false,
                max_width: MAX_COLUMNS,
                max_height: MAX_ROWS,
                max_frame_bytes: MAX_FRAME_BYTES,
                backend_id: String::new(),
                transport_id: String::new(),
            },
            client_sequence,
            correlation_id: format!("connect-{client_sequence}"),
        }
    }

    fn test_raster_request(transport: RasterTransport) -> TerminalRasterSubscribeRequest {
        TerminalRasterSubscribeRequest {
            session_id: "session-01".to_string(),
            requested_renderer_id: CPU_RENDERER_ID.to_string(),
            requested_damage_mode_id: transport.damage_mode_id().to_string(),
            requested_transport_id: transport.transport_id().to_string(),
            requested_transport_version: RASTER_TRANSPORT_VERSION,
            presentation_generation: "generation-01".to_string(),
            after_terminal_sequence: 0,
            after_frame_sequence: 0,
            max_frame_bytes: MAX_FRAME_BYTES,
            max_regions: MAX_DIRTY_REGIONS as u16,
            client_sequence: 1,
            correlation_id: "raster-01".to_string(),
        }
    }

    fn test_gpu_identity(frame_sequence: u64) -> GpuSlugRenderIdentity {
        GpuSlugRenderIdentity {
            session_sequence: 7,
            terminal_sequence: frame_sequence,
            frame_sequence,
            presentation_generation: "gpu-test-generation".to_string(),
            correlation_id: format!("gpu-test-{frame_sequence}"),
        }
    }

    fn service_with_unavailable_gpu(message: &str) -> VoxTerminalService {
        let mut service = VoxTerminalService::new();
        service.gpu_registry = Arc::new(GpuRendererRegistry {
            state: Mutex::new(GpuRendererState::Unavailable(CachedGpuError {
                code: TerminalErrorCode::UnsupportedCapability,
                message: message.to_string(),
                retryable: false,
            })),
        });
        service
    }

    #[test]
    fn normalize_listen_address_accepts_loopback_shorthand() {
        assert_eq!(
            normalize_listen_address(":63946").unwrap(),
            "127.0.0.1:63946"
        );
        assert_eq!(
            normalize_listen_address("63946").unwrap(),
            "127.0.0.1:63946"
        );
        assert_eq!(
            normalize_listen_address("localhost:63946").unwrap(),
            "localhost:63946"
        );
    }

    #[test]
    fn raster_capabilities_and_requests_use_stable_exact_combinations() {
        let unavailable_message = "rust-gpu-slug unavailable in deterministic test fixture";
        let service = service_with_unavailable_gpu(unavailable_message);
        let (modes, unavailable) = service
            .presentation_capability_sets(17)
            .expect("deterministic capability snapshot");
        assert_eq!(modes.len(), 3);
        assert_eq!(modes[0].transport_id, TRANSPORT_FULL_PNG);
        assert_eq!(modes[1].transport_id, TRANSPORT_FULL_RAW_RGBA);
        assert_eq!(modes[2].transport_id, TRANSPORT_DIRTY_RAW_RGBA);
        assert_eq!(modes[2].damage_mode_id, DAMAGE_DIRTY);
        assert_eq!(
            modes[2].steady_frame_kind,
            TerminalRasterFrameKind::DirtyRegions
        );
        for transport in [
            RasterTransport::FullPng,
            RasterTransport::FullRawRgba,
            RasterTransport::DirtyRawRgba,
        ] {
            assert_eq!(
                RasterPresentation::from_request(
                    &test_raster_request(transport),
                    &service.gpu_registry,
                    17,
                ),
                Ok(RasterPresentation {
                    renderer: RasterRenderer::CpuFontdue,
                    transport,
                })
            );
        }
        let mut unsupported = test_raster_request(RasterTransport::FullPng);
        unsupported.requested_damage_mode_id = DAMAGE_DIRTY.to_string();
        assert_eq!(
            RasterPresentation::from_request(&unsupported, &service.gpu_registry, 17)
                .expect_err("a mismatched damage mode must not silently fall back")
                .code,
            TerminalErrorCode::UnsupportedCapability
        );
        unsupported = test_raster_request(RasterTransport::FullPng);
        unsupported.requested_renderer_id = GPU_RENDERER_ID.to_string();
        let error = RasterPresentation::from_request(&unsupported, &service.gpu_registry, 17)
            .expect_err("an unavailable GPU renderer must not silently fall back to CPU");
        assert_eq!(error.code, TerminalErrorCode::UnsupportedCapability);
        assert_eq!(error.message, unavailable_message);
        assert_eq!(error.server_sequence, 17);

        assert_eq!(unavailable.len(), 3);
        assert!(unavailable.iter().all(|mode| {
            mode.renderer_id == GPU_RENDERER_ID
                && mode.rasterization_owner == TerminalRasterizationOwner::Server
                && mode.error.code == TerminalErrorCode::UnsupportedCapability
                && mode.error.message == unavailable_message
                && mode.error.server_sequence == 17
        }));
    }

    #[test]
    fn process_gpu_capability_is_six_valid_modes_or_three_exact_negatives() {
        let service = VoxTerminalService::new();
        let (modes, unavailable) = service
            .presentation_capability_sets(23)
            .expect("process capability snapshot");
        let cpu_modes = modes
            .iter()
            .filter(|mode| mode.renderer_id == CPU_RENDERER_ID)
            .count();
        let gpu_modes = modes
            .iter()
            .filter(|mode| mode.renderer_id == GPU_RENDERER_ID)
            .count();
        assert_eq!(cpu_modes, 3);

        if unavailable.is_empty() {
            assert_eq!(modes.len(), 6);
            assert_eq!(gpu_modes, 3);
            assert!(unavailable.is_empty());
        } else {
            assert_eq!(modes.len(), 3);
            assert_eq!(gpu_modes, 0);
            assert_eq!(unavailable.len(), 3);
            assert!(unavailable.iter().all(|mode| {
                mode.renderer_id == GPU_RENDERER_ID
                    && mode.rasterization_owner == TerminalRasterizationOwner::Server
                    && mode.error.code == TerminalErrorCode::UnsupportedCapability
                    && mode.error.server_sequence == 23
            }));
        }
    }

    #[test]
    fn first_device_loss_uses_the_same_typed_error_as_later_capabilities() {
        let service = service_with_unavailable_gpu("fixture before simulated device loss");
        let report = eyre::Report::new(GpuSlugRendererError::Vulkan(
            VulkanOffscreenError::DeviceLost {
                operation: "test dispatch",
            },
        ));

        let first = service.raster_render_error(&report, 41);
        assert_eq!(first.code, TerminalErrorCode::UnsupportedCapability);
        assert!(!first.retryable);
        assert_eq!(first.server_sequence, 41);
        assert!(first.message.contains("device loss during test dispatch"));

        let (modes, unavailable) = service
            .presentation_capability_sets(42)
            .expect("post-loss capability snapshot");
        assert_eq!(modes.len(), 3);
        assert_eq!(unavailable.len(), 3);
        assert!(unavailable.iter().all(|mode| {
            mode.error.code == first.code
                && mode.error.message == first.message
                && mode.error.retryable == first.retryable
                && mode.error.server_sequence == 42
        }));
    }

    #[test]
    fn gpu_slug_bridge_emits_all_transports_and_exact_dirty_composition_when_available() {
        let service = VoxTerminalService::new();
        let Ok(runtime) = service.gpu_registry.runtime(31) else {
            let (_, unavailable) = service
                .presentation_capability_sets(31)
                .expect("unavailable process capability snapshot");
            assert_eq!(unavailable.len(), 3);
            return;
        };
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 12, 3);
        terminal.process_bytes(b"\x1b[?25l\x1b[1;36mhello\x1b[0m");
        let initial_snapshot = terminal.snapshot();

        for (frame_sequence, transport) in [
            RasterTransport::FullPng,
            RasterTransport::FullRawRgba,
            RasterTransport::DirtyRawRgba,
        ]
        .into_iter()
        .enumerate()
        {
            let frame_sequence = u64::try_from(frame_sequence + 1).expect("small test sequence");
            let mut resources = TerminalRenderResources::with_gpu_runtime(Some(runtime.clone()));
            let rendered = resources
                .render_raster_update(
                    &initial_snapshot,
                    0,
                    0,
                    RasterPresentation {
                        renderer: RasterRenderer::GpuSlug,
                        transport,
                    },
                    None,
                    frame_sequence,
                    0,
                    true,
                    MAX_DIRTY_REGIONS as u16,
                    test_gpu_identity(frame_sequence),
                    None,
                )
                .expect("GPU presentation should render")
                .expect("forced GPU presentation should emit a frame");
            rendered
                .update
                .validate()
                .expect("valid GPU frame contract");
            assert!(rendered.cell_width > 0);
            assert!(rendered.cell_height > 0);
            assert!(rendered.font_pixel_size > 0);
            let evidence = rendered
                .gpu_evidence
                .as_ref()
                .expect("GPU render must preserve evidence through the bridge adapter");
            assert_eq!(evidence.identity, test_gpu_identity(frame_sequence));
            assert!(evidence.stats.requested_pixels > 0);
            assert!(evidence.stats.target_allocation_id > 0);
            assert_eq!(rendered.update.descriptor.kind, RasterUpdateKind::Full);
            match transport {
                RasterTransport::FullPng => {
                    assert_eq!(
                        rendered.update.descriptor.compression,
                        FrameCompression::Png
                    );
                    assert!(rendered.update.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
                }
                RasterTransport::FullRawRgba | RasterTransport::DirtyRawRgba => {
                    assert_eq!(
                        rendered.update.descriptor.compression,
                        FrameCompression::Raw
                    );
                    assert_eq!(
                        rendered.update.bytes.len(),
                        usize::try_from(
                            rendered
                                .update
                                .descriptor
                                .width
                                .saturating_mul(rendered.update.descriptor.height)
                                .saturating_mul(4)
                        )
                        .expect("small test frame length")
                    );
                }
            }
        }

        let mut resources = TerminalRenderResources::with_gpu_runtime(Some(runtime));
        let dirty_presentation = RasterPresentation {
            renderer: RasterRenderer::GpuSlug,
            transport: RasterTransport::DirtyRawRgba,
        };
        let initial = resources
            .render_raster_update(
                &initial_snapshot,
                0,
                0,
                dirty_presentation,
                None,
                40,
                0,
                true,
                MAX_DIRTY_REGIONS as u16,
                test_gpu_identity(40),
                None,
            )
            .expect("initial GPU dirty-mode render")
            .expect("initial GPU dirty-mode render is a full resync");
        let mut composed =
            ComposedRasterFrame::from_full(&initial.update).expect("compose GPU full frame");

        terminal.process_bytes(b"\x1b[1;1HZ\x1b[3;12H!");
        let current_snapshot = terminal.snapshot();
        let dirty = resources
            .render_raster_update(
                &current_snapshot,
                0,
                0,
                dirty_presentation,
                Some(&initial_snapshot),
                41,
                40,
                false,
                MAX_DIRTY_REGIONS as u16,
                test_gpu_identity(41),
                None,
            )
            .expect("incremental GPU render")
            .expect("visible GPU mutation emits dirty regions");
        assert_eq!(dirty.update.descriptor.kind, RasterUpdateKind::DirtyRegions);
        assert!(!dirty.update.regions.is_empty());
        composed
            .apply(&dirty.update)
            .expect("compose packed GPU dirty regions");

        let expected = resources
            .render_raster_update(
                &current_snapshot,
                0,
                0,
                RasterPresentation {
                    renderer: RasterRenderer::GpuSlug,
                    transport: RasterTransport::FullRawRgba,
                },
                Some(&initial_snapshot),
                42,
                41,
                true,
                MAX_DIRTY_REGIONS as u16,
                test_gpu_identity(42),
                None,
            )
            .expect("GPU full-frame comparison render")
            .expect("forced GPU comparison frame");
        assert_eq!(composed.pixels, expected.update.bytes);
    }

    #[test]
    fn dirty_raster_update_composes_to_the_exact_retained_full_frame() {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 12, 3);
        terminal.process_bytes(b"\x1b[1;36mhello\x1b[0m");
        let previous = terminal.snapshot();
        let mut resources = TerminalRenderResources::default();
        let full = resources
            .render_cpu_raster_update(
                &previous,
                0,
                0,
                RasterTransport::DirtyRawRgba,
                None,
                1,
                0,
                true,
                MAX_DIRTY_REGIONS as u16,
            )
            .expect("initial raster render")
            .expect("initial render is a complete resync");
        assert_eq!(full.update.descriptor.kind, RasterUpdateKind::Full);
        assert!(full.full_recovery);
        let mut composed =
            ComposedRasterFrame::from_full(&full.update).expect("compose full frame");

        terminal.process_bytes(b"\x1b[2D!\x1b[4m?\x1b[0m");
        let current = terminal.snapshot();
        let dirty = resources
            .render_cpu_raster_update(
                &current,
                0,
                0,
                RasterTransport::DirtyRawRgba,
                Some(&previous),
                2,
                1,
                false,
                MAX_DIRTY_REGIONS as u16,
            )
            .expect("incremental raster render")
            .expect("visible mutation emits a dirty update");
        assert_eq!(dirty.update.descriptor.kind, RasterUpdateKind::DirtyRegions);
        assert!(!dirty.full_recovery);
        assert_eq!(dirty.update.descriptor.base_sequence, 1);
        composed.apply(&dirty.update).expect("compose dirty update");
        assert_eq!(composed.pixels, resources.frame.pixels);
    }

    #[test]
    fn negotiated_dirty_region_overflow_falls_back_to_a_full_raw_resync() {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 12, 4);
        terminal.process_bytes(b"\x1b[?25l");
        let previous = terminal.snapshot();
        let mut resources = TerminalRenderResources::default();
        resources
            .render_cpu_raster_update(
                &previous,
                0,
                0,
                RasterTransport::DirtyRawRgba,
                None,
                1,
                0,
                true,
                1,
            )
            .expect("initial raster render")
            .expect("initial render is a complete resync");

        terminal.process_bytes(b"\x1b[1;1HA\x1b[4;12HZ");
        let current = terminal.snapshot();
        let (regions, full_refresh) = damage_regions(Some(&previous), &current);
        assert!(!full_refresh);
        assert!(
            regions.len() > 1,
            "fixture must exceed the negotiated bound"
        );

        let fallback = resources
            .render_cpu_raster_update(
                &current,
                0,
                0,
                RasterTransport::DirtyRawRgba,
                Some(&previous),
                2,
                1,
                false,
                1,
            )
            .expect("region overflow should be recoverable")
            .expect("region overflow should publish a full refresh");
        assert_eq!(fallback.update.descriptor.kind, RasterUpdateKind::Full);
        assert_eq!(
            fallback.update.descriptor.compression,
            FrameCompression::Raw
        );
        assert_eq!(fallback.update.descriptor.base_sequence, 0);
        assert!(fallback.update.regions.is_empty());
        assert!(fallback.full_recovery);
    }

    #[test]
    fn ordinary_full_mode_frames_are_not_resyncs_after_the_initial_frame() {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 12, 3);
        terminal.process_bytes(b"\x1b[?25lA");
        let first_snapshot = terminal.snapshot();
        let mut resources = TerminalRenderResources::default();
        let first = resources
            .render_cpu_raster_update(
                &first_snapshot,
                0,
                0,
                RasterTransport::FullRawRgba,
                None,
                1,
                0,
                true,
                MAX_DIRTY_REGIONS as u16,
            )
            .expect("initial full-mode render")
            .expect("initial full-mode frame");
        assert!(first.full_recovery);

        terminal.process_bytes(b"B");
        let second_snapshot = terminal.snapshot();
        let second = resources
            .render_cpu_raster_update(
                &second_snapshot,
                0,
                0,
                RasterTransport::FullRawRgba,
                Some(&first_snapshot),
                2,
                1,
                false,
                MAX_DIRTY_REGIONS as u16,
            )
            .expect("steady full-mode render")
            .expect("steady full-mode frame");
        assert_eq!(second.update.descriptor.kind, RasterUpdateKind::Full);
        assert!(!second.full_recovery);
    }

    #[tokio::test]
    async fn v3_subscription_pushes_initial_full_frame_with_exact_generation() {
        let (client_link, server_link) = vox::memory_link_pair(16);
        let service = VoxTerminalService::new();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            vox::acceptor_on(server_link)
                .on_lane(TerminalDispatcher::new(server_service))
                .establish_connection()
                .await
                .expect("Vox server connection should establish")
        });
        let connection = vox::initiator_on(client_link)
            .establish_connection()
            .await
            .expect("Vox client connection should establish");
        let server_guard = server.await.expect("Vox server task should establish");
        let client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("terminal service lane should open");

        let mut connect_request = test_connect_request("v3-initial", 1);
        connect_request.requested_width = 20;
        connect_request.requested_height = 6;
        connect_request.surface.columns = 20;
        connect_request.surface.rows = 6;
        connect_request.surface.panel_width = 240;
        connect_request.surface.panel_height = 120;
        let connected = client
            .connect(connect_request)
            .await
            .expect("terminal session should connect");
        let capabilities = client
            .presentation_capabilities(TerminalPresentationCapabilitiesRequest {
                session_id: connected.session_id.clone(),
                client_sequence: 2,
            })
            .await
            .expect("V3 capabilities should be available for the session");
        assert_eq!(capabilities.default_transport_id, TRANSPORT_FULL_PNG);
        let (expected_modes, expected_unavailable) = service
            .presentation_capability_sets(capabilities.server_sequence)
            .expect("expected atomic capability snapshot");
        assert_eq!(capabilities.modes, expected_modes);
        assert_eq!(capabilities.unavailable_presentations, expected_unavailable);

        let mut request = test_raster_request(RasterTransport::FullPng);
        request.session_id = connected.session_id.clone();
        request.presentation_generation = "generation-switch-17".to_string();
        request.client_sequence = 3;
        request.correlation_id = "v3-initial-01".to_string();
        let (frame_tx, mut frame_rx) = vox::channel::<TerminalRasterFrameEvent>();
        let subscription_client = client.clone();
        let subscription = tokio::spawn(async move {
            subscription_client
                .subscribe_raster_frames(request, frame_tx)
                .await
        });

        let initial = tokio::time::timeout(Duration::from_secs(10), frame_rx.recv())
            .await
            .expect("initial V3 frame should arrive without polling")
            .expect("initial V3 frame receive should succeed")
            .expect("initial V3 frame should be present");
        let initial = own_raster_frame(initial);
        assert_eq!(initial.presentation_generation, "generation-switch-17");
        assert_eq!(initial.renderer_id, CPU_RENDERER_ID);
        assert_eq!(initial.damage_mode_id, DAMAGE_FULL);
        assert_eq!(initial.transport_id, TRANSPORT_FULL_PNG);
        assert_eq!(initial.frame.kind, TerminalRasterFrameKind::Full);
        assert_eq!(initial.frame.encoding, TerminalFrameEncoding::Png);
        assert_eq!(initial.frame.stride, 0);
        assert!(initial.frame.payload.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(initial.base_frame_sequence, 0);
        assert!(initial.frame_sequence > 0);
        assert!(initial.full_resync);

        client
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "V3 initial-frame test complete".to_string(),
                client_sequence: 4,
            })
            .await
            .expect("test session should disconnect");
        tokio::time::timeout(Duration::from_secs(5), subscription)
            .await
            .expect("V3 subscription should stop with its session")
            .expect("V3 subscription task should not panic")
            .expect("V3 subscription should close cleanly");
        connection.shutdown().ok();
        server_guard.shutdown().ok();
    }

    #[tokio::test]
    async fn v3_switch_cpu_gpu_cpu_releases_idle_receiver_without_terminal_mutation() {
        let (client_link, server_link) = vox::memory_link_pair(32);
        let service = VoxTerminalService::new();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            vox::acceptor_on(server_link)
                .on_lane(TerminalDispatcher::new(server_service))
                .establish_connection()
                .await
                .expect("Vox server connection should establish")
        });
        let connection = vox::initiator_on(client_link)
            .establish_connection()
            .await
            .expect("Vox client connection should establish");
        let server_guard = server.await.expect("Vox server task should establish");
        let control_client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("terminal control lane should open");

        let mut connect_request = test_connect_request("v3-switch-cpu-gpu-cpu", 1);
        connect_request.requested_width = 20;
        connect_request.requested_height = 6;
        connect_request.surface.columns = 20;
        connect_request.surface.rows = 6;
        connect_request.surface.panel_width = 240;
        connect_request.surface.panel_height = 120;
        let connected = control_client
            .connect(connect_request)
            .await
            .expect("terminal session should connect");
        let capabilities = control_client
            .presentation_capabilities(TerminalPresentationCapabilitiesRequest {
                session_id: connected.session_id.clone(),
                client_sequence: 2,
            })
            .await
            .expect("presentation capabilities should be available");
        assert!(
            capabilities
                .modes
                .iter()
                .any(|mode| mode.renderer_id == GPU_RENDERER_ID),
            "the real CPU -> GPU -> CPU lifecycle proof requires a supported no-window GPU renderer"
        );
        let runtime = service
            .session(&connected.session_id)
            .expect("connected session should remain registered");
        let session_sequence = runtime.session_sequence;

        let first_client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("first raster lane should open");
        let mut first_request = test_raster_request(RasterTransport::FullPng);
        first_request.session_id = connected.session_id.clone();
        first_request.presentation_generation = "generation-cpu-first".to_string();
        first_request.client_sequence = 3;
        first_request.correlation_id = "switch-cpu-first".to_string();
        let (first_tx, mut first_rx) = vox::channel::<TerminalRasterFrameEvent>();
        let first_subscription = tokio::spawn(async move {
            first_client
                .subscribe_raster_frames(first_request, first_tx)
                .await
        });
        let first_frame = tokio::time::timeout(Duration::from_secs(10), first_rx.recv())
            .await
            .expect("first CPU frame should arrive")
            .expect("first CPU frame receive should succeed")
            .expect("first CPU frame should be present");
        let first_frame = own_raster_frame(first_frame);
        assert_eq!(first_frame.renderer_id, CPU_RENDERER_ID);

        let gpu_client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("GPU raster lane should open");
        let mut gpu_request = test_raster_request(RasterTransport::FullRawRgba);
        gpu_request.session_id = connected.session_id.clone();
        gpu_request.requested_renderer_id = GPU_RENDERER_ID.to_string();
        gpu_request.presentation_generation = "generation-gpu-middle".to_string();
        gpu_request.client_sequence = 4;
        gpu_request.correlation_id = "switch-gpu-middle".to_string();
        let (gpu_tx, mut gpu_rx) = vox::channel::<TerminalRasterFrameEvent>();
        let gpu_subscription = tokio::spawn(async move {
            gpu_client
                .subscribe_raster_frames(gpu_request, gpu_tx)
                .await
        });
        let gpu_frame = tokio::time::timeout(Duration::from_secs(30), gpu_rx.recv())
            .await
            .expect("GPU frame should arrive")
            .expect("GPU frame receive should succeed")
            .expect("GPU frame should be present");
        let gpu_frame = own_raster_frame(gpu_frame);
        assert_eq!(gpu_frame.renderer_id, GPU_RENDERER_ID);
        assert_eq!(gpu_frame.session_epoch, first_frame.session_epoch);
        assert_eq!(gpu_frame.connection_epoch, first_frame.connection_epoch);
        assert!(gpu_frame.terminal_sequence >= first_frame.terminal_sequence);
        assert_eq!(runtime.publication.subscriber_count(), 2);

        drop(first_rx);
        tokio::time::timeout(Duration::from_secs(5), first_subscription)
            .await
            .expect("dropping the idle CPU receiver should retire its subscription")
            .expect("first CPU subscription task should not panic")
            .expect("first CPU subscription should complete cleanly");
        assert_eq!(
            runtime.publication.subscriber_count(),
            1,
            "receiver closure must release the first renderer permit without a PTY mutation"
        );

        let third_client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("third raster lane should open");
        let mut third_request = test_raster_request(RasterTransport::FullPng);
        third_request.session_id = connected.session_id.clone();
        third_request.presentation_generation = "generation-cpu-third".to_string();
        third_request.client_sequence = 5;
        third_request.correlation_id = "switch-cpu-third".to_string();
        let (third_tx, mut third_rx) = vox::channel::<TerminalRasterFrameEvent>();
        let third_subscription = tokio::spawn(async move {
            third_client
                .subscribe_raster_frames(third_request, third_tx)
                .await
        });
        let third_frame = tokio::time::timeout(Duration::from_secs(10), third_rx.recv())
            .await
            .expect("third CPU frame must not block behind the retired presentation")
            .expect("third CPU frame receive should succeed")
            .expect("third CPU frame should be present");
        let third_frame = own_raster_frame(third_frame);
        assert_eq!(third_frame.renderer_id, CPU_RENDERER_ID);
        assert_eq!(third_frame.session_epoch, first_frame.session_epoch);
        assert_eq!(third_frame.connection_epoch, first_frame.connection_epoch);
        assert!(third_frame.terminal_sequence >= gpu_frame.terminal_sequence);
        assert_eq!(runtime.session_sequence, session_sequence);
        assert!(
            Arc::ptr_eq(
                &runtime,
                &service
                    .session(&connected.session_id)
                    .expect("presentation switches must retain the same PTY session")
            ),
            "renderer/transport switches must not restart the Rust terminal session"
        );

        drop(third_rx);
        drop(gpu_rx);
        tokio::time::timeout(Duration::from_secs(5), third_subscription)
            .await
            .expect("third CPU subscription should retire")
            .expect("third CPU subscription task should not panic")
            .expect("third CPU subscription should complete cleanly");
        tokio::time::timeout(Duration::from_secs(5), gpu_subscription)
            .await
            .expect("GPU subscription should retire")
            .expect("GPU subscription task should not panic")
            .expect("GPU subscription should complete cleanly");
        assert_eq!(runtime.publication.subscriber_count(), 0);

        control_client
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "CPU GPU CPU switch lifecycle test complete".to_string(),
                client_sequence: 6,
            })
            .await
            .expect("test session should disconnect");
        connection.shutdown().ok();
        server_guard.shutdown().ok();
    }

    #[tokio::test]
    async fn v3_raster_lane_does_not_starve_control_lane_content_reads() {
        let (client_link, server_link) = vox::memory_link_pair(16);
        let service = VoxTerminalService::new();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            vox::acceptor_on(server_link)
                .on_lane(TerminalDispatcher::new(server_service))
                .establish_connection()
                .await
                .expect("Vox server connection should establish")
        });
        let connection = vox::initiator_on(client_link)
            .establish_connection()
            .await
            .expect("Vox client connection should establish");
        let server_guard = server.await.expect("Vox server task should establish");
        let control_client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("terminal control lane should open");

        let mut connect_request = test_connect_request("v3-control-during-raster", 1);
        connect_request.requested_width = 20;
        connect_request.requested_height = 6;
        connect_request.surface.columns = 20;
        connect_request.surface.rows = 6;
        connect_request.surface.panel_width = 240;
        connect_request.surface.panel_height = 120;
        let connected = control_client
            .connect(connect_request)
            .await
            .expect("terminal session should connect");
        let raster_client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("terminal raster lane should open");
        let mut request = test_raster_request(RasterTransport::FullRawRgba);
        request.session_id = connected.session_id.clone();
        request.presentation_generation = "generation-control-witness".to_string();
        request.client_sequence = 2;
        request.correlation_id = "v3-control-during-raster-01".to_string();
        let (frame_tx, mut frame_rx) = vox::channel::<TerminalRasterFrameEvent>();
        let subscription = tokio::spawn(async move {
            raster_client
                .subscribe_raster_frames(request, frame_tx)
                .await
        });

        tokio::time::timeout(Duration::from_secs(10), frame_rx.recv())
            .await
            .expect("initial full RGBA frame should arrive")
            .expect("initial full RGBA frame receive should succeed")
            .expect("initial full RGBA frame should be present");
        let content = tokio::time::timeout(
            Duration::from_secs(2),
            control_client.get_content(TerminalContentRequest {
                session_id: connected.session_id.clone(),
                after_sequence: 0,
                max_chars: MAX_CONTENT_CHARS,
                client_sequence: 3,
            }),
        )
        .await
        .expect("raster publication must not starve the control lane")
        .expect("terminal content should remain readable while raster frames stream");
        assert_eq!(content.session_id, connected.session_id);

        control_client
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "control-lane concurrency test complete".to_string(),
                client_sequence: 4,
            })
            .await
            .expect("test session should disconnect");
        tokio::time::timeout(Duration::from_secs(5), subscription)
            .await
            .expect("raster subscription should stop with its session")
            .expect("raster subscription task should not panic")
            .expect("raster subscription should close cleanly");
        connection.shutdown().ok();
        server_guard.shutdown().ok();
    }

    #[tokio::test]
    async fn publication_hub_is_idle_when_unchanged_and_keeps_only_the_newest_mutation() {
        let hub = PublicationHub::new();
        let (_subscriber_id, mut signals) = hub.register(1);
        assert_eq!(signals.borrow_and_update().terminal_sequence, 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), signals.changed())
                .await
                .is_err(),
            "an idle publication must block instead of producing synthetic timer work"
        );

        for sequence in 2..=100 {
            hub.publish(sequence, sequence == 100);
        }
        signals
            .changed()
            .await
            .expect("the newest mutation should wake the subscriber");
        let newest = *signals.borrow_and_update();
        assert_eq!(newest.terminal_sequence, 100);
        assert_eq!(newest.mutations, 99);
        assert!(newest.full_resync);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), signals.changed())
                .await
                .is_err(),
            "replaced mutations must not survive as an unbounded queue"
        );
    }

    #[tokio::test]
    async fn publication_hubs_keep_two_sessions_isolated() {
        let first = PublicationHub::new();
        let second = PublicationHub::new();
        let (_, mut first_signals) = first.register(1);
        let (_, mut second_signals) = second.register(10);
        first_signals.borrow_and_update();
        second_signals.borrow_and_update();

        first.publish(2, false);
        first_signals
            .changed()
            .await
            .expect("the first session should receive its own mutation");
        assert_eq!(first_signals.borrow_and_update().terminal_sequence, 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), second_signals.changed())
                .await
                .is_err(),
            "a first-session mutation must not wake the second session"
        );
    }

    #[test]
    fn service_epochs_are_process_scoped_and_connection_unique() {
        let first = VoxTerminalService::new();
        let second = VoxTerminalService::new();
        assert_eq!(first.process_epoch, second.process_epoch);
        assert_ne!(first.connection_epoch, second.connection_epoch);
        assert!(
            first
                .connection_epoch
                .starts_with(first.process_epoch.as_ref())
        );
        assert!(
            second
                .connection_epoch
                .starts_with(second.process_epoch.as_ref())
        );
    }

    #[tokio::test]
    async fn connection_scoped_services_cannot_access_or_disconnect_each_others_sessions() {
        let root = VoxTerminalService::new();
        let owner = root.for_connection();
        let other = root.for_connection();
        let connected = owner
            .connect(test_connect_request("connection-owner", 1))
            .await
            .expect("the owning connection should create a terminal session");

        let lookup_error = other
            .capabilities(TerminalCapabilitiesRequest {
                session_id: connected.session_id.clone(),
                client_sequence: 2,
            })
            .await
            .expect_err("another connection must not access the session");
        assert_eq!(lookup_error.code, TerminalErrorCode::SessionNotFound);

        let disconnect_error = other
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id.clone(),
                reason: "wrong connection".to_string(),
                client_sequence: 2,
            })
            .await
            .expect_err("another connection must not remove the session");
        assert_eq!(disconnect_error.code, TerminalErrorCode::SessionNotFound);
        assert!(
            owner.session(&connected.session_id).is_ok(),
            "a rejected foreign disconnect must leave the owner's session intact"
        );

        owner
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "owner cleanup".to_string(),
                client_sequence: 3,
            })
            .await
            .expect("the owning connection should disconnect its session");
    }

    #[tokio::test]
    async fn raster_renderer_leases_are_bounded_and_reuse_session_resources() {
        let service = VoxTerminalService::new();
        let connected = service
            .connect(test_connect_request("bounded-raster-renderers", 1))
            .await
            .expect("test terminal session should connect");
        let runtime = service
            .session(&connected.session_id)
            .expect("connected session should remain registered");

        let mut first = runtime
            .lease_raster_render_resources()
            .await
            .expect("first active presentation should lease a renderer");
        let second = runtime
            .lease_raster_render_resources()
            .await
            .expect("one pending presentation may overlap the active presentation");
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                runtime.lease_raster_render_resources(),
            )
            .await
            .is_err(),
            "a third presentation must wait instead of creating unbounded renderer state"
        );

        first
            .resources
            .as_mut()
            .expect("leased resources should be present")
            .stats
            .frame_buffer_reuses = 7;
        drop(first);
        let third = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.lease_raster_render_resources(),
        )
        .await
        .expect("retiring one generation should release a renderer permit")
        .expect("the session should remain open");
        assert_eq!(
            third
                .resources
                .as_ref()
                .expect("re-leased resources should be present")
                .stats
                .frame_buffer_reuses,
            7,
            "a presentation switch should recover the warm per-session resource"
        );

        drop(third);
        drop(second);
        service
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "bounded raster renderer test complete".to_string(),
                client_sequence: 2,
            })
            .await
            .expect("test terminal session should disconnect");
    }

    #[tokio::test]
    async fn raster_subscription_guards_and_session_shutdown_cancel_gpu_work() {
        let service = VoxTerminalService::new();
        let connected = service
            .connect(test_connect_request("gpu-cancellation", 1))
            .await
            .expect("test terminal session should connect");
        let runtime = service
            .session(&connected.session_id)
            .expect("test session runtime");

        let (subscriber_id, _signals) = runtime.publication.register(runtime.sequence());
        let retired = VulkanOffscreenCancellation::default();
        runtime
            .raster_render_cancellations
            .lock()
            .expect("cancellation registry")
            .insert(subscriber_id, retired.clone());
        {
            let _guard = SubscriptionGuard {
                runtime: Arc::downgrade(&runtime),
                subscriber_id,
                cancellation: Some(retired.clone()),
            };
            assert!(!retired.is_cancelled());
        }
        assert!(retired.is_cancelled());
        assert_eq!(runtime.publication.subscriber_count(), 0);
        assert!(
            runtime
                .raster_render_cancellations
                .lock()
                .expect("cancellation registry after retirement")
                .is_empty()
        );

        let shutdown = VulkanOffscreenCancellation::default();
        runtime
            .raster_render_cancellations
            .lock()
            .expect("shutdown cancellation registry")
            .insert(999, shutdown.clone());
        service
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "GPU cancellation test complete".to_string(),
                client_sequence: 2,
            })
            .await
            .expect("test terminal session should disconnect");
        assert!(shutdown.is_cancelled());
    }

    #[test]
    fn retained_render_resources_reuse_warm_a_b_a_sizes_and_buffers() {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 80, 24);
        terminal.process_bytes(b"\x1b[?25lcache me");
        let snapshot = terminal.snapshot();
        let mut resources = TerminalRenderResources::default();

        let first = resources
            .encode_png(&snapshot, 0, 0)
            .expect("first A frame should render");
        let first_stats = first.resource_stats;
        assert!(first.font_load_micros > 0);
        assert_eq!(first_stats.font_catalog_acquisitions, 1);
        assert_eq!(first_stats.font_renderer_constructions, 1);
        assert_eq!(first_stats.frame_buffer_grows, 1);

        let second = resources
            .encode_png(&snapshot, 1600, 900)
            .expect("B frame should render");
        let second_stats = second.resource_stats;
        assert!(second.font_load_micros > 0);
        assert_eq!(second_stats.font_renderer_constructions, 2);

        let third = resources
            .encode_png(&snapshot, 0, 0)
            .expect("second A frame should render from retained state");
        let third_stats = third.resource_stats;
        assert_eq!(third.font_load_micros, 0);
        assert_eq!(third_stats.font_catalog_acquisitions, 1);
        assert_eq!(third_stats.font_renderer_constructions, 2);
        assert_eq!(third_stats.font_renderer_evictions, 0);
        assert_eq!(third_stats.font_renderer_cache_len, 2);
        assert_eq!(
            third_stats.font_renderer_cache_capacity,
            MAX_CACHED_FONT_SIZES as u64
        );
        assert!(third_stats.font_renderer_cache_hits > second_stats.font_renderer_cache_hits);
        assert!(third_stats.glyph_cache_hits > second_stats.glyph_cache_hits);
        assert!(third_stats.frame_buffer_reuses >= 1);
        assert!(third_stats.png_buffer_reuses >= 1);
        assert_eq!(third.payload, first.payload);
    }

    #[test]
    fn cpu_bridge_fits_discrete_grid_metrics_within_panel_target() {
        let terminal = TerminalSession::new(TerminalLimits::default(), 70, 21);
        let snapshot = terminal.snapshot();
        let mut resources = TerminalRenderResources::default();

        let rendered = resources
            .encode_png(&snapshot, 425, 238)
            .expect("normal-scale SFM panel should fit without clipping");

        assert_eq!(
            u32::from(rendered.width),
            70 * u32::from(rendered.cell_width)
        );
        assert_eq!(
            u32::from(rendered.height),
            21 * u32::from(rendered.cell_height)
        );
        assert!(rendered.width <= 425);
        assert!(rendered.height <= 238);
    }

    #[test]
    fn size_renderer_cache_is_bounded_and_evicts_least_recently_used_entry() {
        let mut resources = TerminalRenderResources::default();
        for pixel_size in [8.0_f32, 9.0, 10.0, 11.0, 12.0] {
            resources
                .ensure_font(pixel_size)
                .expect("size renderer should be constructed");
        }
        let stats = resources.snapshot_stats();
        assert_eq!(stats.font_renderer_cache_len, MAX_CACHED_FONT_SIZES as u64);
        assert_eq!(stats.font_renderer_constructions, 5);
        assert_eq!(stats.font_renderer_evictions, 1);

        let constructions_before = stats.font_renderer_constructions;
        resources
            .ensure_font(9.0)
            .expect("retained size should remain cached");
        let retained = resources.snapshot_stats();
        assert_eq!(retained.font_renderer_constructions, constructions_before);

        resources
            .ensure_font(8.0)
            .expect("evicted least-recent size should be reconstructable");
        let reconstructed = resources.snapshot_stats();
        assert_eq!(
            reconstructed.font_renderer_constructions,
            constructions_before + 1
        );
        assert_eq!(reconstructed.font_renderer_evictions, 2);
        assert_eq!(
            reconstructed.font_renderer_cache_len,
            MAX_CACHED_FONT_SIZES as u64
        );
    }

    #[tokio::test]
    async fn one_locked_session_does_not_block_another_session_or_the_registry() {
        let service = VoxTerminalService::new();
        let first = service
            .connect(test_connect_request("first", 1))
            .await
            .expect("first terminal session should connect");
        let second = service
            .connect(test_connect_request("second", 2))
            .await
            .expect("second terminal session should connect");

        let first_state = service
            .session(&first.session_id)
            .expect("first session should remain registered");
        let second_state = service
            .session(&second.session_id)
            .expect("second session should remain registered");
        assert_ne!(first_state.session_epoch, second_state.session_epoch);
        assert_eq!(first_state.connection_epoch, second_state.connection_epoch);
        let first_guard = first_state.state.lock().expect("first session lock");
        let second_capabilities = tokio::time::timeout(
            Duration::from_millis(250),
            service.capabilities(TerminalCapabilitiesRequest {
                session_id: second.session_id.clone(),
                client_sequence: 3,
            }),
        )
        .await
        .expect("a locked first session must not block registry/second-session access")
        .expect("second session capabilities should succeed");
        assert_eq!(second_capabilities.session_id, second.session_id);
        drop(first_guard);

        service
            .disconnect(TerminalDisconnectRequest {
                session_id: first.session_id,
                reason: "test complete".to_string(),
                client_sequence: 4,
            })
            .await
            .expect("first terminal session should disconnect");
        service
            .disconnect(TerminalDisconnectRequest {
                session_id: second.session_id,
                reason: "test complete".to_string(),
                client_sequence: 5,
            })
            .await
            .expect("second terminal session should disconnect");
    }

    #[tokio::test]
    async fn vox_puppet_runs_power_shell_and_returns_bounded_png() {
        let (client_link, server_link) = vox::memory_link_pair(16);
        let service = VoxTerminalService::new();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            vox::acceptor_on(server_link)
                .on_lane(TerminalDispatcher::new(server_service))
                .establish_connection()
                .await
                .expect("Vox server connection should establish")
        });
        let connection = vox::initiator_on(client_link)
            .establish_connection()
            .await
            .expect("Vox client connection should establish");
        let server_guard = server.await.expect("Vox server task should establish");
        let client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("terminal service lane should open");

        let connected = client
            .connect(TerminalConnectRequest {
                endpoint: "test".to_string(),
                requested_width: 120,
                requested_height: 40,
                surface: TerminalSurfaceMetrics {
                    columns: 120,
                    rows: 40,
                    panel_width: 1200,
                    panel_height: 760,
                    ..TerminalSurfaceMetrics::default()
                },
                capabilities: TerminalCapabilities {
                    text_input: true,
                    key_input: true,
                    mouse_input: true,
                    structured_cells: false,
                    raster_frames: true,
                    selection: false,
                    scrollback: true,
                    dirty_frames: false,
                    max_width: MAX_COLUMNS,
                    max_height: MAX_ROWS,
                    max_frame_bytes: MAX_FRAME_BYTES,
                    backend_id: "".to_string(),
                    transport_id: "".to_string(),
                },
                client_sequence: 1,
                correlation_id: "connect-01".to_string(),
            })
            .await
            .expect("terminal session should connect");
        let (frame_tx, mut frame_rx) = vox::channel::<TerminalFrameEvent>();
        let subscription_client = client.clone();
        let subscription_session = connected.session_id.clone();
        let subscription = tokio::spawn(async move {
            subscription_client
                .subscribe_frames(
                    TerminalSubscribeRequest {
                        session_id: subscription_session,
                        after_terminal_sequence: 0,
                        max_frame_bytes: MAX_FRAME_BYTES,
                        client_sequence: 2,
                        correlation_id: "push-01".to_string(),
                    },
                    frame_tx,
                )
                .await
        });
        let initial = tokio::time::timeout(Duration::from_secs(10), frame_rx.recv())
            .await
            .expect("initial pushed frame should arrive without polling")
            .expect("initial pushed frame receive should succeed")
            .expect("initial pushed frame should be present");
        let initial = own_frame(initial);
        assert!(initial.full_resync);
        assert!(initial.frame.complete);
        assert_eq!(initial.frame.kind, TerminalFrameKind::Full);
        assert!(initial.frame.payload.starts_with(b"\x89PNG\r\n\x1a\n"));
        let initial_terminal_sequence = initial.terminal_sequence;
        let initial_frame_sequence = initial.frame_sequence;

        let (cancelled_tx, cancelled_rx) = vox::channel::<TerminalFrameEvent>();
        let cancelled_client = client.clone();
        let cancelled_session = connected.session_id.clone();
        let cancelled_subscription = tokio::spawn(async move {
            cancelled_client
                .subscribe_frames(
                    TerminalSubscribeRequest {
                        session_id: cancelled_session,
                        after_terminal_sequence: initial_terminal_sequence,
                        max_frame_bytes: MAX_FRAME_BYTES,
                        client_sequence: 3,
                        correlation_id: "cancelled-push-01".to_string(),
                    },
                    cancelled_tx,
                )
                .await
        });
        let runtime = service
            .session(&connected.session_id)
            .expect("connected session should remain registered");
        for _ in 0..100 {
            if runtime.publication.subscriber_count() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.publication.subscriber_count(), 2);
        drop(cancelled_rx);
        for _ in 0..200 {
            if runtime.publication.subscriber_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            runtime.publication.subscriber_count(),
            1,
            "dropping the channel receiver must unregister its idle request-scoped publisher without requiring a terminal mutation"
        );
        tokio::time::timeout(Duration::from_secs(5), cancelled_subscription)
            .await
            .expect("dropped receiver should finish the subscription request")
            .expect("dropped receiver subscription task should not panic")
            .expect("dropped receiver subscription should complete cleanly");
        client
            .send_text(TerminalTextInput {
                session_id: connected.session_id.clone(),
                text: "1..100; Write-Host -ForegroundColor Cyan \"hello, world!\"; exit\r"
                    .to_string(),
                client_sequence: 2,
                correlation_id: "output-01".to_string(),
            })
            .await
            .expect("PowerShell puppet input should be accepted");

        let mut pushed = None;
        for _ in 0..600 {
            let candidate = tokio::time::timeout(Duration::from_secs(10), frame_rx.recv())
                .await
                .expect("PTY mutation should push a frame without another RPC")
                .expect("pushed frame receive should succeed")
                .expect("pushed frame should be present");
            let candidate = own_frame(candidate);
            let output_ready = service
                .session(&connected.session_id)
                .ok()
                .and_then(|session| {
                    session.state.lock().ok().map(|session| {
                        let text = session.terminal.snapshot().visible_text();
                        text.lines().any(|line| line.trim() == "100")
                            && text.contains("hello, world!")
                    })
                })
                .unwrap_or(false);
            if output_ready && !candidate.frame.payload.is_empty() {
                pushed = Some(candidate);
                break;
            }
        }
        let pushed = pushed.expect("PowerShell output should arrive through pushed frames");
        assert!(pushed.terminal_sequence > initial_terminal_sequence);
        assert!(pushed.frame_sequence > initial_frame_sequence);
        assert_eq!(pushed.connection_epoch, service.connection_epoch.as_ref());
        assert!(
            pushed
                .session_epoch
                .starts_with(service.process_epoch.as_ref())
        );
        assert!(pushed.publication.pre_render_coalesced > 0);
        assert!(pushed.publication.mutations > 0);
        assert!(pushed.publication.renders_started >= pushed.publication.renders_completed);
        assert!(pushed.publication.renders_completed >= pushed.publication.frames_pushed);
        assert!(pushed.publication.pending_depth <= 1);
        assert!(pushed.publication.pending_depth_max <= 1);
        assert_eq!(
            pushed.frame.timing.font_load_us, 0,
            "the request-scoped publisher should retain its warmed font renderer"
        );
        let snapshot = pushed.frame;
        assert_eq!(snapshot.encoding, TerminalFrameEncoding::Png);
        assert_eq!(snapshot.kind, TerminalFrameKind::Full);
        assert!(snapshot.payload.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(snapshot.payload.len() <= MAX_FRAME_BYTES as usize);

        let session = service
            .sessions
            .lock()
            .expect("terminal session registry lock")
            .get(&connected.session_id)
            .cloned()
            .expect("session should remain available");
        let session = session.state.lock().expect("terminal session lock");
        let terminal_snapshot = session.terminal.snapshot();
        let visible_text = terminal_snapshot.visible_text();
        assert!(
            visible_text.lines().any(|line| line.trim() == "100"),
            "Rust terminal state should contain range output 100:\n{visible_text}"
        );
        let hello = terminal_snapshot
            .cells
            .chunks(usize::from(terminal_snapshot.columns))
            .enumerate()
            .find_map(|(row, cells)| {
                let text: String = cells.iter().map(|cell| cell.character).collect();
                text.find("hello, world!").map(|column| (row, column))
            })
            .unwrap_or_else(|| panic!("Rust terminal state should contain Write-Host output (closed={}, sequence={}, raw={:?}):\n{}", session.closed, session.sequence, String::from_utf8_lossy(&session.debug_output), terminal_snapshot.visible_text()));
        assert!(matches!(
            terminal_snapshot
                .cell(hello.1 as u16, hello.0 as u16)
                .expect("hello cell")
                .foreground,
            Color::Indexed(6 | 14) | Color::Rgb(0, 255, 255)
        ));
        drop(session);

        let content = client
            .get_content(TerminalContentRequest {
                session_id: connected.session_id.clone(),
                after_sequence: 0,
                max_chars: 4096,
                client_sequence: 4,
            })
            .await
            .expect("terminal content witness should be returned");
        assert!(content.complete);
        assert!(!content.truncated);
        assert!(
            content.text.lines().any(|line| line.trim() == "100"),
            "content witness should contain range output 100:\n{}",
            content.text
        );
        assert!(
            content.text.contains("hello, world!"),
            "content witness should contain Write-Host output:\n{}",
            content.text
        );

        let explicit_snapshot = client
            .snapshot(TerminalSnapshotRequest {
                session_id: connected.session_id.clone(),
                after_sequence: 0,
                max_frame_bytes: MAX_FRAME_BYTES,
                client_sequence: 5,
                correlation_id: "explicit-capture-01".to_string(),
            })
            .await
            .expect("explicit capture should remain available for diagnostics/resync");
        let timing = service
            .latest_snapshot_timing()
            .expect("Rust snapshot timing witness should be recorded");
        assert_eq!(
            timing.correlation_id.as_str(),
            explicit_snapshot.correlation_id.as_str()
        );
        assert!(timing.frame_width > 0);
        assert!(timing.frame_height > 0);
        assert!(timing.payload_bytes > 0);
        let timing_json = facet_json::to_string(&timing).expect("timing witness should be JSON");
        println!("SFM_TERMINAL_TIMING_WITNESS {timing_json}");

        client
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "test complete".to_string(),
                client_sequence: 6,
            })
            .await
            .expect("terminal session should disconnect");
        let subscription_result = tokio::time::timeout(Duration::from_secs(5), subscription)
            .await
            .expect("session close should terminate the request-scoped subscription")
            .expect("subscription task should not panic")
            .expect("subscription RPC should complete cleanly");
        assert_eq!(subscription_result.state, TerminalState::Closed);
        connection.shutdown().ok();
        server_guard.shutdown().ok();
    }

    #[tokio::test]
    async fn vox_cancel_interrupts_a_running_power_shell_command() {
        let (client_link, server_link) = vox::memory_link_pair(16);
        let service = VoxTerminalService::new();
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            vox::acceptor_on(server_link)
                .on_lane(TerminalDispatcher::new(server_service))
                .establish_connection()
                .await
                .expect("Vox server connection should establish")
        });
        let connection = vox::initiator_on(client_link)
            .establish_connection()
            .await
            .expect("Vox client connection should establish");
        let server_guard = server.await.expect("Vox server task should establish");
        let client = connection
            .open_lane::<TerminalClient>()
            .await
            .expect("terminal service lane should open");
        let connected = client
            .connect(TerminalConnectRequest {
                endpoint: "test-cancel".to_string(),
                requested_width: 120,
                requested_height: 40,
                surface: TerminalSurfaceMetrics {
                    columns: 120,
                    rows: 40,
                    panel_width: 1200,
                    panel_height: 760,
                    ..TerminalSurfaceMetrics::default()
                },
                capabilities: TerminalCapabilities {
                    text_input: true,
                    key_input: true,
                    mouse_input: true,
                    structured_cells: false,
                    raster_frames: true,
                    selection: false,
                    scrollback: true,
                    dirty_frames: false,
                    max_width: MAX_COLUMNS,
                    max_height: MAX_ROWS,
                    max_frame_bytes: MAX_FRAME_BYTES,
                    backend_id: "".to_string(),
                    transport_id: "".to_string(),
                },
                client_sequence: 1,
                correlation_id: "connect-cancel-01".to_string(),
            })
            .await
            .expect("terminal session should connect");

        client
            .send_text(TerminalTextInput {
                session_id: connected.session_id.clone(),
                text: "Write-Output READY\r".to_string(),
                client_sequence: 2,
                correlation_id: "ready-01".to_string(),
            })
            .await
            .expect("PowerShell readiness input should be accepted");
        let mut ready = false;
        for client_sequence in 3..=100 {
            let candidate = client
                .get_content(TerminalContentRequest {
                    session_id: connected.session_id.clone(),
                    after_sequence: 0,
                    max_chars: 32 * 1024,
                    client_sequence,
                })
                .await
                .expect("terminal content should be readable during startup");
            if candidate.text.contains("READY") {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(ready, "PowerShell readiness command did not complete");

        client
            .send_text(TerminalTextInput {
                session_id: connected.session_id.clone(),
                text:
                    "1..10000 | ForEach-Object { Write-Output $_; Start-Sleep -Milliseconds 10 }\r"
                        .to_string(),
                client_sequence: 101,
                correlation_id: "long-running-01".to_string(),
            })
            .await
            .expect("long-running PowerShell input should be accepted");
        tokio::time::sleep(Duration::from_millis(1_000)).await;

        let cancelled = client
            .cancel(TerminalCancelRequest {
                session_id: connected.session_id.clone(),
                request_sequence: 2,
                reason: "puppet cancellation proof".to_string(),
                client_sequence: 3,
                correlation_id: "cancel-01".to_string(),
            })
            .await
            .expect("terminal cancellation should be accepted");
        assert_eq!(cancelled.state, TerminalState::Ready);

        let mut content = String::new();
        for client_sequence in 4..=120 {
            let candidate = client
                .get_content(TerminalContentRequest {
                    session_id: connected.session_id.clone(),
                    after_sequence: 0,
                    max_chars: 32 * 1024,
                    client_sequence,
                })
                .await
                .expect("terminal content should remain readable after cancellation");
            content = candidate.text;
            if content.contains("PS ") || content.contains("❯") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            content.contains("PS ") || content.contains("❯"),
            "cancelled PowerShell command should return to a prompt:\n{content}"
        );
        assert!(
            !content.lines().any(|line| line.trim() == "10000"),
            "cancelled PowerShell command should not contain the range sentinel:\n{content}"
        );

        client
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "test complete".to_string(),
                client_sequence: 121,
            })
            .await
            .expect("terminal session should disconnect");
        connection.shutdown().ok();
        server_guard.shutdown().ok();
    }

    #[test]
    fn key_input_uses_terminal_control_sequences_without_hardcoded_chords() {
        assert_eq!(
            key_input_bytes(65, GLFW_MOD_CONTROL, true),
            b"\x1b[65;30;1;1;8;1_"
        );
        assert_eq!(
            key_input_bytes(67, GLFW_MOD_CONTROL, true),
            b"\x1b[67;46;3;1;8;1_"
        );
        assert_eq!(
            key_input_bytes(67, GLFW_MOD_CONTROL, false),
            b"\x1b[67;46;0;0;8;1_"
        );
        assert_eq!(
            key_input_bytes(76, GLFW_MOD_CONTROL, true),
            b"\x1b[76;38;12;1;8;1_"
        );
        assert_eq!(
            key_input_bytes(GLFW_KEY_BACKSPACE, GLFW_MOD_CONTROL, true),
            b"\x1b[8;14;8;1;8;1_"
        );
        assert_eq!(
            key_input_bytes(GLFW_KEY_RIGHT, GLFW_MOD_CONTROL, true),
            b"\x1b[39;77;0;1;264;1_"
        );
        assert_eq!(
            key_input_bytes(GLFW_KEY_LEFT, GLFW_MOD_CONTROL | GLFW_MOD_SHIFT, true),
            b"\x1b[37;75;0;1;280;1_"
        );
        assert_eq!(
            key_input_bytes(GLFW_KEY_RIGHT, 0, false),
            b"\x1b[39;77;0;0;256;1_"
        );
    }

    #[test]
    fn unmodified_character_key_codes_are_supported_for_protocol_tests() {
        assert_eq!(key_input_bytes(65, 0, true), b"\x1b[65;30;97;1;0;1_");
        assert_eq!(
            key_input_bytes(65, GLFW_MOD_SHIFT, true),
            b"\x1b[65;30;65;1;16;1_"
        );
        assert_eq!(
            key_input_bytes(GLFW_KEY_ENTER, 0, true),
            b"\x1b[13;28;13;1;0;1_"
        );
    }

    #[test]
    fn mouse_input_respects_terminal_reporting_mode_and_uses_one_based_sgr_cells() {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 80, 24);
        assert!(mouse_input_bytes(&terminal, 2, 3, 0, 0, true, false, 0, 0).is_empty());
        terminal.process_bytes(b"\x1b[?1000;1006h");
        assert_eq!(
            mouse_input_bytes(&terminal, 2, 3, 1, 0, true, false, 0, 0),
            b"\x1b[<0;3;4M".to_vec()
        );
        assert_eq!(
            mouse_input_bytes(&terminal, 2, 3, 0, 0, false, false, 0, 1),
            b"\x1b[<64;3;4M".to_vec()
        );
        assert_eq!(
            mouse_input_bytes(&terminal, 4, 5, 1, 0, true, true, 0, 0),
            b"\x1b[<32;5;6M".to_vec()
        );
        assert_eq!(
            mouse_input_bytes(&terminal, 4, 5, 2, 0, true, true, 0, 0),
            b"\x1b[<33;5;6M".to_vec()
        );
    }
}
