#![cfg(windows)]

use std::net::TcpListener;
use std::net::TcpStream;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
use windows::Win32::UI::WindowsAndMessaging::GW_OWNER;
use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
use windows::Win32::UI::WindowsAndMessaging::GetWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use windows::core::BOOL;

#[derive(Debug)]
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug)]
struct Enumeration {
    process_id: u32,
    windows: Vec<WindowEvidence>,
}

#[derive(Debug, Eq, PartialEq)]
struct WindowEvidence {
    hwnd: isize,
    class_name: String,
    title: String,
    visible: bool,
    ex_style: isize,
    owner: isize,
    taskbar_candidate: bool,
}

unsafe extern "system" fn collect_process_windows(window: HWND, state: LPARAM) -> BOOL {
    // SAFETY: EnumWindows receives a live stack-owned Enumeration pointer for
    // the duration of this synchronous callback traversal.
    let state = unsafe { &mut *(state.0 as *mut Enumeration) };
    let mut owner = 0_u32;
    // SAFETY: EnumWindows supplied the HWND and owner points to live storage.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut owner)) };
    if owner == state.process_id {
        let mut class_name = [0_u16; 256];
        let mut title = [0_u16; 256];
        // SAFETY: Both buffers are valid mutable UTF-16 storage and window is
        // supplied by EnumWindows for the duration of this callback.
        let class_name_len =
            usize::try_from(unsafe { GetClassNameW(window, &mut class_name) }.max(0))
                .expect("non-negative class-name length should fit usize");
        // SAFETY: Same lifetime and buffer argument as GetClassNameW above.
        let title_len = usize::try_from(unsafe { GetWindowTextW(window, &mut title) }.max(0))
            .expect("non-negative window-title length should fit usize");
        // SAFETY: The remaining calls inspect the EnumWindows-owned HWND only.
        let visible = unsafe { IsWindowVisible(window) }.as_bool();
        // SAFETY: GWL_EXSTYLE reads one scalar from the valid HWND.
        let ex_style = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) };
        // SAFETY: GW_OWNER only reads the relationship for the valid EnumWindows HWND.
        let owner = unsafe { GetWindow(window, GW_OWNER) }.map_or(0, |owner| owner.0 as isize);
        let ex_style_bits = ex_style.cast_unsigned();
        let tool_window = usize::try_from(WS_EX_TOOLWINDOW.0)
            .expect("Win32 extended style should fit the current pointer width");
        let app_window = usize::try_from(WS_EX_APPWINDOW.0)
            .expect("Win32 extended style should fit the current pointer width");
        let taskbar_candidate = visible
            && (ex_style_bits & tool_window == 0)
            && (owner == 0 || (ex_style_bits & app_window != 0));
        state.windows.push(WindowEvidence {
            hwnd: window.0 as isize,
            class_name: String::from_utf16_lossy(&class_name[..class_name_len]),
            title: String::from_utf16_lossy(&title[..title_len]),
            visible,
            ex_style,
            owner,
            taskbar_candidate,
        });
    }
    true.into()
}

fn top_level_windows(process_id: u32) -> Vec<WindowEvidence> {
    let mut state = Enumeration {
        process_id,
        windows: Vec::new(),
    };
    // SAFETY: The callback is synchronous and state remains alive until it
    // returns. The callback only records HWND values owned by process_id.
    unsafe {
        EnumWindows(
            Some(collect_process_windows),
            LPARAM((&raw mut state).cast::<core::ffi::c_void>() as isize),
        )
    }
    .expect("EnumWindows should enumerate the current desktop");
    state.windows
}

#[test]
fn serve_process_creates_no_visible_or_taskbar_window() {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    let address = reservation.local_addr().expect("reserved loopback address");
    drop(reservation);

    let child = Command::new(env!("CARGO_BIN_EXE_teamy-terminal"))
        .arg("serve")
        .arg(address.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn no-window terminal server");
    let mut child = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            break;
        }
        if let Some(status) = child.0.try_wait().expect("inspect terminal server") {
            panic!("terminal server exited before listening: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "terminal server did not begin listening within 10 seconds"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Give the service enough time to initialize its cached no-window Vulkan
    // capability after the TCP listener becomes observable.
    thread::sleep(Duration::from_millis(100));
    let windows = top_level_windows(child.0.id());
    assert!(
        windows
            .iter()
            .all(|window| !window.visible && !window.taskbar_candidate),
        "teamy-terminal serve must not create a visible or taskbar-capable window: {windows:?}"
    );
}
