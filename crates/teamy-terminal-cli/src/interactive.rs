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
use teamy_cancellation::CancellationToken;
use teamy_terminal_control::ControlAction;
use teamy_terminal_control::ControlCall;
use teamy_terminal_control::ControlKey;
use teamy_terminal_control::ControlServer;
use teamy_terminal_control::InstanceInfo;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;
#[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
use teamy_terminal_core::TerminalSnapshot;
#[cfg(feature = "dirty_rendering")]
use teamy_terminal_font::CellRect;
#[cfg(feature = "dirty_rendering")]
use teamy_terminal_font::RasterizedFrame;
use teamy_terminal_font::TerminalFont;
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
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::event_loop::ControlFlow;
use winit::event_loop::EventLoop;
use winit::keyboard::Key;
use winit::keyboard::ModifiersState;
use winit::keyboard::NamedKey;
use winit::window::Icon;
use winit::window::Window;
use winit::window::WindowId;

const INITIAL_WINDOW_WIDTH: u32 = 1280;
const INITIAL_WINDOW_HEIGHT: u32 = 720;
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../resources/main.png");
const MAX_PTY_DRAIN_EVENTS_PER_TURN: usize = 32;
const MAX_PTY_DRAIN_BYTES_PER_TURN: usize = 512 * 1024;
static EVENT_LOOP_LOGGED: Once = Once::new();
static PTY_DRAIN_LOGGED: Once = Once::new();
static CONTROL_DRAIN_LOGGED: Once = Once::new();

