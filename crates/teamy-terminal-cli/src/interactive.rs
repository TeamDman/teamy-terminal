use crate::vox_server::mouse_input_bytes;
use arboard::Clipboard;
use eyre::Context;
use eyre::Result;
use std::fs::File;
use std::io::BufWriter;
use std::io::Cursor;
use std::path::Path;
use std::sync::Once;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use std::time::Instant;
use teamy_cancellation::CancellationToken;
use teamy_terminal_control::ControlAction;
use teamy_terminal_control::ControlCall;
use teamy_terminal_control::ControlKey;
use teamy_terminal_control::ControlPhysicalKey;
use teamy_terminal_control::ControlResponse;
use teamy_terminal_control::ControlServer;
use teamy_terminal_control::InstanceInfo;
use teamy_terminal_core::ApprovedPaste;
use teamy_terminal_core::Cell;
use teamy_terminal_core::CursorPosition;
use teamy_terminal_core::CursorShape;
use teamy_terminal_core::MULTILINE_PASTE_APPROVE_LABEL;
use teamy_terminal_core::MULTILINE_PASTE_CANCEL_LABEL;
use teamy_terminal_core::MULTILINE_PASTE_PREVIEW_LABEL;
use teamy_terminal_core::MULTILINE_PASTE_WARNING;
use teamy_terminal_core::MULTILINE_PASTE_WARNING_TITLE;
use teamy_terminal_core::PasteConfirmation;
use teamy_terminal_core::PasteGuard;
use teamy_terminal_core::PasteRequest;
use teamy_terminal_core::PromptMetadata;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;
use teamy_terminal_core::TerminalSnapshot;
#[cfg(all(test, feature = "dirty_rendering"))]
use teamy_terminal_font::CellRect;
use teamy_terminal_font::DEFAULT_PIXEL_SIZE;
#[cfg(feature = "dirty_rendering")]
use teamy_terminal_font::RasterizedFrame;
use teamy_terminal_font::TerminalFont;
#[cfg(feature = "dirty_rendering")]
use teamy_terminal_font::damage_regions;
use teamy_terminal_pty::PtyConfig;
use teamy_terminal_pty::PtyEvent;
use teamy_terminal_pty::PtySession;
#[cfg(feature = "dirty_rendering")]
use teamy_terminal_renderer::PixelRect;
use teamy_terminal_renderer::VulkanRenderer;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::ElementState;
use winit::event::KeyEvent;
use winit::event::MouseButton;
use winit::event::MouseScrollDelta;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::event_loop::ControlFlow;
use winit::event_loop::EventLoop;
use winit::keyboard::Key;
use winit::keyboard::ModifiersState;
use winit::keyboard::NamedKey;
#[cfg(windows)]
use winit::platform::windows::WindowAttributesExtWindows;
#[cfg(windows)]
use winit::platform::windows::WindowExtWindows;
use winit::window::Icon;
use winit::window::Window;
use winit::window::WindowId;

const INITIAL_WINDOW_WIDTH: u32 = 1280;
const INITIAL_WINDOW_HEIGHT: u32 = 720;
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../resources/main.png");
const MIN_FONT_PIXEL_SIZE: f32 = 8.0;
const MAX_FONT_PIXEL_SIZE: f32 = 32.0;
const FONT_PIXEL_SIZE_STEP: f32 = 1.0;
const EVENT_LOOP_POLL_INTERVAL: Duration = Duration::from_millis(8);
const MAX_PTY_DRAIN_EVENTS_PER_TURN: usize = 32;
const MAX_PTY_DRAIN_BYTES_PER_TURN: usize = 512 * 1024;
static EVENT_LOOP_LOGGED: Once = Once::new();
static PTY_DRAIN_LOGGED: Once = Once::new();
static CONTROL_DRAIN_LOGGED: Once = Once::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasteDialogChoice {
    PasteAnyway,
    Cancel,
}

impl PasteDialogChoice {
    const fn toggled(self) -> Self {
        match self {
            Self::PasteAnyway => Self::Cancel,
            Self::Cancel => Self::PasteAnyway,
        }
    }
}

#[derive(Debug)]
struct PasteDialog {
    confirmation: PasteConfirmation,
    choice: PasteDialogChoice,
}

impl PasteDialog {
    const fn new(confirmation: PasteConfirmation) -> Self {
        Self {
            confirmation,
            choice: PasteDialogChoice::Cancel,
        }
    }

    fn handle_key(&mut self, key: &Key) -> Option<PasteDialogChoice> {
        match key {
            Key::Named(NamedKey::Enter) => Some(self.choice),
            Key::Named(NamedKey::Escape) => Some(PasteDialogChoice::Cancel),
            Key::Named(
                NamedKey::Tab
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::ArrowUp
                | NamedKey::ArrowDown,
            ) => {
                self.choice = self.choice.toggled();
                None
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PasteDialogLayout {
    button_row: u16,
    paste_start: u16,
    paste_end: u16,
    cancel_start: u16,
    cancel_end: u16,
}

impl PasteDialogLayout {
    fn for_grid(columns: u16, rows: u16) -> Self {
        let paste_width = u16::try_from(MULTILINE_PASTE_APPROVE_LABEL.len() + 4)
            .unwrap_or(u16::MAX)
            .min(columns);
        let cancel_width =
            u16::try_from(MULTILINE_PASTE_CANCEL_LABEL.len() + 4).unwrap_or(u16::MAX);
        let cancel_start = paste_width.saturating_add(2).min(columns);
        Self {
            button_row: rows.saturating_sub(1),
            paste_start: 0,
            paste_end: paste_width,
            cancel_start,
            cancel_end: cancel_start.saturating_add(cancel_width).min(columns),
        }
    }

    const fn choice_at(self, position: CursorPosition) -> Option<PasteDialogChoice> {
        if position.row != self.button_row {
            return None;
        }
        if position.column >= self.paste_start && position.column < self.paste_end {
            Some(PasteDialogChoice::PasteAnyway)
        } else if position.column >= self.cancel_start && position.column < self.cancel_end {
            Some(PasteDialogChoice::Cancel)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecondaryClickAction {
    CopySelection,
    GuardedPaste,
}

const fn secondary_click_action(has_selection: bool) -> SecondaryClickAction {
    if has_selection {
        SecondaryClickAction::CopySelection
    } else {
        SecondaryClickAction::GuardedPaste
    }
}

/// Run the standalone interactive Vulkan terminal.
pub fn run(cancellation_token: &CancellationToken, initially_hidden: bool) -> Result<()> {
    let event_loop = EventLoop::new().wrap_err("failed to create the terminal event loop")?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(
        Instant::now() + EVENT_LOOP_POLL_INTERVAL,
    ));
    let mut application =
        InteractiveApplication::new(cancellation_token.clone(), initially_hidden)?;
    event_loop
        .run_app(&mut application)
        .wrap_err("the terminal event loop failed")?;
    application.finish()
}

fn load_window_icon() -> Result<Icon> {
    let mut decoder = png::Decoder::new(Cursor::new(WINDOW_ICON_PNG));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .wrap_err("failed to read the embedded terminal window icon")?;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| eyre::eyre!("embedded terminal window icon is too large to decode"))?;
    let mut rgba = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut rgba)
        .wrap_err("failed to decode the embedded terminal window icon")?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        eyre::bail!(
            "embedded terminal window icon decoded as {:?}/{:?}, expected RGBA8",
            info.color_type,
            info.bit_depth
        );
    }
    rgba.truncate(info.buffer_size());
    Icon::from_rgba(rgba, info.width, info.height)
        .map_err(|error| eyre::eyre!("failed to construct the terminal window icon: {error:?}"))
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent event-loop flags can validly coexist; an enum would admit fewer real states"
)]
struct InteractiveApplication {
    cancellation_token: CancellationToken,
    session: Option<PtySession>,
    terminal: TerminalSession,
    font: TerminalFont,
    font_pixel_size: f32,
    window: Option<Window>,
    renderer: Option<VulkanRenderer>,
    modifiers: ModifiersState,
    selecting: bool,
    pressed_mouse_buttons: u8,
    last_cursor_position: Option<winit::dpi::PhysicalPosition<f64>>,
    paste_guard: PasteGuard,
    paste_dialog: Option<PasteDialog>,
    suppress_dialog_key_until_release: bool,
    dirty: bool,
    resize_pending: bool,
    #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
    last_rendered_snapshot: Option<TerminalSnapshot>,
    #[cfg(feature = "dirty_rendering")]
    rendered_frame: Option<RasterizedFrame>,
    failure: Option<String>,
    control_server: Option<ControlServer>,
    control_calls: Option<Receiver<ControlCall>>,
    initially_hidden: bool,
}

impl InteractiveApplication {
    fn new(cancellation_token: CancellationToken, initially_hidden: bool) -> Result<Self> {
        let config = PtyConfig::interactive();
        let session = PtySession::spawn(&config)
            .wrap_err("failed to launch default-profile pwsh.exe through portable-pty")?;
        Ok(Self {
            cancellation_token,
            session: Some(session),
            terminal: TerminalSession::new(TerminalLimits::default(), 120, 36),
            font: TerminalFont::embedded().wrap_err("failed to load embedded Caskaydia font")?,
            font_pixel_size: DEFAULT_PIXEL_SIZE,
            window: None,
            renderer: None,
            modifiers: ModifiersState::default(),
            selecting: false,
            pressed_mouse_buttons: 0,
            last_cursor_position: None,
            paste_guard: PasteGuard::default(),
            paste_dialog: None,
            suppress_dialog_key_until_release: false,
            dirty: true,
            resize_pending: false,
            #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
            last_rendered_snapshot: None,
            #[cfg(feature = "dirty_rendering")]
            rendered_frame: None,
            failure: None,
            control_server: None,
            control_calls: None,
            initially_hidden,
        })
    }

    fn finish(self) -> Result<()> {
        if let Some(failure) = self.failure {
            return Err(eyre::eyre!(failure));
        }
        Ok(())
    }

    fn fail(&mut self, error: impl std::fmt::Display, event_loop: &ActiveEventLoop) {
        if self.failure.is_none() {
            self.failure = Some(error.to_string());
        }
        event_loop.exit();
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let _span = tracing::info_span!("create_terminal_window").entered();
        if self.window.is_some() {
            return Ok(());
        }
        let icon = load_window_icon()?;
        let mut attributes = Window::default_attributes()
            .with_title("Teamy Terminal — Vulkan")
            .with_inner_size(PhysicalSize::new(
                INITIAL_WINDOW_WIDTH,
                INITIAL_WINDOW_HEIGHT,
            ))
            .with_window_icon(Some(icon));
        if self.initially_hidden {
            attributes = attributes.with_visible(false);
            #[cfg(windows)]
            {
                attributes = attributes.with_skip_taskbar(true);
            }
        }
        let window = event_loop
            .create_window(attributes)
            .wrap_err("failed to create the terminal window")?;
        let renderer = VulkanRenderer::new(&window).map_err(|error| {
            eyre::eyre!(
                "graphics construction failed; Vulkan is required and no CPU rendering fallback exists: {error:#}"
            )
        })?;
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.resize_terminal()?;
        let instance = InstanceInfo::current("Teamy Terminal — Vulkan");
        let (calls, receiver) = std::sync::mpsc::sync_channel(64);
        let server = ControlServer::start(&instance, calls)
            .wrap_err("failed to start the Teamy Terminal Vox control service")?;
        tracing::info!(instance_id = instance.id, endpoint = %instance.endpoint, "Teamy Terminal Vox control service registered");
        self.control_calls = Some(receiver);
        self.control_server = Some(server);
        Ok(())
    }

    fn resize_terminal(&mut self) -> Result<()> {
        let _span = tracing::info_span!("resize_terminal").entered();
        let Some(renderer) = self.renderer.as_ref() else {
            return Ok(());
        };
        let extent = renderer.extent();
        let columns = u16::try_from(
            (extent.width / self.font.cell_width())
                .max(1)
                .min(u32::from(self.terminal.limits().max_columns)),
        )
        .expect("bounded terminal columns fit in u16");
        let rows = u16::try_from(
            (extent.height / self.font.cell_height())
                .max(1)
                .min(u32::from(self.terminal.limits().max_rows)),
        )
        .expect("bounded terminal rows fit in u16");
        self.terminal.resize(columns, rows);
        if let Some(session) = self.session.as_ref() {
            session.resize(
                columns,
                rows,
                u16::try_from(extent.width.min(u32::from(u16::MAX)))
                    .expect("clamped PTY pixel width fits in u16"),
                u16::try_from(extent.height.min(u32::from(u16::MAX)))
                    .expect("clamped PTY pixel height fits in u16"),
            )?;
        }
        self.dirty = true;
        #[cfg(feature = "dirty_rendering")]
        let _ = self.rendered_frame.take();
        Ok(())
    }

    fn adjust_font_size(&mut self, direction: f32) -> Result<()> {
        let next_size = adjusted_font_pixel_size(self.font_pixel_size, direction);
        if (next_size - self.font_pixel_size).abs() < f32::EPSILON {
            return Ok(());
        }

        self.font = TerminalFont::new(next_size)
            .wrap_err("failed to rebuild the terminal font at the requested size")?;
        self.font_pixel_size = next_size;
        #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
        let _ = self.last_rendered_snapshot.take();
        #[cfg(feature = "dirty_rendering")]
        let _ = self.rendered_frame.take();
        self.resize_terminal()
    }

    fn drain_pty(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(feature = "extended_observability")]
        let mut process_span: Option<tracing::Span> = None;
        let mut drained_events = 0;
        let mut drained_bytes = 0;
        while self.session.is_some() {
            if drained_events >= MAX_PTY_DRAIN_EVENTS_PER_TURN
                || drained_bytes >= MAX_PTY_DRAIN_BYTES_PER_TURN
            {
                break;
            }
            let event = match self
                .session
                .as_ref()
                .expect("session remains present during PTY drain")
                .try_recv()
            {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    self.fail(error, event_loop);
                    return;
                }
            };
            #[cfg(feature = "extended_observability")]
            if process_span.is_none() {
                process_span = Some(tracing::debug_span!("process_pty_output"));
            }
            #[cfg(feature = "extended_observability")]
            let _process_span = process_span
                .as_ref()
                .expect("PTY process span is initialized for an event")
                .enter();
            match event {
                PtyEvent::Output(bytes) => {
                    drained_bytes = drained_bytes.saturating_add(bytes.len());
                    let win32_input_mode_before = self.terminal.win32_input_mode_enabled();
                    self.terminal.process_bytes(&bytes);
                    let win32_input_mode_after = self.terminal.win32_input_mode_enabled();
                    if win32_input_mode_before != win32_input_mode_after {
                        tracing::info!(
                            enabled = win32_input_mode_after,
                            "ConPTY Win32 input mode changed"
                        );
                    }
                    let responses: Vec<Vec<u8>> = self.terminal.drain_responses().collect();
                    if let Some(session) = self.session.as_ref() {
                        for response in responses {
                            if let Err(error) = session.write_input(&response) {
                                self.fail(error, event_loop);
                                return;
                            }
                        }
                    }
                    self.dirty = true;
                }
                PtyEvent::Exited(exit) => {
                    if !exit.success {
                        self.failure = Some(format!(
                            "PowerShell exited unsuccessfully with code {:?}",
                            exit.code
                        ));
                    }
                    event_loop.exit();
                    return;
                }
                PtyEvent::Error(error) => {
                    self.fail(error, event_loop);
                    return;
                }
            }
            drained_events += 1;
        }
    }

