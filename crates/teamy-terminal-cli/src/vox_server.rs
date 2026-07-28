//! Rust-authoritative Vox terminal endpoint.
//!
//! The endpoint owns the PowerShell PTY, VT parsing, and CPU PNG rasterization.
//! Java receives only bounded full-frame PNG snapshots and sends input text
//! back through the generated Vox contract.

use eyre::{Context, Result};
use vox_terminal as vox;
use spec_proto::terminal::{
    Terminal, TerminalCancelRequest, TerminalCapabilities, TerminalCapabilitiesRequest,
    TerminalCapabilitiesResult, TerminalConnectRequest, TerminalConnectResult, TerminalCursor,
    TerminalDisconnectRequest, TerminalDispatcher, TerminalError, TerminalErrorCode,
    TerminalFrameEncoding, TerminalFrameKind, TerminalInputResult, TerminalKeyInput,
    TerminalMouseInput, TerminalOperationResult, TerminalPromptMetadata, TerminalRange,
    TerminalResizeRequest, TerminalResizeResult, TerminalSelection, TerminalSnapshot,
    TerminalSnapshotRequest, TerminalState, TerminalTextInput,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use teamy_terminal_core::{TerminalLimits, TerminalSession};
use teamy_terminal_font::{DEFAULT_PIXEL_SIZE, TerminalFont};
use teamy_terminal_pty::{PtyConfig, PtyEvent, PtySession};

const MAX_COLUMNS: u16 = 240;
const MAX_ROWS: u16 = 120;
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
pub const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:63946";

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
    let has_modifier = modifiers & (GLFW_MOD_SHIFT | GLFW_MOD_CONTROL | GLFW_MOD_ALT | GLFW_MOD_SUPER) != 0;
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

fn mouse_input_bytes(
    terminal: &TerminalSession,
    x: u16,
    y: u16,
    buttons: u8,
    button: u8,
    pressed: bool,
    wheel_x: i32,
    wheel_y: i32,
) -> Vec<u8> {
    if !terminal.mouse_reporting_enabled() {
        return Vec::new();
    }
    let mut code = if wheel_y > 0 {
        64
    } else if wheel_y < 0 {
        65
    } else if wheel_x > 0 {
        66
    } else if wheel_x < 0 {
        67
    } else {
        u16::from(button.min(2))
    };
    if !pressed && wheel_x == 0 && wheel_y == 0 {
        code = if buttons == 0 { 3 } else { code };
    }
    let x = u32::from(x).saturating_add(1);
    let y = u32::from(y).saturating_add(1);
    if terminal.sgr_mouse_mode_enabled() {
        let suffix = if pressed || wheel_x != 0 || wheel_y != 0 { 'M' } else { 'm' };
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
    sessions: Arc<Mutex<BTreeMap<String, TerminalSessionState>>>,
    next_session: Arc<AtomicU64>,
}

struct TerminalSessionState {
    pty: PtySession,
    terminal: TerminalSession,
    sequence: i64,
    closed: bool,
    debug_output: Vec<u8>,
}

impl VoxTerminalService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            next_session: Arc::new(AtomicU64::new(1)),
        }
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
            dirty_frames: false,
            max_width: MAX_COLUMNS,
            max_height: MAX_ROWS,
            max_frame_bytes: MAX_FRAME_BYTES,
        }
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

    fn drain_pty(session: &mut TerminalSessionState) -> Result<(), TerminalError> {
        // PTY output is drained opportunistically.  In particular, an input
        // request must not hold the session mutex while a long command such
        // as `1..10000` is still producing output; snapshot requests need to
        // be able to observe that output concurrently.
        for event_index in 0..512 {
            let event = session
                .pty
                .recv_timeout(if event_index == 0 {
                    Duration::from_millis(10)
                } else {
                    Duration::ZERO
                })
                .map_err(|error| {
                    Self::error(
                        TerminalErrorCode::Internal,
                        error.to_string(),
                        session.sequence,
                    )
                })?;
            let Some(event) = event else {
                break;
            };
            match event {
                PtyEvent::Output(bytes) => {
                    Self::answer_terminal_queries(&session.pty, &bytes)?;
                    session.debug_output.extend_from_slice(&bytes);
                    if session.debug_output.len() > 64 * 1024 {
                        let remove = session.debug_output.len() - 64 * 1024;
                        session.debug_output.drain(..remove);
                    }
                    session.terminal.process_bytes(&bytes);
                    Self::next_sequence(&mut session.sequence);
                }
                PtyEvent::Exited(status) => {
                    session.closed = true;
                    Self::next_sequence(&mut session.sequence);
                    if !status.success {
                        return Err(Self::error(
                            TerminalErrorCode::Disconnected,
                            format!("PowerShell exited with code {:?}", status.code),
                            session.sequence,
                        ));
                    }
                }
                PtyEvent::Error(message) => {
                    return Err(Self::error(
                        TerminalErrorCode::Internal,
                        message,
                        session.sequence,
                    ));
                }
            }
        }
        Ok(())
    }

    fn answer_terminal_queries(pty: &PtySession, bytes: &[u8]) -> Result<(), TerminalError> {
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

    fn encode_png(terminal: &TerminalSession) -> Result<(u16, u16, u32, Vec<u8>), TerminalError> {
        let snapshot = terminal.snapshot();
        let mut font = TerminalFont::new(DEFAULT_PIXEL_SIZE)
            .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        let width = u32::from(snapshot.columns).saturating_mul(font.cell_width());
        let height = u32::from(snapshot.rows).saturating_mul(font.cell_height());
        let frame = font.render(&snapshot, width, height);
        let stride = width.saturating_mul(4);
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(Cursor::new(&mut bytes), frame.width, frame.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
            writer
                .write_image_data(&frame.pixels)
                .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        }
        let width = u16::try_from(width).map_err(|_| {
            Self::error(
                TerminalErrorCode::CapacityExceeded,
                "PNG width exceeds protocol bounds",
                0,
            )
        })?;
        let height = u16::try_from(height).map_err(|_| {
            Self::error(
                TerminalErrorCode::CapacityExceeded,
                "PNG height exceeds protocol bounds",
                0,
            )
        })?;
        Ok((width, height, stride, bytes))
    }

    fn empty_range() -> TerminalRange {
        TerminalRange {
            start_x: 0,
            start_y: 0,
            end_x: 0,
            end_y: 0,
        }
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
        let session_id = format!(
            "sfm-terminal-{}",
            self.next_session.fetch_add(1, Ordering::Relaxed)
        );
        let pty = PtySession::spawn(&PtyConfig::test())
            .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        let mut state = TerminalSessionState {
            pty,
            terminal: TerminalSession::new(
                TerminalLimits::default(),
                request.requested_width,
                request.requested_height,
            ),
            sequence: 0,
            closed: false,
            debug_output: Vec::new(),
        };
        state
            .pty
            .resize(request.requested_width, request.requested_height, 0, 0)
            .map_err(|error| Self::error(TerminalErrorCode::Internal, error.to_string(), 0))?;
        Self::drain_pty(&mut state)?;
        let sequence = Self::next_sequence(&mut state.sequence);
        self.sessions
            .lock()
            .map_err(|_| {
                Self::error(
                    TerminalErrorCode::Internal,
                    "terminal session lock poisoned",
                    sequence,
                )
            })?
            .insert(session_id.clone(), state);
        Ok(TerminalConnectResult {
            session_id,
            capabilities: Self::capabilities(),
            width: request.requested_width,
            height: request.requested_height,
            server_sequence: sequence,
            state: TerminalState::Ready,
        })
    }

    async fn capabilities(
        &self,
        request: TerminalCapabilitiesRequest,
    ) -> Result<TerminalCapabilitiesResult, TerminalError> {
        let sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        Ok(TerminalCapabilitiesResult {
            session_id: request.session_id,
            capabilities: Self::capabilities(),
            server_sequence: session.sequence,
        })
    }

    async fn resize(
        &self,
        request: TerminalResizeRequest,
    ) -> Result<TerminalResizeResult, TerminalError> {
        Self::validate_dimensions(request.width, request.height)?;
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get_mut(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
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
        Ok(TerminalResizeResult {
            session_id: request.session_id,
            width: request.width,
            height: request.height,
            server_sequence: sequence,
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
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get_mut(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        let text = request.text.into_bytes();
        session.pty.write_input(&text).map_err(|error| {
            Self::error(
                TerminalErrorCode::Internal,
                error.to_string(),
                session.sequence,
            )
        })?;
        Self::drain_pty(session)?;
        let sequence = Self::next_sequence(&mut session.sequence);
        Ok(TerminalInputResult {
            session_id: request.session_id,
            server_sequence: sequence,
            frame_sequence: sequence,
        })
    }

    async fn send_key(
        &self,
        request: TerminalKeyInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get_mut(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        let bytes = key_input_bytes(request.key_code, request.modifiers, request.pressed);
        let input_changed = !bytes.is_empty();
        if !bytes.is_empty() {
            session.pty.write_input(&bytes).map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    session.sequence,
                )
            })?;
        }
        Self::drain_pty(session)?;
        let sequence = if input_changed {
            Self::next_sequence(&mut session.sequence)
        } else {
            session.sequence
        };
        Ok(TerminalInputResult {
            session_id: request.session_id,
            server_sequence: sequence,
            frame_sequence: sequence,
        })
    }

    async fn send_mouse(
        &self,
        request: TerminalMouseInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get_mut(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        let bytes = mouse_input_bytes(
            &session.terminal,
            request.x,
            request.y,
            request.buttons,
            request.button,
            request.pressed,
            request.wheel_x,
            request.wheel_y,
        );
        let input_changed = !bytes.is_empty();
        if !bytes.is_empty() {
            session.pty.write_input(&bytes).map_err(|error| {
                Self::error(
                    TerminalErrorCode::Internal,
                    error.to_string(),
                    session.sequence,
                )
            })?;
        }
        Self::drain_pty(session)?;
        let sequence = if input_changed {
            Self::next_sequence(&mut session.sequence)
        } else {
            session.sequence
        };
        Ok(TerminalInputResult {
            session_id: request.session_id,
            server_sequence: sequence,
            frame_sequence: sequence,
        })
    }

    async fn snapshot(
        &self,
        request: TerminalSnapshotRequest,
    ) -> Result<TerminalSnapshot, TerminalError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get_mut(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        Self::drain_pty(session)?;
        let sequence = session.sequence;
        if request.after_sequence > 0 && request.after_sequence >= sequence {
            return Ok(TerminalSnapshot {
                session_id: request.session_id,
                sequence,
                width: 0,
                height: 0,
                encoding: TerminalFrameEncoding::Png,
                kind: TerminalFrameKind::Full,
                stride: 0,
                tile_x: 0,
                tile_y: 0,
                tile_width: 0,
                tile_height: 0,
                payload: Vec::new(),
                complete: false,
                cursor: TerminalCursor { x: 0, y: 0, visible: false },
                selection_present: false,
                selection: TerminalSelection { anchor_x: 0, anchor_y: 0, focus_x: 0, focus_y: 0 },
                prompt: TerminalPromptMetadata {
                    prompt_present: false,
                    prompt: Self::empty_range(),
                    command_present: false,
                    command: Self::empty_range(),
                    command_status_present: false,
                    command_status: 0,
                },
            });
        }
        let (width, height, stride, payload) =
            Self::encode_png(&session.terminal).map_err(|mut error| {
                error.server_sequence = sequence;
                error
            })?;
        let max_bytes = request.max_frame_bytes.min(MAX_FRAME_BYTES);
        if max_bytes == 0 || payload.len() > max_bytes as usize {
            return Err(Self::error(
                TerminalErrorCode::CapacityExceeded,
                "PNG snapshot exceeds requested frame bound",
                sequence,
            ));
        }
        Ok(TerminalSnapshot {
            session_id: request.session_id,
            sequence,
            width,
            height,
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
            prompt: TerminalPromptMetadata {
                prompt_present: false,
                prompt: Self::empty_range(),
                command_present: false,
                command: Self::empty_range(),
                command_status_present: false,
                command_status: 0,
            },
        })
    }

    async fn cancel(
        &self,
        request: TerminalCancelRequest,
    ) -> Result<TerminalOperationResult, TerminalError> {
        let sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let session = sessions.get(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        Ok(TerminalOperationResult {
            session_id: request.session_id,
            state: TerminalState::Ready,
            server_sequence: session.sequence,
        })
    }

    async fn disconnect(
        &self,
        request: TerminalDisconnectRequest,
    ) -> Result<TerminalOperationResult, TerminalError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            Self::error(
                TerminalErrorCode::Internal,
                "terminal session lock poisoned",
                0,
            )
        })?;
        let mut session = sessions.remove(&request.session_id).ok_or_else(|| {
            Self::error(
                TerminalErrorCode::SessionNotFound,
                "terminal session not found",
                0,
            )
        })?;
        session.pty.kill().map_err(|error| {
            Self::error(
                TerminalErrorCode::Internal,
                error.to_string(),
                session.sequence,
            )
        })?;
        let sequence = Self::next_sequence(&mut session.sequence);
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
        stream.set_nodelay(true).ok();
        let service = service.clone();
        tokio::spawn(async move {
            let result = vox::acceptor_on(vox::transport::tcp::StreamLink::tcp(stream))
                .on_lane(TerminalDispatcher::new(service))
                .establish_connection()
                .await;
            match result {
                Ok(connection) => {
                    connection.closed().await;
                    connection.shutdown().ok();
                }
                Err(error) => tracing::warn!(?peer, ?error, "Vox terminal connection failed"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec_proto::terminal::TerminalClient;
    use teamy_terminal_core::Color;

    #[test]
    fn normalize_listen_address_accepts_loopback_shorthand() {
        assert_eq!(normalize_listen_address(":63946").unwrap(), "127.0.0.1:63946");
        assert_eq!(normalize_listen_address("63946").unwrap(), "127.0.0.1:63946");
        assert_eq!(normalize_listen_address("localhost:63946").unwrap(), "localhost:63946");
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
                },
                client_sequence: 1,
            })
            .await
            .expect("terminal session should connect");
        client
            .send_text(TerminalTextInput {
                session_id: connected.session_id.clone(),
                text: "1..100; Write-Host -ForegroundColor Cyan \"hello, world!\"; exit\r"
                    .to_string(),
                client_sequence: 2,
            })
            .await
            .expect("PowerShell puppet input should be accepted");

        let mut snapshot = None;
        for client_sequence in 3..=600 {
            let candidate = client
                .snapshot(TerminalSnapshotRequest {
                    session_id: connected.session_id.clone(),
                    after_sequence: 0,
                    max_frame_bytes: MAX_FRAME_BYTES,
                    client_sequence,
                })
                .await
                .expect("terminal PNG snapshot should be returned");
            let output_ready = service
                .sessions
                .lock()
                .expect("terminal session lock")
                .get(&connected.session_id)
                .is_some_and(|session| session.terminal.snapshot().visible_text().contains("100"));
            if output_ready && !candidate.payload.is_empty() {
                snapshot = Some(candidate);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let snapshot = snapshot.expect("PowerShell output should arrive in a bounded polling window");
        assert_eq!(snapshot.encoding, TerminalFrameEncoding::Png);
        assert_eq!(snapshot.kind, TerminalFrameKind::Full);
        assert!(snapshot.payload.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(snapshot.payload.len() <= MAX_FRAME_BYTES as usize);

        let sessions = service.sessions.lock().expect("terminal session lock");
        let session = sessions
            .get(&connected.session_id)
            .expect("session should remain available");
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
        drop(sessions);

        client
            .disconnect(TerminalDisconnectRequest {
                session_id: connected.session_id,
                reason: "test complete".to_string(),
                client_sequence: 4,
            })
            .await
            .expect("terminal session should disconnect");
        connection.shutdown().ok();
        server_guard.shutdown().ok();
    }

    #[test]
    fn key_input_uses_terminal_control_sequences_without_hardcoded_chords() {
        assert_eq!(key_input_bytes(65, GLFW_MOD_CONTROL, true), vec![0x01]);
        assert_eq!(key_input_bytes(67, GLFW_MOD_CONTROL, true), vec![0x03]);
        assert_eq!(key_input_bytes(GLFW_KEY_BACKSPACE, GLFW_MOD_CONTROL, true), vec![0x17]);
        assert_eq!(
            key_input_bytes(GLFW_KEY_RIGHT, GLFW_MOD_CONTROL, true),
            b"\x1b[1;5C".to_vec()
        );
        assert_eq!(key_input_bytes(GLFW_KEY_RIGHT, 0, false), Vec::<u8>::new());
    }

    #[test]
    fn unmodified_character_key_codes_are_supported_for_protocol_tests() {
        assert_eq!(key_input_bytes(65, 0, true), b"a".to_vec());
        assert_eq!(key_input_bytes(65, GLFW_MOD_SHIFT, true), b"A".to_vec());
        assert_eq!(key_input_bytes(GLFW_KEY_ENTER, 0, true), b"\r".to_vec());
    }

    #[test]
    fn mouse_input_respects_terminal_reporting_mode_and_uses_one_based_sgr_cells() {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 80, 24);
        assert!(mouse_input_bytes(&terminal, 2, 3, 0, 0, true, 0, 0).is_empty());
        terminal.process_bytes(b"\x1b[?1000;1006h");
        assert_eq!(
            mouse_input_bytes(&terminal, 2, 3, 1, 0, true, 0, 0),
            b"\x1b[<0;3;4M".to_vec()
        );
        assert_eq!(
            mouse_input_bytes(&terminal, 2, 3, 0, 0, false, 0, 1),
            b"\x1b[<64;3;4M".to_vec()
        );
    }
}