/// Run the standalone interactive Vulkan terminal.
pub fn run(cancellation_token: &CancellationToken) -> Result<()> {
    let event_loop = EventLoop::new().wrap_err("failed to create the terminal event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut application = InteractiveApplication::new(cancellation_token.clone())?;
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

struct InteractiveApplication {
    cancellation_token: CancellationToken,
    session: Option<PtySession>,
    terminal: TerminalSession,
    font: TerminalFont,
    window: Option<Window>,
    renderer: Option<VulkanRenderer>,
    modifiers: ModifiersState,
    dirty: bool,
    resize_pending: bool,
    #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
    last_rendered_snapshot: Option<TerminalSnapshot>,
    #[cfg(feature = "dirty_rendering")]
    rendered_frame: Option<RasterizedFrame>,
    failure: Option<String>,
    control_server: Option<ControlServer>,
    control_calls: Option<Receiver<ControlCall>>,
}

impl InteractiveApplication {
    fn new(cancellation_token: CancellationToken) -> Result<Self> {
        let config = PtyConfig::interactive();
        let session = PtySession::spawn(&config)
            .wrap_err("failed to launch default-profile pwsh.exe through portable-pty")?;
        Ok(Self {
            cancellation_token,
            session: Some(session),
            terminal: TerminalSession::new(TerminalLimits::default(), 120, 36),
            font: TerminalFont::embedded().wrap_err("failed to load embedded Caskaydia font")?,
            window: None,
            renderer: None,
            modifiers: ModifiersState::default(),
            dirty: true,
            resize_pending: false,
            #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
            last_rendered_snapshot: None,
            #[cfg(feature = "dirty_rendering")]
            rendered_frame: None,
            failure: None,
            control_server: None,
            control_calls: None,
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
        let attributes = Window::default_attributes()
            .with_title("Teamy Terminal — Vulkan")
            .with_inner_size(PhysicalSize::new(
                INITIAL_WINDOW_WIDTH,
                INITIAL_WINDOW_HEIGHT,
            ))
            .with_window_icon(Some(icon));
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
        {
            self.rendered_frame = None;
        }
        Ok(())
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
                    self.terminal.process_bytes(&bytes);
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
            ControlAction::Ping => Ok(()),
            ControlAction::SendText { text } => self
                .session
                .as_ref()
                .ok_or_else(|| eyre::eyre!("PowerShell session is no longer running"))
                .and_then(|session| session.write_input(text.as_bytes())),
            ControlAction::SendKeys { keys } => self.write_control_keys(&keys),
            ControlAction::Resize { width, height } => {
                if width == 0 || height == 0 {
                    Err(eyre::eyre!("control resize dimensions must be non-zero"))
                } else if let Some(window) = self.window.as_ref() {
                    let _ = window.request_inner_size(PhysicalSize::new(width, height));
                    Ok(())
                } else {
                    Err(eyre::eyre!("terminal window is not initialized"))
                }
            }
            ControlAction::Screenshot { path } => self.write_screenshot(Path::new(&path)),
            ControlAction::Close => {
                if let Some(session) = self.session.as_ref() {
                    session
                        .kill()
                        .map_err(|error| eyre::eyre!("failed to close PowerShell: {error}"))
                        .map(|()| ())
                } else {
                    Ok(())
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
        for key in keys {
            session.write_input(&control_key_bytes(key)?)?;
        }
        Ok(())
    }

    fn write_screenshot(&mut self, path: &Path) -> Result<()> {
        let extent = self
            .renderer
            .as_ref()
            .ok_or_else(|| eyre::eyre!("terminal renderer is not initialized"))?
            .extent();
        let snapshot = self.terminal.snapshot();
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
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let extent = renderer.extent();
        let snapshot = self.terminal.snapshot();
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
                {
                    self.rendered_frame = Some(frame);
                }
                #[cfg(any(feature = "extended_observability", feature = "dirty_rendering"))]
                {
                    self.last_rendered_snapshot = Some(snapshot);
                }
                #[cfg(feature = "extended_observability")]
                tracing::info!(message = "finished frame", tracy.frame_mark = true);
            }
            Err(error) => self.fail(error, event_loop),
        }
    }

    fn input(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        if event.state != ElementState::Pressed {
            return;
        }
        let bytes = match terminal_input(event, self.modifiers) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(error) => {
                self.fail(error, event_loop);
                return;
            }
        };
        if let Some(session) = self.session.as_ref()
            && let Err(error) = session.write_input(&bytes)
        {
            self.fail(error, event_loop);
        }
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

#[cfg(feature = "dirty_rendering")]
const MAX_DIRTY_RECTS: usize = 64;

#[cfg(feature = "dirty_rendering")]
fn damage_regions(
    previous: Option<&TerminalSnapshot>,
    current: &TerminalSnapshot,
) -> (Vec<CellRect>, bool) {
    let Some(previous) = previous else {
        return (vec![CellRect::full(current.columns, current.rows)], true);
    };
    if previous.columns != current.columns || previous.rows != current.rows {
        return (vec![CellRect::full(current.columns, current.rows)], true);
    }

    let columns = usize::from(current.columns);
    let rows = usize::from(current.rows);
    let mut changed = vec![false; current.cells.len()];
    for (index, (previous, current)) in previous.cells.iter().zip(&current.cells).enumerate() {
        changed[index] = previous != current;
    }
    if previous.cursor_visible || current.cursor_visible {
        mark_cursor_cell(&mut changed, current.columns, previous.cursor);
        mark_cursor_cell(&mut changed, current.columns, current.cursor);
    }
    let changed_count = changed.iter().filter(|changed| **changed).count();
    if changed_count == 0 {
        return (Vec::new(), false);
    }
    if changed_count.saturating_mul(3) >= changed.len() {
        return (vec![CellRect::full(current.columns, current.rows)], true);
    }

    let mut regions = Vec::new();
    for row in 0..rows {
        let row_start = row * columns;
        let mut column = 0;
        while column < columns {
            while column < columns && !changed[row_start + column] {
                column += 1;
            }
            if column == columns {
                break;
            }
            let start = column;
            while column < columns && changed[row_start + column] {
                column += 1;
            }
            let end = column;
            let expanded_start = start.saturating_sub(1);
            let expanded_end = (end + 1).min(columns);
            regions.push(CellRect::new(
                u16::try_from(expanded_start).expect("terminal column fits in u16"),
                u16::try_from(row).expect("terminal row fits in u16"),
                u16::try_from(expanded_end - expanded_start)
                    .expect("dirty rectangle width fits in u16"),
                1,
            ));
            if regions.len() > MAX_DIRTY_RECTS {
                return (vec![CellRect::full(current.columns, current.rows)], true);
            }
        }
    }
    (regions, false)
}

#[cfg(feature = "dirty_rendering")]
fn mark_cursor_cell(
    changed: &mut [bool],
    columns: u16,
    cursor: teamy_terminal_core::CursorPosition,
) {
    if cursor.column >= columns {
        return;
    }
    let index = usize::from(cursor.row) * usize::from(columns) + usize::from(cursor.column);
    if let Some(cell) = changed.get_mut(index) {
        *cell = true;
    }
}

fn terminal_input(event: &KeyEvent, modifiers: ModifiersState) -> Result<Option<Vec<u8>>> {
    terminal_input_key(&event.logical_key, event.text.as_deref(), modifiers)
}

fn terminal_input_key(
    key: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
) -> Result<Option<Vec<u8>>> {
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
            "c" => return Ok(Some(vec![0x03])),
            "d" => return Ok(Some(vec![0x04])),
            "z" => return Ok(Some(vec![0x1a])),
            "l" => return Ok(Some(vec![0x0c])),
            "v" => {
                let mut clipboard =
                    Clipboard::new().wrap_err("failed to open the system clipboard for paste")?;
                let text = clipboard
                    .get_text()
                    .wrap_err("failed to read text from the system clipboard")?;
                return Ok(Some(text.into_bytes()));
            }
            _ => {}
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
        ControlKey::CtrlC => vec![0x03],
        ControlKey::CtrlD => vec![0x04],
        ControlKey::CtrlZ => vec![0x1a],
        ControlKey::CtrlL => vec![0x0c],
    };
    if bytes.len() > teamy_terminal_control::MAX_INPUT_BYTES {
        eyre::bail!("control key text exceeds the input limit")
    }
    Ok(bytes)
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
            terminal_input_key(&character("c"), None, ModifiersState::CONTROL)
                .expect("Ctrl+C mapping should not fail"),
            Some(vec![0x03])
        );
        assert_eq!(
            terminal_input_key(&character("d"), None, ModifiersState::CONTROL)
                .expect("Ctrl+D mapping should not fail"),
            Some(vec![0x04])
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
            terminal_input_key(&character("v"), None, ModifiersState::SHIFT)
                .expect("ordinary text mapping should not fail"),
            Some(b"v".to_vec())
        );
    }

    #[test]
    fn embedded_window_icon_decodes_for_winit() {
        load_window_icon().expect("the checked-in window icon should decode");
    }

    #[test]
    fn navigation_and_text_keys_use_pty_sequences() {
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
        terminal.process_bytes(b"\x1b[?25labcdefgh");
        let previous = terminal.snapshot();
        terminal.process_bytes(b"\x1b[1GZ");
        let current = terminal.snapshot();

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(!full_refresh);
        assert_eq!(regions, vec![CellRect::new(0, 0, 2, 1)]);
    }

    #[cfg(feature = "dirty_rendering")]
    #[test]
    fn damage_regions_force_full_refresh_after_resize() {
        let previous = teamy_terminal_core::TerminalSnapshot {
            columns: 2,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition::default(),
            cursor_visible: false,
            cells: vec![teamy_terminal_core::Cell::default(); 2],
            scrollback_rows: 0,
        };
        let current = teamy_terminal_core::TerminalSnapshot {
            columns: 3,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition::default(),
            cursor_visible: false,
            cells: vec![teamy_terminal_core::Cell::default(); 3],
            scrollback_rows: 0,
        };

        let (regions, full_refresh) = damage_regions(Some(&previous), &current);

        assert!(full_refresh);
        assert_eq!(regions, vec![CellRect::full(3, 1)]);
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
            cells: cells.clone(),
            scrollback_rows: 0,
        };
        let current = teamy_terminal_core::TerminalSnapshot {
            columns: 8,
            rows: 1,
            cursor: teamy_terminal_core::CursorPosition { column: 7, row: 0 },
            cursor_visible: true,
            cells,
            scrollback_rows: 0,
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
            cells: vec![teamy_terminal_core::Cell::default(); 2],
            scrollback_rows: 0,
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
        let width = u32::from(font.cell_width()) * 8;
        let height = u32::from(font.cell_height());
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