    fn drain_control(&mut self, event_loop: &ActiveEventLoop) {
        loop {
            let result = self
                .control_calls
                .as_ref()
                .expect("control receiver remains present while the application runs")
                .try_recv();
            match result {
                Ok(call) => {
                    tracing::debug!("Teamy Terminal control request received");
                    self.handle_control_call(call, event_loop);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.fail("Teamy Terminal control service disconnected", event_loop);
                    return;
                }
            }
            if self.failure.is_some() {
                return;
            }
        }
    }

    fn handle_control_call(&mut self, call: ControlCall, event_loop: &ActiveEventLoop) {
        let ControlCall {
            operation,
            responder,
        } = call;
        let should_exit = matches!(operation, ControlAction::Close);
        let result = match operation {
            ControlAction::Ping => Ok(ControlResponse::Ok),
            ControlAction::SendText { text } => {
                self.write_control_text(&text).map(|()| ControlResponse::Ok)
            }
            ControlAction::SendKeys { keys } => {
                self.write_control_keys(&keys).map(|()| ControlResponse::Ok)
            }
            ControlAction::Resize { width, height } => {
                if width == 0 || height == 0 {
                    Err(eyre::eyre!("control resize dimensions must be non-zero"))
                } else if let Some(window) = self.window.as_ref() {
                    let _ = window.request_inner_size(PhysicalSize::new(width, height));
                    Ok(ControlResponse::Ok)
                } else {
                    Err(eyre::eyre!("terminal window is not initialized"))
                }
            }
            ControlAction::Select {
                anchor_column,
                anchor_row,
                focus_column,
                focus_row,
            } => {
                self.terminal.begin_selection(CursorPosition {
                    column: anchor_column,
                    row: anchor_row,
                });
                self.terminal.update_selection(CursorPosition {
                    column: focus_column,
                    row: focus_row,
                });
                self.dirty = true;
                Ok(ControlResponse::Ok)
            }
            ControlAction::CopySelection => self
                .copy_selection_text()
                .map(|text| ControlResponse::Content { text }),
            ControlAction::Zoom { steps } => {
                self.adjust_font_steps(steps).map(|()| ControlResponse::Ok)
            }
            ControlAction::SetVisible { visible } => self
                .set_window_visible(visible)
                .map(|()| ControlResponse::Ok),
            ControlAction::Screenshot { path } => self
                .write_screenshot(Path::new(&path))
                .map(|()| ControlResponse::Ok),
            ControlAction::GetContent => Ok(ControlResponse::Content {
                text: self.terminal.snapshot().visible_text(),
            }),
            ControlAction::Close => {
                if let Some(session) = self.session.as_ref() {
                    session
                        .kill()
                        .map_err(|error| eyre::eyre!("failed to close PowerShell: {error}"))
                        .map(|()| ControlResponse::Ok)
                } else {
                    Ok(ControlResponse::Ok)
                }
            }
        };
        let response = result.map_err(|error| error.to_string());
        let _ = responder.send(response);
        if should_exit {
            event_loop.exit();
        }
    }

