use arboard::Clipboard;
use eyre::Context;
use eyre::Result;
use std::fs::File;
use std::io::BufWriter;
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
use teamy_terminal_font::TerminalFont;
use teamy_terminal_pty::PtyConfig;
use teamy_terminal_pty::PtyEvent;
use teamy_terminal_pty::PtySession;
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
use winit::window::Window;
use winit::window::WindowId;

const INITIAL_WINDOW_WIDTH: u32 = 1280;
const INITIAL_WINDOW_HEIGHT: u32 = 720;
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

struct InteractiveApplication {
    cancellation_token: CancellationToken,
    session: Option<PtySession>,
    terminal: TerminalSession,
    font: TerminalFont,
    window: Option<Window>,
    renderer: Option<VulkanRenderer>,
    modifiers: ModifiersState,
    dirty: bool,
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
        if self.window.is_some() {
            return Ok(());
        }
        let attributes = Window::default_attributes()
            .with_title("Teamy Terminal — Vulkan")
            .with_inner_size(PhysicalSize::new(
                INITIAL_WINDOW_WIDTH,
                INITIAL_WINDOW_HEIGHT,
            ));
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
        Ok(())
    }

    fn drain_pty(&mut self, event_loop: &ActiveEventLoop) {
        while self.session.is_some() {
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
            match event {
                PtyEvent::Output(bytes) => {
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
        if !self.dirty {
            return;
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let extent = renderer.extent();
        let snapshot = self.terminal.snapshot();
        let frame = self.font.render(&snapshot, extent.width, extent.height);
        match renderer.render(&frame) {
            Ok(needs_recreate) => {
                self.dirty = false;
                if needs_recreate {
                    let Some(window) = self.window.as_ref() else {
                        return;
                    };
                    if let Err(error) = renderer
                        .recreate(window)
                        .and_then(|()| self.resize_terminal())
                    {
                        self.fail(error, event_loop);
                    } else {
                        self.dirty = true;
                    }
                }
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
                if let Some(renderer) = self.renderer.as_mut()
                    && let Some(window) = self.window.as_ref()
                    && let Err(error) = renderer
                        .recreate(window)
                        .and_then(|()| self.resize_terminal())
                {
                    self.fail(error, event_loop);
                }
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

fn terminal_input(event: &KeyEvent, modifiers: ModifiersState) -> Result<Option<Vec<u8>>> {
    terminal_input_key(&event.logical_key, event.text.as_deref(), modifiers)
}

fn terminal_input_key(
    key: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
) -> Result<Option<Vec<u8>>> {
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
            terminal_input_key(&character("v"), None, ModifiersState::SHIFT)
                .expect("ordinary text mapping should not fail"),
            Some(b"v".to_vec())
        );
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
}
