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
            key_input: false,
            mouse_input: false,
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
        let mut idle_polls = 0;
        for _ in 0..200 {
            let event = session
                .pty
                .recv_timeout(Duration::from_millis(25))
                .map_err(|error| {
                    Self::error(
                        TerminalErrorCode::Internal,
                        error.to_string(),
                        session.sequence,
                    )
                })?;
            let Some(event) = event else {
                idle_polls += 1;
                if idle_polls >= 8 {
                    break;
                }
                continue;
            };
            idle_polls = 0;
            match event {
                PtyEvent::Output(bytes) => {
                    Self::answer_terminal_queries(&session.pty, &bytes)?;
                    session.debug_output.extend_from_slice(&bytes);
                    if session.debug_output.len() > 64 * 1024 {
                        let remove = session.debug_output.len() - 64 * 1024;
                        session.debug_output.drain(..remove);
                    }
                    session.terminal.process_bytes(&bytes);
                }
                PtyEvent::Exited(status) => {
                    session.closed = true;
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
        let mut text = request.text.into_bytes();
        if !text.ends_with(b"\r") && !text.ends_with(b"\n") {
            text.push(b'\r');
        }
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
        _request: TerminalKeyInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        Err(Self::error(
            TerminalErrorCode::UnsupportedCapability,
            "PNG endpoint accepts text input only",
            0,
        ))
    }

    async fn send_mouse(
        &self,
        _request: TerminalMouseInput,
    ) -> Result<TerminalInputResult, TerminalError> {
        Err(Self::error(
            TerminalErrorCode::UnsupportedCapability,
            "PNG endpoint accepts text input only",
            0,
        ))
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
                    key_input: false,
                    mouse_input: false,
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

        let snapshot = client
            .snapshot(TerminalSnapshotRequest {
                session_id: connected.session_id.clone(),
                after_sequence: 0,
                max_frame_bytes: MAX_FRAME_BYTES,
                client_sequence: 3,
            })
            .await
            .expect("terminal PNG snapshot should be returned");
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
}