    fn write_control_keys(&mut self, keys: &[ControlKey]) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| eyre::eyre!("PowerShell session is no longer running"))?;
        let win32_input_mode = self.terminal.win32_input_mode_enabled();
        let mut pressed = Vec::new();
        tracing::debug!(
            win32_input_mode,
            key_count = keys.len(),
            "encoding control-plane key actions"
        );
        let result = (|| {
            for key in keys {
                match key {
                    ControlKey::KeyDown { key } => {
                        if !pressed.contains(key) {
                            pressed.push(key.clone());
                        }
                        let bytes = if win32_input_mode {
                            win32_physical_key_event_bytes(key, true, &pressed)?
                        } else {
                            legacy_physical_key_bytes(key, true, &pressed)
                        };
                        if !bytes.is_empty() {
                            session.write_input(&bytes)?;
                        }
                    }
                    ControlKey::KeyUp { key } => {
                        let bytes = if win32_input_mode {
                            win32_physical_key_event_bytes(key, false, &pressed)?
                        } else {
                            legacy_physical_key_bytes(key, false, &pressed)
                        };
                        if !bytes.is_empty() {
                            session.write_input(&bytes)?;
                        }
                        pressed.retain(|pressed_key| pressed_key != key);
                    }
                    ControlKey::Delay { milliseconds } => {
                        std::thread::sleep(Duration::from_millis(u64::from(*milliseconds)));
                    }
                    _ => {
                        let bytes = if win32_input_mode {
                            win32_control_key_bytes(key)?
                        } else {
                            control_key_bytes(key)?
                        };
                        session.write_input(&bytes)?;
                    }
                }
            }
            Ok(())
        })();

        let release_result = release_pressed_keys(session, win32_input_mode, &mut pressed);
        result.and(release_result)
    }

    fn write_control_text(&self, text: &str) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| eyre::eyre!("PowerShell session is no longer running"))?;
        let bytes = if self.terminal.win32_input_mode_enabled() {
            win32_text_bytes(text)
        } else {
            text.as_bytes().to_vec()
        };
        session.write_input(&bytes)
    }

    fn write_screenshot(&mut self, path: &Path) -> Result<()> {
        let extent = self
            .renderer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("terminal renderer is not initialized"))?
            .extent();
        let snapshot = self.presentation_snapshot();
        let frame = self.font.render(&snapshot, extent.width, extent.height);
        let file = File::create(path)
            .wrap_err_with(|| format!("failed to create screenshot at {}", path.display()))?;
        let writer = BufWriter::new(file);
        let mut encoder = png::Encoder::new(writer, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .wrap_err("failed to write screenshot PNG header")?;
        writer
            .write_image_data(&frame.pixels)
            .wrap_err("failed to write screenshot PNG pixels")?;
        Ok(())
    }

    fn adjust_font_steps(&mut self, steps: i16) -> Result<()> {
        let direction = f32::from(steps.signum());
        for _ in 0..steps.unsigned_abs() {
            self.adjust_font_size(direction)?;
        }
        Ok(())
    }

    fn set_window_visible(&self, visible: bool) -> Result<()> {
        let window = self
            .window
            .as_ref()
            .ok_or_else(|| eyre::eyre!("terminal window is not initialized"))?;
        window.set_visible(visible);
        #[cfg(windows)]
        window.set_skip_taskbar(!visible);
        Ok(())
    }

    fn presentation_snapshot(&self) -> TerminalSnapshot {
        self.paste_dialog.as_ref().map_or_else(
            || self.terminal.snapshot(),
            |dialog| {
                paste_dialog_snapshot(
                    self.terminal.columns(),
                    self.terminal.rows(),
                    &dialog.confirmation,
                    dialog.choice,
                )
            },
        )
    }

    fn request_native_clipboard_paste(&mut self) -> Result<()> {
        let mut clipboard =
            Clipboard::new().wrap_err("failed to open the system clipboard for paste")?;
        let text = clipboard
            .get_text()
            .wrap_err("failed to read text from the system clipboard")?;
        match self.paste_guard.request(text)? {
            PasteRequest::Ready(approved) => self.write_approved_paste(&approved),
            PasteRequest::ConfirmationRequired(confirmation) => {
                self.paste_dialog = Some(PasteDialog::new(confirmation));
                self.dirty = true;
                Ok(())
            }
        }
    }

    fn write_approved_paste(&self, approved: &ApprovedPaste) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| eyre::eyre!("PowerShell session is no longer running"))?;
        session.write_input(approved.text().as_bytes())
    }

    fn forward_child_mouse(
        &self,
        button: u8,
        pressed: bool,
        motion: bool,
        wheel_x: i32,
        wheel_y: i32,
    ) -> Result<bool> {
        if !self.terminal.mouse_reporting_enabled() {
            return Ok(false);
        }
        let Some(position) = self.last_cursor_position else {
            return Ok(true);
        };
        let Some(cell) = self.terminal_cell_at(position) else {
            return Ok(true);
        };
        let bytes = mouse_input_bytes(
            &self.terminal,
            cell.column,
            cell.row,
            self.pressed_mouse_buttons,
            button,
            pressed,
            motion,
            wheel_x,
            wheel_y,
        );
        if !bytes.is_empty() {
            let session = self
                .session
                .as_ref()
                .ok_or_else(|| eyre::eyre!("PowerShell session is no longer running"))?;
            session.write_input(&bytes)?;
        }
        Ok(true)
    }

    fn set_mouse_button_state(&mut self, button: MouseButton, pressed: bool) -> Option<u8> {
        let button = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Back => 3,
            MouseButton::Forward => 4,
            MouseButton::Other(value) => u8::try_from(value).ok()?.min(7),
        };
        let mask = 1_u8 << button;
        if pressed {
            self.pressed_mouse_buttons |= mask;
        } else {
            self.pressed_mouse_buttons &= !mask;
        }
        Some(button)
    }

    fn finish_paste_dialog(&mut self, choice: PasteDialogChoice) -> Result<()> {
        let id = self
            .paste_dialog
            .as_ref()
            .ok_or_else(|| eyre::eyre!("no paste confirmation is open"))?
            .confirmation
            .id();
        let approved = match choice {
            PasteDialogChoice::PasteAnyway => Some(self.paste_guard.approve(id)?),
            PasteDialogChoice::Cancel => {
                self.paste_guard.cancel(id)?;
                None
            }
        };
        self.paste_dialog = None;
        self.restore_terminal_focus();
        if let Some(approved) = approved.as_ref() {
            self.write_approved_paste(approved)?;
        }
        Ok(())
    }

    fn restore_terminal_focus(&mut self) {
        if let Some(window) = self.window.as_ref() {
            window.focus_window();
        }
        self.dirty = true;
    }

    fn handle_paste_dialog_key(&mut self, event: &KeyEvent) -> Result<bool> {
        if self.paste_dialog.is_none() {
            return Ok(false);
        }
        if event.state != ElementState::Pressed || event.repeat {
            return Ok(true);
        }
        let choice = self
            .paste_dialog
            .as_mut()
            .expect("paste dialog remains present while handling its key")
            .handle_key(&event.logical_key);
        self.dirty = true;
        if let Some(choice) = choice {
            self.suppress_dialog_key_until_release = true;
            self.finish_paste_dialog(choice)?;
        }
        Ok(true)
    }

    fn handle_paste_dialog_click(&mut self) -> Result<bool> {
        if self.paste_dialog.is_none() {
            return Ok(false);
        }
        let Some(position) = self.last_cursor_position else {
            return Ok(true);
        };
        let Some(cell) = self.terminal_cell_at(position) else {
            return Ok(true);
        };
        let layout = PasteDialogLayout::for_grid(self.terminal.columns(), self.terminal.rows());
        let Some(choice) = layout.choice_at(cell) else {
            return Ok(true);
        };
        self.paste_dialog
            .as_mut()
            .expect("paste dialog remains present after hit testing")
            .choice = choice;
        self.finish_paste_dialog(choice)?;
        Ok(true)
    }

    fn handle_secondary_click(&mut self) -> Result<()> {
        match secondary_click_action(self.terminal.selection().is_some()) {
            SecondaryClickAction::CopySelection => {
                let copied = self.copy_selection()?;
                debug_assert!(copied, "an active selection should have copyable text");
                Ok(())
            }
            SecondaryClickAction::GuardedPaste => self.request_native_clipboard_paste(),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one event-loop render transaction keeps failure handling and frame-cache commits atomic"
    )]
    fn render(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(feature = "extended_observability")]
        let _span = tracing::debug_span!("render_terminal_frame").entered();
        if !self.dirty {
            return;
        }
        if self.resize_pending {
            let Some(window) = self.window.as_ref() else {
                return;
            };
            let Some(renderer) = self.renderer.as_mut() else {
                return;
            };
            if let Err(error) = renderer.recreate(window) {
                self.fail(error, event_loop);
                return;
            }
            self.resize_pending = false;
            if let Err(error) = self.resize_terminal() {
                self.fail(error, event_loop);
                return;
            }
        }
        let snapshot = self.presentation_snapshot();
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let extent = renderer.extent();
        #[cfg(feature = "extended_observability")]
        let frame_stats = frame_stats(self.last_rendered_snapshot.as_ref(), &snapshot);
        #[cfg(feature = "dirty_rendering")]
        let (cell_regions, full_refresh) = {
            let (regions, full_refresh) =
                damage_regions(self.last_rendered_snapshot.as_ref(), &snapshot);
            let full_refresh = full_refresh
                || self.rendered_frame.as_ref().is_none_or(|frame| {
                    frame.width != extent.width || frame.height != extent.height
                });
            if !full_refresh && regions.is_empty() {
                self.dirty = false;
                return;
            }
            (regions, full_refresh)
        };
        let frame = {
            #[cfg(feature = "extended_observability")]
            let _span = tracing::debug_span!("rasterize_terminal_frame").entered();
            #[cfg(feature = "dirty_rendering")]
            {
                if full_refresh {
                    self.font.render(&snapshot, extent.width, extent.height)
                } else {
                    let mut frame = self
                        .rendered_frame
                        .take()
                        .expect("dirty rendering has a cached frame");
                    self.font.render_cells(&snapshot, &mut frame, &cell_regions);
                    frame
                }
            }
            #[cfg(not(feature = "dirty_rendering"))]
            {
                self.font.render(&snapshot, extent.width, extent.height)
            }
        };
        #[cfg(feature = "dirty_rendering")]
        let pixel_regions = if full_refresh {
            vec![PixelRect::full(frame.width, frame.height)]
        } else {
            cell_regions
                .iter()
                .copied()
                .map(|region| {
                    PixelRect::new(
                        u32::from(region.column) * self.font.cell_width(),
                        u32::from(region.row) * self.font.cell_height(),
                        u32::from(region.columns) * self.font.cell_width(),
                        u32::from(region.rows) * self.font.cell_height(),
                    )
                })
                .collect()
        };
        #[cfg(all(feature = "extended_observability", feature = "dirty_rendering"))]
        let rasterized_pixels = pixel_regions
            .iter()
            .map(|region| {
                usize::try_from(region.width).unwrap_or(0)
                    * usize::try_from(region.height).unwrap_or(0)
            })
            .sum::<usize>();
        #[cfg(all(feature = "extended_observability", not(feature = "dirty_rendering")))]
        let rasterized_pixels = frame.pixels.len() / 4;
        #[cfg(feature = "extended_observability")]
        let _present_span = tracing::debug_span!("upload_and_present_frame").entered();
        let result = {
            #[cfg(feature = "dirty_rendering")]
            {
                renderer.render_damage(&frame, &pixel_regions)
            }
            #[cfg(not(feature = "dirty_rendering"))]
            {
                renderer.render(&frame)
            }
        };
        #[cfg(feature = "extended_observability")]
        let uploaded_bytes = renderer.last_upload_bytes();
        #[cfg(all(feature = "extended_observability", feature = "dirty_rendering"))]
        tracing::debug!(
            visible_cells = snapshot.cells.len(),
            changed_cells = frame_stats.0,
            cursor_changed = frame_stats.1,
            rasterized_pixels,
            uploaded_bytes,
            dirty_region_count = pixel_regions.len(),
            uploaded_region_count = renderer.last_upload_region_count(),
            uploaded_full_refresh = renderer.last_upload_was_full_refresh(),
            "terminal frame stats"
        );
        #[cfg(all(feature = "extended_observability", not(feature = "dirty_rendering")))]
        tracing::debug!(
            visible_cells = snapshot.cells.len(),
            changed_cells = frame_stats.0,
            cursor_changed = frame_stats.1,
            rasterized_pixels,
            uploaded_bytes,
            "terminal frame stats"
        );
        match result {
            Ok(needs_recreate) => {
                self.dirty = false;
                if needs_recreate {
                    self.resize_pending = true;
                    self.dirty = true;
                }
                #[cfg(feature = "dirty_rendering")]
                let _ = self.rendered_frame.replace(frame);
                #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
                let _ = self.last_rendered_snapshot.replace(snapshot);
                #[cfg(feature = "extended_observability")]
                tracing::info!(message = "finished frame", tracy.frame_mark = true);
            }
            Err(error) => self.fail(error, event_loop),
        }
    }

    fn input(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        if self.suppress_dialog_key_until_release {
            if event.state == ElementState::Released {
                self.suppress_dialog_key_until_release = false;
            }
            return;
        }
        match self.handle_paste_dialog_key(event) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                self.fail(error, event_loop);
                return;
            }
        }
        if event.state != ElementState::Pressed {
            return;
        }
        let is_v = match &event.logical_key {
            Key::Character(character) => character.as_str().eq_ignore_ascii_case("v"),
            _ => false,
        };
        if self.modifiers.control_key() && is_v {
            if !event.repeat
                && let Err(error) = self.request_native_clipboard_paste()
            {
                self.fail(error, event_loop);
            }
            return;
        }
        let is_c = match &event.logical_key {
            Key::Character(character) => character.as_str().eq_ignore_ascii_case("c"),
            _ => false,
        };
        if self.modifiers.control_key()
            && is_c
            && (self.modifiers.shift_key() || self.terminal.selection().is_some())
        {
            match self.copy_selection() {
                Ok(true) => return,
                Ok(false) if self.modifiers.shift_key() => return,
                Ok(false) => {}
                Err(error) => {
                    self.fail(error, event_loop);
                    return;
                }
            }
        }
        let win32_input_mode = self.terminal.win32_input_mode_enabled();
        let bytes = match terminal_input(event, self.modifiers, win32_input_mode) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(error) => {
                self.fail(error, event_loop);
                return;
            }
        };
        if self.modifiers.control_key() {
            tracing::info!(
                key = ?event.logical_key,
                text = ?event.text,
                modifiers = ?self.modifiers,
                win32_input_mode,
                bytes = ?bytes,
                "encoded Ctrl-modified window key"
            );
        }
        if let Some(session) = self.session.as_ref()
            && let Err(error) = session.write_input(&bytes)
        {
            self.fail(error, event_loop);
        }
    }

    fn terminal_cell_at(
        &self,
        position: winit::dpi::PhysicalPosition<f64>,
    ) -> Option<CursorPosition> {
        terminal_cell_at_position(
            position,
            self.font.cell_width(),
            self.font.cell_height(),
            self.terminal.columns(),
            self.terminal.rows(),
        )
    }

    fn update_selection_from_position(&mut self, position: winit::dpi::PhysicalPosition<f64>) {
        if let Some(cell) = self.terminal_cell_at(position) {
            self.terminal.update_selection(cell);
            self.dirty = true;
        }
    }

    fn handle_cursor_moved(
        &mut self,
        position: winit::dpi::PhysicalPosition<f64>,
        event_loop: &ActiveEventLoop,
    ) {
        self.last_cursor_position = Some(position);
        if self.terminal.mouse_reporting_enabled() && self.pressed_mouse_buttons != 0 {
            self.selecting = false;
            let button =
                u8::try_from(self.pressed_mouse_buttons.trailing_zeros().min(7)).unwrap_or(0);
            if let Err(error) = self.forward_child_mouse(button, true, true, 0, 0) {
                self.fail(error, event_loop);
            }
        } else if self.selecting {
            self.update_selection_from_position(position);
        }
    }

    fn handle_reported_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        event_loop: &ActiveEventLoop,
    ) {
        self.selecting = false;
        let pressed = state == ElementState::Pressed;
        if let Some(button) = self.set_mouse_button_state(button, pressed)
            && let Err(error) = self.forward_child_mouse(button, pressed, false, 0, 0)
        {
            self.fail(error, event_loop);
        }
    }

    fn copy_selection(&mut self) -> Result<bool> {
        self.terminal.selected_text().map_or(Ok(false), |text| {
            self.copy_selection_text_for(text).map(|()| true)
        })
    }

    fn copy_selection_text(&mut self) -> Result<String> {
        let text = self
            .terminal
            .selected_text()
            .ok_or_else(|| eyre::eyre!("no active terminal selection to copy"))?;
        self.copy_selection_text_for(text.clone())?;
        Ok(text)
    }

    fn copy_selection_text_for(&mut self, text: String) -> Result<()> {
        let mut clipboard = Clipboard::new().wrap_err("failed to open the system clipboard")?;
        clipboard
            .set_text(text)
            .wrap_err("failed to copy terminal selection to the system clipboard")?;
        self.terminal.clear_selection();
        self.dirty = true;
        Ok(())
    }
}

impl ApplicationHandler for InteractiveApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.initialize(event_loop) {
            self.fail(error, event_loop);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(session) = self.session.as_ref() {
                    let _ = session.kill();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                self.resize_pending = true;
                self.dirty = true;
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.handle_cursor_moved(position, event_loop);
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if self.paste_dialog.is_some() => {
                if state == ElementState::Pressed
                    && let Err(error) = self.handle_paste_dialog_click()
                {
                    self.fail(error, event_loop);
                }
            }
            WindowEvent::MouseInput { .. } if self.paste_dialog.is_some() => {}
            WindowEvent::MouseInput { state, button, .. }
                if self.terminal.mouse_reporting_enabled() =>
            {
                self.handle_reported_mouse_input(state, button, event_loop);
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => {
                    self.set_mouse_button_state(MouseButton::Left, true);
                    if let Some(position) = self.last_cursor_position
                        && let Some(cell) = self.terminal_cell_at(position)
                    {
                        self.terminal.begin_selection(cell);
                        self.selecting = true;
                        self.dirty = true;
                    }
                }
                ElementState::Released => {
                    self.set_mouse_button_state(MouseButton::Left, false);
                    if self.selecting {
                        if let Some(position) = self.last_cursor_position {
                            self.update_selection_from_position(position);
                        }
                        self.selecting = false;
                        if self
                            .terminal
                            .selection()
                            .is_some_and(|selection| selection.anchor == selection.focus)
                        {
                            self.terminal.clear_selection();
                        }
                        self.dirty = true;
                    }
                }
            },
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                self.set_mouse_button_state(MouseButton::Right, true);
                if let Err(error) = self.handle_secondary_click() {
                    self.fail(error, event_loop);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Right,
                ..
            } => {
                self.set_mouse_button_state(MouseButton::Right, false);
            }
            WindowEvent::MouseWheel { delta, .. } if self.modifiers.control_key() => {
                if let Some(direction) = wheel_zoom_direction(&delta)
                    && let Err(error) = self.adjust_font_size(direction)
                {
                    self.fail(error, event_loop);
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.terminal.mouse_reporting_enabled() => {
                let (wheel_x, wheel_y) = mouse_wheel_steps(&delta);
                if let Err(error) = self.forward_child_mouse(0, true, false, wheel_x, wheel_y) {
                    self.fail(error, event_loop);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.input(&event, event_loop),
            WindowEvent::RedrawRequested => self.render(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        EVENT_LOOP_LOGGED
            .call_once(|| tracing::info!("Teamy Terminal event loop reached about_to_wait"));
        if self.cancellation_token.is_cancelled() {
            if let Some(session) = self.session.as_ref() {
                let _ = session.kill();
            }
            event_loop.exit();
            return;
        }
        self.drain_pty(event_loop);
        PTY_DRAIN_LOGGED
            .call_once(|| tracing::info!("Teamy Terminal completed its first PTY drain"));
        if self.failure.is_some() {
            return;
        }
        self.drain_control(event_loop);
        CONTROL_DRAIN_LOGGED
            .call_once(|| tracing::info!("Teamy Terminal completed its first control drain"));
        if self.dirty
            && let Some(window) = self.window.as_ref()
        {
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + EVENT_LOOP_POLL_INTERVAL,
        ));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(session) = self.session.as_ref() {
            let _ = session.kill();
        }
        self.renderer.take();
        self.window.take();
        self.control_calls.take();
        self.control_server.take();
        self.session.take();
    }
}

fn adjusted_font_pixel_size(current: f32, direction: f32) -> f32 {
    (current + direction.signum() * FONT_PIXEL_SIZE_STEP)
        .clamp(MIN_FONT_PIXEL_SIZE, MAX_FONT_PIXEL_SIZE)
}

fn wheel_zoom_direction(delta: &MouseScrollDelta) -> Option<f32> {
    let vertical = match delta {
        MouseScrollDelta::LineDelta(_, vertical) => f64::from(*vertical),
        MouseScrollDelta::PixelDelta(position) => position.y,
    };
    if vertical > 0.0 {
        Some(1.0)
    } else if vertical < 0.0 {
        Some(-1.0)
    } else {
        None
    }
}

fn mouse_wheel_steps(delta: &MouseScrollDelta) -> (i32, i32) {
    let (horizontal, vertical) = match delta {
        MouseScrollDelta::LineDelta(horizontal, vertical) => {
            (f64::from(*horizontal), f64::from(*vertical))
        }
        MouseScrollDelta::PixelDelta(position) => (position.x, position.y),
    };
    (mouse_wheel_step(horizontal), mouse_wheel_step(vertical))
}

fn mouse_wheel_step(value: f64) -> i32 {
    if value > 0.0 {
        1
    } else if value < 0.0 {
        -1
    } else {
        0
    }
}

fn terminal_cell_at_position(
    position: winit::dpi::PhysicalPosition<f64>,
    cell_width: u32,
    cell_height: u32,
    columns: u16,
    rows: u16,
) -> Option<CursorPosition> {
    if !position.x.is_finite()
        || !position.y.is_finite()
        || position.x < 0.0
        || position.y < 0.0
        || cell_width == 0
        || cell_height == 0
    {
        return None;
    }
    let cell_position = winit::dpi::PhysicalPosition::new(
        (position.x / f64::from(cell_width)).floor(),
        (position.y / f64::from(cell_height)).floor(),
    );
    if cell_position.x >= f64::from(columns) || cell_position.y >= f64::from(rows) {
        return None;
    }
    let cell_position = cell_position.cast::<u16>();
    Some(CursorPosition {
        column: cell_position.x,
        row: cell_position.y,
    })
}

#[cfg(feature = "extended_observability")]
fn frame_stats(previous: Option<&TerminalSnapshot>, current: &TerminalSnapshot) -> (usize, bool) {
    let changed_cells = previous.map_or(current.cells.len(), |previous| {
        if previous.columns != current.columns || previous.rows != current.rows {
            current.cells.len()
        } else {
            previous
                .cells
                .iter()
                .zip(&current.cells)
                .filter(|(previous, current)| previous != current)
                .count()
        }
    });
    let cursor_changed = previous.is_some_and(|previous| {
        previous.cursor != current.cursor || previous.cursor_visible != current.cursor_visible
    });
    (changed_cells, cursor_changed)
}

fn terminal_input(
    event: &KeyEvent,
    modifiers: ModifiersState,
    win32_input_mode: bool,
) -> Result<Option<Vec<u8>>> {
    if win32_input_mode {
        win32_terminal_input(event, modifiers)
    } else {
        terminal_input_key(&event.logical_key, event.text.as_deref(), modifiers)
    }
}

const LEFT_ALT_PRESSED: u32 = 0x0002;
const RIGHT_ALT_PRESSED: u32 = 0x0001;
const LEFT_CTRL_PRESSED: u32 = 0x0008;
const SHIFT_PRESSED: u32 = 0x0010;
const ENHANCED_KEY: u32 = 0x0100;

fn win32_key_event(
    virtual_key: u32,
    scan_code: u32,
    unicode_character: u32,
    key_down: bool,
    control_state: u32,
) -> Vec<u8> {
    format!(
        "\x1b[{virtual_key};{scan_code};{unicode_character};{};{control_state};1_",
        u8::from(key_down),
    )
    .into_bytes()
}

fn win32_key_press(
    virtual_key: u32,
    scan_code: u32,
    unicode_character: u32,
    control_state: u32,
) -> Vec<u8> {
    let mut bytes = win32_key_event(
        virtual_key,
        scan_code,
        unicode_character,
        true,
        control_state,
    );
    bytes.extend(win32_key_event(
        virtual_key,
        scan_code,
        0,
        false,
        control_state,
    ));
    bytes
}

fn win32_text_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len().saturating_mul(20));
    for character in text.chars() {
        if matches!(character, '\r' | '\n') {
            bytes.extend(win32_key_press(0x0D, 0x1C, u32::from('\r'), 0));
            continue;
        }
        if character == '\t' {
            bytes.extend(win32_key_press(0x09, 0x0F, u32::from('\t'), 0));
            continue;
        }
        let Some((virtual_key, scan_code)) = ascii_key_location(character) else {
            bytes.extend(character.to_string().as_bytes());
            continue;
        };
        let control_state = u32::from(character.is_ascii_uppercase()) * SHIFT_PRESSED;
        bytes.extend(win32_key_press(
            virtual_key,
            scan_code,
            u32::from(character),
            control_state,
        ));
    }
    bytes
}

fn release_pressed_keys(
    session: &PtySession,
    win32_input_mode: bool,
    pressed: &mut Vec<ControlPhysicalKey>,
) -> Result<()> {
    while let Some(key) = pressed.pop() {
        let bytes = if win32_input_mode {
            win32_physical_key_event_bytes(&key, false, pressed)?
        } else {
            legacy_physical_key_bytes(&key, false, pressed)
        };
        if !bytes.is_empty() {
            session.write_input(&bytes)?;
        }
    }
    Ok(())
}

fn physical_modifier_flags(key: &ControlPhysicalKey) -> u32 {
    match key {
        // ConPTY's Win32 input path distinguishes the left and right control
        // keys.  The keyboard event generated for Ctrl+C is the left-control
        // `WriteCtrlKey` form used by OpenConsole, so keep scripted Ctrl
        // state consistent with that form instead of claiming both keys.
        ControlPhysicalKey::Ctrl => LEFT_CTRL_PRESSED,
        ControlPhysicalKey::Shift => SHIFT_PRESSED,
        ControlPhysicalKey::Alt => LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED,
        _ => 0,
    }
}

fn physical_control_state(
    key: &ControlPhysicalKey,
    key_down: bool,
    pressed: &[ControlPhysicalKey],
) -> u32 {
    let mut state = pressed.iter().fold(0, |state, pressed_key| {
        state | physical_modifier_flags(pressed_key)
    });
    let flags = physical_modifier_flags(key);
    if key_down {
        state |= flags;
    } else {
        state &= !flags;
    }
    state
}

pub(crate) fn win32_physical_key_event_bytes(
    key: &ControlPhysicalKey,
    key_down: bool,
    pressed: &[ControlPhysicalKey],
) -> Result<Vec<u8>> {
    // Winit reports modifier transitions through ModifiersChanged rather than
    // as terminal key events. ConPTY's Win32 input mode likewise expects the
    // modifier state on the following key record, so track these actions in
    // `pressed` without emitting standalone modifier records.
    if matches!(
        key,
        ControlPhysicalKey::Ctrl | ControlPhysicalKey::Shift | ControlPhysicalKey::Alt
    ) {
        return Ok(Vec::new());
    }
    let control_state = physical_control_state(key, key_down, pressed);
    let (virtual_key, scan_code, mut unicode_character, enhanced) = match key {
        ControlPhysicalKey::Character { text } => {
            let mut characters = text.chars();
            let character = characters
                .next()
                .ok_or_else(|| eyre::eyre!("physical character key cannot be empty"))?;
            if characters.next().is_some() {
                eyre::bail!("physical character key must contain one Unicode scalar")
            }
            let (virtual_key, scan_code) = ascii_key_location(character).unwrap_or((0, 0));
            (virtual_key, scan_code, u32::from(character), false)
        }
        ControlPhysicalKey::Ctrl => (0x11, 0x1D, 0, false),
        ControlPhysicalKey::Shift => (0x10, 0x2A, 0, false),
        ControlPhysicalKey::Alt => (0x12, 0x38, 0, false),
        ControlPhysicalKey::Enter => (0x0D, 0x1C, u32::from('\r'), false),
        ControlPhysicalKey::Tab => (0x09, 0x0F, u32::from('\t'), false),
        ControlPhysicalKey::Backspace => (0x08, 0x0E, 0x08, false),
        ControlPhysicalKey::Escape => (0x1B, 0x01, 0x1B, false),
        ControlPhysicalKey::ArrowUp => (0x26, 0x48, 0, true),
        ControlPhysicalKey::ArrowDown => (0x28, 0x50, 0, true),
        ControlPhysicalKey::ArrowRight => (0x27, 0x4D, 0, true),
        ControlPhysicalKey::ArrowLeft => (0x25, 0x4B, 0, true),
        ControlPhysicalKey::Home => (0x24, 0x47, 0, true),
        ControlPhysicalKey::End => (0x23, 0x4F, 0, true),
        ControlPhysicalKey::Delete => (0x2E, 0x53, 0, true),
        ControlPhysicalKey::PageUp => (0x21, 0x49, 0, true),
        ControlPhysicalKey::PageDown => (0x22, 0x51, 0, true),
    };
    if control_state & LEFT_CTRL_PRESSED != 0
        && let Some(character) = char::from_u32(unicode_character)
        && let Some(control) = control_character_byte(&character.to_string())
    {
        unicode_character = u32::from(control);
    }
    if !key_down {
        unicode_character = 0;
    }
    Ok(win32_key_event(
        virtual_key,
        scan_code,
        unicode_character,
        key_down,
        control_state | if enhanced { ENHANCED_KEY } else { 0 },
    ))
}

fn legacy_physical_key_bytes(
    key: &ControlPhysicalKey,
    key_down: bool,
    pressed: &[ControlPhysicalKey],
) -> Vec<u8> {
    if !key_down {
        return Vec::new();
    }
    let ctrl = pressed.contains(&ControlPhysicalKey::Ctrl);
    let shift = pressed.contains(&ControlPhysicalKey::Shift);
    match key {
        ControlPhysicalKey::Character { text } if ctrl => {
            let text = text.to_ascii_lowercase();
            control_character_byte(&text)
                .map_or_else(|| text.into_bytes(), |character| vec![character])
        }
        ControlPhysicalKey::Character { text } if shift => text.to_ascii_uppercase().into_bytes(),
        ControlPhysicalKey::Character { text } => text.as_bytes().to_vec(),
        ControlPhysicalKey::Ctrl | ControlPhysicalKey::Shift | ControlPhysicalKey::Alt => {
            Vec::<u8>::new()
        }
        ControlPhysicalKey::Enter => b"\r".to_vec(),
        ControlPhysicalKey::Tab => b"\t".to_vec(),
        ControlPhysicalKey::Backspace => vec![0x7F],
        ControlPhysicalKey::Escape => vec![0x1B],
        ControlPhysicalKey::ArrowUp => b"\x1b[A".to_vec(),
        ControlPhysicalKey::ArrowDown => b"\x1b[B".to_vec(),
        ControlPhysicalKey::ArrowRight => b"\x1b[C".to_vec(),
        ControlPhysicalKey::ArrowLeft => b"\x1b[D".to_vec(),
        ControlPhysicalKey::Home => b"\x1b[H".to_vec(),
        ControlPhysicalKey::End => b"\x1b[F".to_vec(),
        ControlPhysicalKey::Delete => b"\x1b[3~".to_vec(),
        ControlPhysicalKey::PageUp => b"\x1b[5~".to_vec(),
        ControlPhysicalKey::PageDown => b"\x1b[6~".to_vec(),
    }
}

fn win32_control_key_bytes(key: &ControlKey) -> Result<Vec<u8>> {
    let bytes = match key {
        ControlKey::Character { text } => win32_text_bytes(text),
        ControlKey::Paste { text } => text.as_bytes().to_vec(),
        ControlKey::Enter => win32_key_press(0x0D, 0x1C, u32::from('\r'), 0),
        ControlKey::Tab => win32_key_press(0x09, 0x0F, u32::from('\t'), 0),
        ControlKey::Backspace => win32_key_press(0x08, 0x0E, 0x08, 0),
        ControlKey::Escape => win32_key_press(0x1B, 0x01, 0x1B, 0),
        ControlKey::ArrowUp => win32_key_press(0x26, 0x48, 0, ENHANCED_KEY),
        ControlKey::ArrowDown => win32_key_press(0x28, 0x50, 0, ENHANCED_KEY),
        ControlKey::ArrowRight => win32_key_press(0x27, 0x4D, 0, ENHANCED_KEY),
        ControlKey::ArrowLeft => win32_key_press(0x25, 0x4B, 0, ENHANCED_KEY),
        ControlKey::Home => win32_key_press(0x24, 0x47, 0, ENHANCED_KEY),
        ControlKey::End => win32_key_press(0x23, 0x4F, 0, ENHANCED_KEY),
        ControlKey::Delete => win32_key_press(0x2E, 0x53, 0, ENHANCED_KEY),
        ControlKey::PageUp => win32_key_press(0x21, 0x49, 0, ENHANCED_KEY),
        ControlKey::PageDown => win32_key_press(0x22, 0x51, 0, ENHANCED_KEY),
        ControlKey::CtrlA => win32_ctrl_letter('a', 0x01),
        ControlKey::CtrlArrowUp => win32_key_press(0x26, 0x48, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlArrowDown => win32_key_press(0x28, 0x50, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlArrowRight => win32_key_press(0x27, 0x4D, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlArrowLeft => win32_key_press(0x25, 0x4B, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlHome => win32_key_press(0x24, 0x47, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlEnd => win32_key_press(0x23, 0x4F, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlDelete => win32_key_press(0x2E, 0x53, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlPageUp => win32_key_press(0x21, 0x49, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlPageDown => win32_key_press(0x22, 0x51, 0, LEFT_CTRL_PRESSED),
        ControlKey::CtrlCharacter { character } => {
            let Some(control) = control_character_byte(&character.to_string()) else {
                eyre::bail!("unsupported control character: {character:?}")
            };
            if character.is_ascii_alphabetic() {
                win32_ctrl_letter(character.to_ascii_lowercase(), control)
            } else {
                vec![control]
            }
        }
        ControlKey::CtrlC => win32_ctrl_letter('c', 0x03),
        ControlKey::CtrlD => win32_ctrl_letter('d', 0x04),
        ControlKey::CtrlZ => win32_ctrl_letter('z', 0x1A),
        ControlKey::CtrlL => win32_ctrl_letter('l', 0x0C),
        ControlKey::KeyDown { .. } | ControlKey::KeyUp { .. } | ControlKey::Delay { .. } => {
            eyre::bail!("scripted key actions must be handled by the sequence writer")
        }
    };
    if bytes.len() > teamy_terminal_control::MAX_INPUT_BYTES {
        eyre::bail!("control key text exceeds the input limit")
    }
    Ok(bytes)
}

fn win32_ctrl_letter(character: char, unicode_character: u8) -> Vec<u8> {
    let (virtual_key, scan_code) = ascii_key_location(character).unwrap_or((0, 0));
    win32_key_press(
        virtual_key,
        scan_code,
        u32::from(unicode_character),
        LEFT_CTRL_PRESSED,
    )
}

fn ascii_key_location(character: char) -> Option<(u32, u32)> {
    let character = character.to_ascii_uppercase();
    if character.is_ascii_uppercase() {
        let index = u32::from(character as u8 - b'A');
        let scan_code = match character {
            'A' => 0x1E,
            'B' => 0x30,
            'C' => 0x2E,
            'D' => 0x20,
            'E' => 0x12,
            'F' => 0x21,
            'G' => 0x22,
            'H' => 0x23,
            'I' => 0x17,
            'J' => 0x24,
            'K' => 0x25,
            'L' => 0x26,
            'M' => 0x32,
            'N' => 0x31,
            'O' => 0x18,
            'P' => 0x19,
            'Q' => 0x10,
            'R' => 0x13,
            'S' => 0x1F,
            'T' => 0x14,
            'U' => 0x16,
            'V' => 0x2F,
            'W' => 0x11,
            'X' => 0x2D,
            'Y' => 0x15,
            'Z' => 0x2C,
            _ => return None,
        };
        return Some((0x41 + index, scan_code));
    }
    if character.is_ascii_digit() {
        let scan_code = match character {
            '1' => 0x02,
            '2' => 0x03,
            '3' => 0x04,
            '4' => 0x05,
            '5' => 0x06,
            '6' => 0x07,
            '7' => 0x08,
            '8' => 0x09,
            '9' => 0x0A,
            '0' => 0x0B,
            _ => return None,
        };
        return Some((u32::from(character as u8), scan_code));
    }
    match character {
        ' ' => Some((0x20, 0x39)),
        _ => None,
    }
}

fn win32_terminal_input(event: &KeyEvent, modifiers: ModifiersState) -> Result<Option<Vec<u8>>> {
    if modifiers.control_key()
        && let Key::Character(character) = &event.logical_key
        && character.as_str().eq_ignore_ascii_case("v")
    {
        return terminal_input_key(&event.logical_key, event.text.as_deref(), modifiers);
    }

    let Some((virtual_key, scan_code, enhanced)) = win32_logical_key(&event.logical_key) else {
        return terminal_input_key(&event.logical_key, event.text.as_deref(), modifiers);
    };
    let control_state = win32_control_state(modifiers) | if enhanced { ENHANCED_KEY } else { 0 };
    let unicode_character = if modifiers.control_key() {
        match &event.logical_key {
            Key::Character(character) => control_character_byte(character).map(u32::from),
            Key::Named(NamedKey::Backspace) => Some(0x08),
            _ => None,
        }
    } else {
        event
            .text
            .as_deref()
            .and_then(single_character)
            .map(u32::from)
            .or_else(|| match &event.logical_key {
                Key::Character(character) => single_character(character).map(u32::from),
                _ => None,
            })
    }
    .unwrap_or(0);
    Ok(Some(win32_key_press(
        virtual_key,
        scan_code,
        unicode_character,
        control_state,
    )))
}

fn win32_control_state(modifiers: ModifiersState) -> u32 {
    let mut state = 0;
    if modifiers.shift_key() {
        state |= SHIFT_PRESSED;
    }
    if modifiers.control_key() {
        state |= LEFT_CTRL_PRESSED;
    }
    if modifiers.alt_key() {
        state |= LEFT_ALT_PRESSED;
    }
    state
}

fn win32_logical_key(key: &Key) -> Option<(u32, u32, bool)> {
    match key {
        Key::Character(character) => single_character(character)
            .and_then(ascii_key_location)
            .map(|(virtual_key, scan_code)| (virtual_key, scan_code, false)),
        Key::Named(NamedKey::Enter) => Some((0x0D, 0x1C, false)),
        Key::Named(NamedKey::Tab) => Some((0x09, 0x0F, false)),
        Key::Named(NamedKey::Backspace) => Some((0x08, 0x0E, false)),
        Key::Named(NamedKey::Escape) => Some((0x1B, 0x01, false)),
        Key::Named(NamedKey::ArrowUp) => Some((0x26, 0x48, true)),
        Key::Named(NamedKey::ArrowDown) => Some((0x28, 0x50, true)),
        Key::Named(NamedKey::ArrowRight) => Some((0x27, 0x4D, true)),
        Key::Named(NamedKey::ArrowLeft) => Some((0x25, 0x4B, true)),
        Key::Named(NamedKey::Home) => Some((0x24, 0x47, true)),
        Key::Named(NamedKey::End) => Some((0x23, 0x4F, true)),
        Key::Named(NamedKey::Delete) => Some((0x2E, 0x53, true)),
        Key::Named(NamedKey::PageUp) => Some((0x21, 0x49, true)),
        Key::Named(NamedKey::PageDown) => Some((0x22, 0x51, true)),
        Key::Named(NamedKey::Insert) => Some((0x2D, 0x52, true)),
        _ => None,
    }
}

fn single_character(text: &str) -> Option<char> {
    let mut characters = text.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "legacy and Win32 input encoders share one fallible adapter contract"
)]
fn terminal_input_key(
    key: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
) -> Result<Option<Vec<u8>>> {
    if let Some(sequence) = modified_navigation_sequence(key, modifiers) {
        return Ok(Some(sequence));
    }
    if modifiers.control_key() && matches!(key, Key::Named(NamedKey::Backspace)) {
        // PowerShell/PSReadLine and common Unix line disciplines use Ctrl+W
        // (ETB) for backward-kill-word. Microsoft Terminal emits the same
        // byte for Ctrl+Backspace in a compatible prompt.
        return Ok(Some(vec![0x17]));
    }
    if modifiers.control_key()
        && let Key::Character(character) = key
    {
        match character.as_str().to_ascii_lowercase().as_str() {
            "a" => return Ok(Some(vec![0x01])),
            "c" => return Ok(Some(vec![0x03])),
            "d" => return Ok(Some(vec![0x04])),
            "z" => return Ok(Some(vec![0x1a])),
            "l" => return Ok(Some(vec![0x0c])),
            // Ctrl+V is owned by the native adapter so every input mode uses
            // the same bounded multiline confirmation path.
            "v" => return Ok(None),
            _ => {
                if let Some(byte) = control_character_byte(character) {
                    return Ok(Some(vec![byte]));
                }
            }
        }
    }
    let bytes = match key {
        Key::Named(NamedKey::Enter) => b"\r".to_vec(),
        Key::Named(NamedKey::Tab) => b"\t".to_vec(),
        Key::Named(NamedKey::Backspace) => vec![0x7f],
        Key::Named(NamedKey::Escape) => vec![0x1b],
        Key::Named(NamedKey::ArrowUp) => b"\x1b[A".to_vec(),
        Key::Named(NamedKey::ArrowDown) => b"\x1b[B".to_vec(),
        Key::Named(NamedKey::ArrowRight) => b"\x1b[C".to_vec(),
        Key::Named(NamedKey::ArrowLeft) => b"\x1b[D".to_vec(),
        Key::Named(NamedKey::Home) => b"\x1b[H".to_vec(),
        Key::Named(NamedKey::End) => b"\x1b[F".to_vec(),
        Key::Named(NamedKey::Delete) => b"\x1b[3~".to_vec(),
        Key::Named(NamedKey::PageUp) => b"\x1b[5~".to_vec(),
        Key::Named(NamedKey::PageDown) => b"\x1b[6~".to_vec(),
        Key::Character(character) => character.as_bytes().to_vec(),
        _ => text.map(str::as_bytes).unwrap_or_default().to_vec(),
    };
    if bytes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(bytes))
    }
}

fn control_character_byte(character: &str) -> Option<u8> {
    let mut characters = character.chars();
    let character = characters.next()?.to_ascii_lowercase();
    if characters.next().is_some() {
        return None;
    }
    if character.is_ascii_lowercase() {
        return Some(character as u8 - b'a' + 1);
    }
    match character {
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn modified_navigation_sequence(key: &Key, modifiers: ModifiersState) -> Option<Vec<u8>> {
    let modifier = 1
        + i32::from(modifiers.shift_key())
        + i32::from(modifiers.alt_key()) * 2
        + i32::from(modifiers.control_key()) * 4
        + i32::from(modifiers.super_key()) * 8;
    let has_modifier = modifiers.shift_key()
        || modifiers.alt_key()
        || modifiers.control_key()
        || modifiers.super_key();
    if !has_modifier {
        return None;
    }

    if matches!(key, Key::Named(NamedKey::Tab))
        && modifiers.shift_key()
        && !modifiers.alt_key()
        && !modifiers.control_key()
        && !modifiers.super_key()
    {
        return Some(b"\x1b[Z".to_vec());
    }

    let (final_byte, tilde_code) = match key {
        Key::Named(NamedKey::ArrowUp) => (Some('A'), None),
        Key::Named(NamedKey::ArrowDown) => (Some('B'), None),
        Key::Named(NamedKey::ArrowRight) => (Some('C'), None),
        Key::Named(NamedKey::ArrowLeft) => (Some('D'), None),
        Key::Named(NamedKey::Home) => (Some('H'), None),
        Key::Named(NamedKey::End) => (Some('F'), None),
        Key::Named(NamedKey::Insert) => (None, Some(2)),
        Key::Named(NamedKey::Delete) => (None, Some(3)),
        Key::Named(NamedKey::PageUp) => (None, Some(5)),
        Key::Named(NamedKey::PageDown) => (None, Some(6)),
        _ => return None,
    };

    if let Some(final_byte) = final_byte {
        Some(format!("\x1b[1;{modifier}{final_byte}").into_bytes())
    } else {
        Some(
            format!(
                "\x1b[{};{modifier}~",
                tilde_code.expect("modified tilde key has a sequence number")
            )
            .into_bytes(),
        )
    }
}

fn control_key_bytes(key: &ControlKey) -> Result<Vec<u8>> {
    let bytes = match key {
        ControlKey::Character { text } | ControlKey::Paste { text } => text.as_bytes().to_vec(),
        ControlKey::Enter => b"\r".to_vec(),
        ControlKey::Tab => b"\t".to_vec(),
        ControlKey::Backspace => vec![0x7f],
        ControlKey::Escape => vec![0x1b],
        ControlKey::ArrowUp => b"\x1b[A".to_vec(),
        ControlKey::ArrowDown => b"\x1b[B".to_vec(),
        ControlKey::ArrowRight => b"\x1b[C".to_vec(),
        ControlKey::ArrowLeft => b"\x1b[D".to_vec(),
        ControlKey::Home => b"\x1b[H".to_vec(),
        ControlKey::End => b"\x1b[F".to_vec(),
        ControlKey::Delete => b"\x1b[3~".to_vec(),
        ControlKey::PageUp => b"\x1b[5~".to_vec(),
        ControlKey::PageDown => b"\x1b[6~".to_vec(),
        ControlKey::CtrlA => vec![0x01],
        ControlKey::CtrlArrowUp => b"\x1b[1;5A".to_vec(),
        ControlKey::CtrlArrowDown => b"\x1b[1;5B".to_vec(),
        ControlKey::CtrlArrowRight => b"\x1b[1;5C".to_vec(),
        ControlKey::CtrlArrowLeft => b"\x1b[1;5D".to_vec(),
        ControlKey::CtrlHome => b"\x1b[1;5H".to_vec(),
        ControlKey::CtrlEnd => b"\x1b[1;5F".to_vec(),
        ControlKey::CtrlDelete => b"\x1b[3;5~".to_vec(),
        ControlKey::CtrlPageUp => b"\x1b[5;5~".to_vec(),
        ControlKey::CtrlPageDown => b"\x1b[6;5~".to_vec(),
        ControlKey::CtrlCharacter { character } => {
            let Some(byte) = control_character_byte(&character.to_string()) else {
                eyre::bail!("unsupported control character: {character:?}")
            };
            vec![byte]
        }
        ControlKey::CtrlC => vec![0x03],
        ControlKey::CtrlD => vec![0x04],
        ControlKey::CtrlZ => vec![0x1a],
        ControlKey::CtrlL => vec![0x0c],
        ControlKey::KeyDown { .. } | ControlKey::KeyUp { .. } | ControlKey::Delay { .. } => {
            eyre::bail!("scripted key actions must be handled by the sequence writer")
        }
    };
    if bytes.len() > teamy_terminal_control::MAX_INPUT_BYTES {
        eyre::bail!("control key text exceeds the input limit")
    }
    Ok(bytes)
}

fn paste_dialog_snapshot(
    columns: u16,
    rows: u16,
    confirmation: &PasteConfirmation,
    choice: PasteDialogChoice,
) -> TerminalSnapshot {
    let mut snapshot = TerminalSnapshot {
        columns,
        rows,
        cursor: CursorPosition::default(),
        cursor_visible: false,
        cursor_shape: CursorShape::SteadyBlock,
        cells: vec![Cell::default(); usize::from(columns) * usize::from(rows)],
        scrollback_rows: 0,
        selection: None,
        prompt: PromptMetadata::default(),
    };
    if columns == 0 || rows == 0 {
        return snapshot;
    }

    let layout = PasteDialogLayout::for_grid(columns, rows);
    let content_end = layout.button_row;
    let mut row = 0;
    row = write_dialog_text(
        &mut snapshot,
        row,
        content_end,
        MULTILINE_PASTE_WARNING_TITLE,
        true,
        false,
    )
    .saturating_add(1);
    row = write_dialog_text(
        &mut snapshot,
        row,
        content_end,
        MULTILINE_PASTE_WARNING,
        false,
        false,
    )
    .saturating_add(1);
    row = write_dialog_text(
        &mut snapshot,
        row,
        content_end,
        MULTILINE_PASTE_PREVIEW_LABEL,
        true,
        false,
    );
    let preview = display_safe_preview(confirmation.preview());
    let _ = write_dialog_text(&mut snapshot, row, content_end, &preview, false, false);

    let paste_button = format!("[ {MULTILINE_PASTE_APPROVE_LABEL} ]");
    write_dialog_line(
        &mut snapshot,
        layout.paste_start,
        layout.button_row,
        &paste_button,
        choice == PasteDialogChoice::PasteAnyway,
    );
    let cancel_button = format!("[ {MULTILINE_PASTE_CANCEL_LABEL} ]");
    write_dialog_line(
        &mut snapshot,
        layout.cancel_start,
        layout.button_row,
        &cancel_button,
        choice == PasteDialogChoice::Cancel,
    );
    snapshot
}

fn display_safe_preview(preview: &str) -> String {
    let mut display = String::with_capacity(preview.len());
    let mut characters = preview.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    let _ = characters.next();
                }
                display.push('\n');
            }
            '\n' => display.push('\n'),
            '\t' => display.push_str("    "),
            character if character.is_control() => display.push('\u{fffd}'),
            character => display.push(character),
        }
    }
    display
}

fn write_dialog_text(
    snapshot: &mut TerminalSnapshot,
    start_row: u16,
    end_row: u16,
    text: &str,
    bold: bool,
    inverse: bool,
) -> u16 {
    if snapshot.columns == 0 || start_row >= end_row {
        return start_row;
    }
    let mut column = 0;
    let mut row = start_row;
    for character in text.chars() {
        if row >= end_row {
            break;
        }
        if character == '\n' {
            column = 0;
            row = row.saturating_add(1);
            continue;
        }
        if column >= snapshot.columns {
            column = 0;
            row = row.saturating_add(1);
            if row >= end_row {
                break;
            }
        }
        let index = usize::from(row) * usize::from(snapshot.columns) + usize::from(column);
        if let Some(cell) = snapshot.cells.get_mut(index) {
            cell.character = character;
            cell.bold = bold;
            cell.inverse = inverse;
        }
        column = column.saturating_add(1);
    }
    row.saturating_add(1)
}

fn write_dialog_line(
    snapshot: &mut TerminalSnapshot,
    start_column: u16,
    row: u16,
    text: &str,
    inverse: bool,
) {
    if row >= snapshot.rows {
        return;
    }
    for (offset, character) in text.chars().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let column = start_column.saturating_add(offset);
        if column >= snapshot.columns {
            break;
        }
        let index = usize::from(row) * usize::from(snapshot.columns) + usize::from(column);
        if let Some(cell) = snapshot.cells.get_mut(index) {
            cell.character = character;
            cell.bold = true;
            cell.inverse = inverse;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn character(value: &str) -> Key {
        Key::Character(value.into())
    }

    #[test]
    fn control_keys_use_terminal_control_bytes() {
        assert_eq!(
            terminal_input_key(&character("a"), None, ModifiersState::CONTROL)
                .expect("Ctrl+A mapping should not fail"),
            Some(vec![0x01])
        );
        assert_eq!(
            terminal_input_key(&character("c"), None, ModifiersState::CONTROL)
                .expect("Ctrl+C mapping should not fail"),
            Some(vec![0x03])
        );
        assert_eq!(
            terminal_input_key(&character("d"), None, ModifiersState::CONTROL)
                .expect("Ctrl+D mapping should not fail"),
            Some(vec![0x04])
        );
        for (value, expected) in [("b", 0x02), ("e", 0x05), ("h", 0x08), ("q", 0x11)] {
            assert_eq!(
                terminal_input_key(&character(value), None, ModifiersState::CONTROL)
                    .expect("ordinary Ctrl+letter mapping should not fail"),
                Some(vec![expected])
            );
        }
        assert_eq!(
            terminal_input_key(
                &character("B"),
                None,
                ModifiersState::CONTROL.union(ModifiersState::SHIFT)
            )
            .expect("Ctrl+Shift+letter mapping should not fail"),
            Some(vec![0x02])
        );
        assert_eq!(
            terminal_input_key(&character("?"), None, ModifiersState::CONTROL)
                .expect("Ctrl+question-mark mapping should not fail"),
            Some(vec![0x7f])
        );
        assert_eq!(
            terminal_input_key(
                &Key::Named(NamedKey::Backspace),
                None,
                ModifiersState::CONTROL
            )
            .expect("Ctrl+Backspace mapping should not fail"),
            Some(vec![0x17])
        );
        assert_eq!(
            terminal_input_key(
                &Key::Named(NamedKey::Backspace),
                None,
                ModifiersState::CONTROL.union(ModifiersState::SHIFT)
            )
            .expect("Ctrl+Shift+Backspace mapping should not fail"),
            Some(vec![0x17])
        );
        assert_eq!(
            terminal_input_key(&character("v"), None, ModifiersState::SHIFT)
                .expect("ordinary text mapping should not fail"),
            Some(b"v".to_vec())
        );
        assert_eq!(
            terminal_input_key(&character("v"), None, ModifiersState::CONTROL)
                .expect("Ctrl+V ownership should not fail"),
            None,
            "Ctrl+V must be owned by the guarded native clipboard adapter"
        );
    }

    #[test]
    fn native_warning_renders_exact_copy_and_canonical_preview() {
        assert_eq!(MULTILINE_PASTE_WARNING_TITLE, "Warning");
        assert_eq!(
            MULTILINE_PASTE_WARNING,
            "You are about to paste text that contains multiple lines. If you paste this text into your shell, it may result in the unexpected execution of commands. Do you wish to continue?"
        );
        assert_eq!(
            MULTILINE_PASTE_PREVIEW_LABEL,
            "Clipboard contents (preview):"
        );
        assert_eq!(MULTILINE_PASTE_APPROVE_LABEL, "Paste anyway");
        assert_eq!(MULTILINE_PASTE_CANCEL_LABEL, "Cancel");

        let mut guard = PasteGuard::default();
        let PasteRequest::ConfirmationRequired(confirmation) = guard
            .request("99\n100".to_owned())
            .expect("canonical multiline clipboard should be accepted")
        else {
            panic!("canonical multiline clipboard should require confirmation");
        };
        let snapshot = paste_dialog_snapshot(120, 36, &confirmation, PasteDialogChoice::Cancel);
        let text = snapshot.visible_text();
        assert!(text.contains(MULTILINE_PASTE_WARNING_TITLE));
        assert!(text.contains("You are about to paste text that contains multiple lines."));
        assert!(text.contains("Do you wish to continue?"));
        assert!(text.contains(MULTILINE_PASTE_PREVIEW_LABEL));
        assert!(text.contains("99\n100"));
        assert!(text.contains(MULTILINE_PASTE_APPROVE_LABEL));
        assert!(text.contains(MULTILINE_PASTE_CANCEL_LABEL));
    }

    #[test]
    fn native_dialog_defaults_to_cancel_and_requires_explicit_approval_choice() {
        let mut guard = PasteGuard::default();
        let PasteRequest::ConfirmationRequired(confirmation) = guard
            .request("one\ntwo".to_owned())
            .expect("multiline clipboard should be accepted")
        else {
            panic!("multiline clipboard should require confirmation");
        };
        let mut dialog = PasteDialog::new(confirmation);
        assert_eq!(
            dialog.handle_key(&Key::Named(NamedKey::Enter)),
            Some(PasteDialogChoice::Cancel)
        );
        assert_eq!(dialog.handle_key(&Key::Named(NamedKey::Tab)), None);
        assert_eq!(dialog.choice, PasteDialogChoice::PasteAnyway);
        assert_eq!(
            dialog.handle_key(&Key::Named(NamedKey::Enter)),
            Some(PasteDialogChoice::PasteAnyway)
        );
        assert_eq!(
            dialog.handle_key(&Key::Named(NamedKey::Escape)),
            Some(PasteDialogChoice::Cancel)
        );
    }

    #[test]
    fn native_dialog_buttons_have_bounded_hit_regions() {
        let layout = PasteDialogLayout::for_grid(120, 36);
        assert_eq!(
            layout.choice_at(CursorPosition {
                column: layout.paste_start,
                row: layout.button_row
            }),
            Some(PasteDialogChoice::PasteAnyway)
        );
        assert_eq!(
            layout.choice_at(CursorPosition {
                column: layout.cancel_start,
                row: layout.button_row
            }),
            Some(PasteDialogChoice::Cancel)
        );
        assert_eq!(
            layout.choice_at(CursorPosition {
                column: layout.cancel_end,
                row: layout.button_row
            }),
            None
        );
        assert_eq!(
            layout.choice_at(CursorPosition {
                column: layout.paste_start,
                row: layout.button_row - 1
            }),
            None
        );
    }

    #[test]
    fn right_click_branch_is_selection_aware() {
        assert_eq!(
            secondary_click_action(true),
            SecondaryClickAction::CopySelection
        );
        assert_eq!(
            secondary_click_action(false),
            SecondaryClickAction::GuardedPaste
        );
    }

    #[test]
    fn clipboard_preview_cannot_inject_terminal_controls() {
        assert_eq!(
            display_safe_preview("99\r\n100\t\u{1b}[31m"),
            "99\n100    �[31m"
        );
    }

    #[test]
    fn embedded_window_icon_decodes_for_winit() {
        load_window_icon().expect("the checked-in window icon should decode");
    }

    #[test]
    fn ctrl_wheel_font_zoom_is_bounded_and_directional() {
        assert!((adjusted_font_pixel_size(DEFAULT_PIXEL_SIZE, 1.0) - 17.0).abs() < f32::EPSILON);
        assert!((adjusted_font_pixel_size(DEFAULT_PIXEL_SIZE, -1.0) - 15.0).abs() < f32::EPSILON);
        assert!(
            (adjusted_font_pixel_size(MIN_FONT_PIXEL_SIZE, -1.0) - MIN_FONT_PIXEL_SIZE).abs()
                < f32::EPSILON
        );
        assert!(
            (adjusted_font_pixel_size(MAX_FONT_PIXEL_SIZE, 1.0) - MAX_FONT_PIXEL_SIZE).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn mouse_wheel_zoom_uses_vertical_scroll_direction() {
        assert_eq!(
            wheel_zoom_direction(&MouseScrollDelta::LineDelta(0.0, 1.0)),
            Some(1.0)
        );
        assert_eq!(
            wheel_zoom_direction(&MouseScrollDelta::PixelDelta(
                winit::dpi::PhysicalPosition::new(0.0, -8.0)
            )),
            Some(-1.0)
        );
        assert_eq!(
            wheel_zoom_direction(&MouseScrollDelta::LineDelta(0.0, 0.0)),
            None
        );
        assert_eq!(
            mouse_wheel_steps(&MouseScrollDelta::LineDelta(-2.0, 3.0)),
            (-1, 1)
        );
        assert_eq!(
            mouse_wheel_steps(&MouseScrollDelta::PixelDelta(
                winit::dpi::PhysicalPosition::new(0.0, -12.0)
            )),
            (0, -1)
        );
    }

    #[test]
    fn mouse_positions_map_to_bounded_terminal_cells() {
        assert_eq!(
            terminal_cell_at_position(winit::dpi::PhysicalPosition::new(31.9, 47.9), 16, 24, 4, 3,),
            Some(teamy_terminal_core::CursorPosition { column: 1, row: 1 })
        );
        assert_eq!(
            terminal_cell_at_position(winit::dpi::PhysicalPosition::new(64.0, 0.0), 16, 24, 4, 3,),
            None
        );
        assert_eq!(
            terminal_cell_at_position(winit::dpi::PhysicalPosition::new(-1.0, 0.0), 16, 24, 4, 3,),
            None
        );
    }

    #[test]
    fn navigation_keys_use_pty_sequences() {
        assert_eq!(
            terminal_input_key(
                &Key::Named(NamedKey::ArrowLeft),
                None,
                ModifiersState::CONTROL
            )
            .expect("Ctrl+Left mapping should not fail"),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            terminal_input_key(
                &Key::Named(NamedKey::ArrowRight),
                None,
                ModifiersState::CONTROL
            )
            .expect("Ctrl+Right mapping should not fail"),
            Some(b"\x1b[1;5C".to_vec())
        );
        let ctrl_shift = ModifiersState::CONTROL.union(ModifiersState::SHIFT);
        for (key, expected) in [
            (NamedKey::ArrowLeft, b"\x1b[1;6D".as_slice()),
            (NamedKey::ArrowRight, b"\x1b[1;6C".as_slice()),
            (NamedKey::Home, b"\x1b[1;6H".as_slice()),
            (NamedKey::End, b"\x1b[1;6F".as_slice()),
            (NamedKey::Delete, b"\x1b[3;6~".as_slice()),
            (NamedKey::PageUp, b"\x1b[5;6~".as_slice()),
            (NamedKey::PageDown, b"\x1b[6;6~".as_slice()),
        ] {
            assert_eq!(
                terminal_input_key(&Key::Named(key), None, ctrl_shift)
                    .expect("Ctrl+Shift navigation mapping should not fail"),
                Some(expected.to_vec())
            );
        }
        assert_eq!(
            terminal_input_key(&Key::Named(NamedKey::Tab), None, ModifiersState::SHIFT)
                .expect("Shift+Tab mapping should not fail"),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            control_key_bytes(&ControlKey::CtrlHome).expect("Ctrl+Home should encode"),
            b"\x1b[1;5H".to_vec()
        );
        assert_eq!(
            control_key_bytes(&ControlKey::CtrlDelete).expect("Ctrl+Delete should encode"),
            b"\x1b[3;5~".to_vec()
        );
    }

    #[test]
    fn win32_logical_keys_use_console_input_sequences() {
        assert_eq!(
            win32_control_key_bytes(&ControlKey::CtrlC).expect("Win32 Ctrl+C should encode"),
            b"\x1b[67;46;3;1;8;1_\x1b[67;46;0;0;8;1_".to_vec()
        );
        assert_eq!(
            win32_control_key_bytes(&ControlKey::CtrlArrowRight)
                .expect("Win32 Ctrl+Right should encode"),
            b"\x1b[39;77;0;1;8;1_\x1b[39;77;0;0;8;1_".to_vec()
        );
        assert_eq!(
            win32_text_bytes("Ab"),
            b"\x1b[65;30;65;1;16;1_\x1b[65;30;0;0;16;1_\x1b[66;48;98;1;0;1_\x1b[66;48;0;0;0;1_"
                .to_vec()
        );
        assert_eq!(
            win32_text_bytes("A\r\n"),
            b"\x1b[65;30;65;1;16;1_\x1b[65;30;0;0;16;1_\x1b[13;28;13;1;0;1_\x1b[13;28;0;0;0;1_\x1b[13;28;13;1;0;1_\x1b[13;28;0;0;0;1_"
                .to_vec()
        );
        assert_eq!(
            win32_control_key_bytes(&ControlKey::Character {
                text: String::from("Ab")
            })
            .expect("Win32 text should encode as key events"),
            win32_text_bytes("Ab")
        );
        assert_eq!(
            win32_control_key_bytes(&ControlKey::Paste {
                text: String::from("Ab")
            })
            .expect("Win32 paste should remain raw text"),
            b"Ab".to_vec()
        );
    }

    #[test]
    fn win32_physical_keys_track_pressed_modifiers() {
        let numeric_key = ControlPhysicalKey::Character {
            text: String::from("1"),
        };
        assert_eq!(
            win32_physical_key_event_bytes(&numeric_key, true, &[])
                .expect("numeric key down should encode"),
            b"\x1b[49;2;49;1;0;1_".to_vec()
        );
        assert_eq!(
            win32_physical_key_event_bytes(&numeric_key, false, &[])
                .expect("numeric key up should encode"),
            b"\x1b[49;2;0;0;0;1_".to_vec()
        );
        let ctrl = ControlPhysicalKey::Ctrl;
        let c = ControlPhysicalKey::Character {
            text: String::from("c"),
        };
        assert_eq!(
            win32_physical_key_event_bytes(&ctrl, true, std::slice::from_ref(&ctrl))
                .expect("Ctrl key down should only update state"),
            Vec::<u8>::new()
        );
        assert_eq!(
            win32_physical_key_event_bytes(&c, true, std::slice::from_ref(&ctrl))
                .expect("Ctrl+C key down should encode"),
            b"\x1b[67;46;3;1;8;1_".to_vec()
        );
        assert_eq!(
            win32_physical_key_event_bytes(&ctrl, false, std::slice::from_ref(&ctrl))
                .expect("Ctrl key up should only update state"),
            Vec::<u8>::new()
        );
        assert_eq!(
            win32_physical_key_event_bytes(&c, false, &[ctrl.clone(), c.clone()])
                .expect("Ctrl+C key up should encode"),
            b"\x1b[67;46;0;0;8;1_".to_vec()
        );
        let l = ControlPhysicalKey::Character {
            text: String::from("l"),
        };
        assert_eq!(
            win32_physical_key_event_bytes(&l, true, std::slice::from_ref(&ctrl))
                .expect("Ctrl+L key down should use the generic encoder"),
            b"\x1b[76;38;12;1;8;1_".to_vec()
        );
        assert_eq!(
            win32_physical_key_event_bytes(&l, false, &[ctrl.clone(), l.clone()])
                .expect("Ctrl+L key up should use the generic encoder"),
            b"\x1b[76;38;0;0;8;1_".to_vec()
        );
        assert!(
            win32_physical_key_event_bytes(&ctrl, true, std::slice::from_ref(&ctrl))
                .expect("Ctrl modifier down should only update state")
                .is_empty()
        );
        assert!(
            win32_physical_key_event_bytes(&ctrl, false, std::slice::from_ref(&ctrl))
                .expect("Ctrl modifier up should only update state")
                .is_empty()
        );
    }

    #[test]
    fn plain_navigation_and_text_keys_use_pty_sequences() {
        assert_eq!(
            terminal_input_key(
                &Key::Named(NamedKey::ArrowLeft),
                None,
                ModifiersState::default()
            )
            .expect("arrow mapping should not fail"),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            terminal_input_key(
                &Key::Named(NamedKey::Enter),
                None,
                ModifiersState::default()
            )
            .expect("enter mapping should not fail"),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            terminal_input_key(
                &Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
                Some("Ω"),
                ModifiersState::default()
            )
            .expect("text fallback should not fail"),
            Some("Ω".as_bytes().to_vec())
        );
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_expand_local_changes_for_wide_glyph_safety() {
        let mut terminal = teamy_terminal_core::TerminalSession::new(
            teamy_terminal_core::TerminalLimits::default(),
            8,
            2,
        );
        terminal.process_bytes("\x1b[?25lab界efgh".as_bytes());
        let previous = terminal.snapshot();
        terminal.process_bytes("\x1b[1;3H語".as_bytes());
        let current = terminal.snapshot();

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(!full_refresh);
        assert_eq!(regions, vec![CellRect::new(1, 0, 3, 1)]);
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_force_full_refresh_after_resize() {
        let previous = teamy_terminal_core::TerminalSnapshot {
            columns: 2,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition::default(),
            cursor_visible: false,
            cursor_shape: teamy_terminal_core::CursorShape::default(),
            cells: vec![teamy_terminal_core::Cell::default(); 2],
            scrollback_rows: 0,
            selection: None,
            prompt: teamy_terminal_core::PromptMetadata::default(),
        };
        let current = teamy_terminal_core::TerminalSnapshot {
            columns: 3,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition::default(),
            cursor_visible: false,
            cursor_shape: teamy_terminal_core::CursorShape::default(),
            cells: vec![teamy_terminal_core::Cell::default(); 3],
            scrollback_rows: 0,
            selection: None,
            prompt: teamy_terminal_core::PromptMetadata::default(),
        };

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(full_refresh);
        assert_eq!(regions, vec![CellRect::full(3, 1)]);
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_force_full_refresh_after_selection_changes() {
        let mut terminal = teamy_terminal_core::TerminalSession::new(
            teamy_terminal_core::TerminalLimits::default(),
            4,
            2,
        );
        terminal.process_bytes(b"abcd\r\nefgh");
        let previous = terminal.snapshot();
        terminal.begin_selection(teamy_terminal_core::CursorPosition::default());
        terminal.update_selection(teamy_terminal_core::CursorPosition { column: 2, row: 1 });
        let current = terminal.snapshot();

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(full_refresh);
        assert_eq!(regions, vec![CellRect::full(4, 2)]);
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_refresh_cursor_departure_and_arrival() {
        let cells = vec![teamy_terminal_core::Cell::default(); 8];
        let previous = teamy_terminal_core::TerminalSnapshot {
            columns: 8,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition { column: 0, row: 0 },
            cursor_visible: true,
            cursor_shape: teamy_terminal_core::CursorShape::default(),
            cells: cells.clone(),
            scrollback_rows: 0,
            selection: None,
            prompt: teamy_terminal_core::PromptMetadata::default(),
        };
        let current = teamy_terminal_core::TerminalSnapshot {
            columns: 8,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition { column: 7, row: 0 },
            cursor_visible: true,
            cursor_shape: teamy_terminal_core::CursorShape::default(),
            cells,
            scrollback_rows: 0,
            selection: None,
            prompt: teamy_terminal_core::PromptMetadata::default(),
        };

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(!full_refresh);
        assert_eq!(
            regions,
            vec![CellRect::new(0, 0, 2, 1), CellRect::new(6, 0, 2, 1)]
        );
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_force_full_refresh_for_scrolling_content() {
        let mut terminal = teamy_terminal_core::TerminalSession::new(
            teamy_terminal_core::TerminalLimits::default(),
            4,
            3,
        );
        terminal.process_bytes(b"1111\r\n2222\r\n3333");
        let previous = terminal.snapshot();
        terminal.process_bytes(b"\r\n4444");
        let current = terminal.snapshot();

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(full_refresh);
        assert_eq!(regions, vec![CellRect::full(4, 3)]);
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_force_full_refresh_without_a_cached_snapshot() {
        let current = teamy_terminal_core::TerminalSnapshot {
            columns: 2,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition::default(),
            cursor_visible: true,
            cursor_shape: teamy_terminal_core::CursorShape::default(),
            cells: vec![teamy_terminal_core::Cell::default(); 2],
            scrollback_rows: 0,
            selection: None,
            prompt: teamy_terminal_core::PromptMetadata::default(),
        };

        let (regions, full_refresh) = damage_regions(None, &current);

        assert!(full_refresh);
        assert_eq!(regions, vec![CellRect::full(2, 1)]);
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn dirty_rendering_repaints_cursor_changes_to_the_full_frame_result() {
        let mut font = TerminalFont::embedded().expect("embedded font should parse");
        let mut terminal = teamy_terminal_core::TerminalSession::new(
            teamy_terminal_core::TerminalLimits::default(),
            8,
            1,
        );
        terminal.process_bytes(b"abcdefgh");
        let previous = terminal.snapshot();
        let width = font.cell_width() * 8;
        let height = font.cell_height();
        let mut cached = font.render(&previous, width, height);

        terminal.process_bytes(b"\x1b[1;8H");
        let current = terminal.snapshot();
        let (regions, full_refresh) = damage_regions(Some(&previous), &current);
        assert!(!full_refresh);
        let expected = font.render(&current, width, height);
        font.render_cells(&current, &mut cached, &regions);

        assert_eq!(cached, expected);
    }
}
