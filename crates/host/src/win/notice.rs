//! The bar at the top of the screen while someone is connected: who it is, and a button
//! that ends the session. Whoever sits at the computer always knows, and can say no.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_INFOBK, DEFAULT_GUI_FONT, GetStockObject, GetSysColorBrush, HBRUSH, SetBkMode, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, HMENU,
    HWND_TOPMOST, IDC_ARROW, LoadCursorW, MSG, PostMessageW, RegisterClassW, SM_CXSCREEN, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SendMessageW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage,
    WM_APP, WM_COMMAND, WM_CTLCOLORSTATIC, WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CHILD, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const WM_REFRESH: u32 = WM_APP + 1;
const ID_END: usize = 1;
const WIDTH: i32 = 520;
const HEIGHT: i32 = 36;

static WINDOW: AtomicIsize = AtomicIsize::new(0);
static LABEL: AtomicIsize = AtomicIsize::new(0);
static TEXT: Mutex<Option<String>> = Mutex::new(None);
static ON_END: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Start the window thread (once per process). `on_end` runs when the button is clicked.
pub fn start(on_end: impl Fn() + Send + Sync + 'static) {
    if ON_END.set(Box::new(on_end)).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new().name("rdesk-notice".into()).spawn(|| unsafe { window_thread() });
    if let Err(e) = spawned {
        tracing::warn!("session notice: {e}");
    }
}

/// Show `text` (someone is connected) or hide the bar (`None`).
pub fn show(text: Option<String>) {
    *TEXT.lock().unwrap() = text;
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd as HWND, WM_REFRESH, 0, 0) };
    }
}

unsafe fn window_thread() {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide("rdesk-session-notice");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_INFOBK + 1) as usize as HBRUSH,
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let x = (GetSystemMetrics(SM_CXSCREEN) - WIDTH) / 2;
        let title = wide("rdesk");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_BORDER,
            x.max(0),
            0,
            WIDTH,
            HEIGHT,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            tracing::warn!("session notice: could not create the window");
            return;
        }
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let static_class = wide("STATIC");
        let empty = wide("");
        let label = CreateWindowExW(
            0,
            static_class.as_ptr(),
            empty.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            10,
            10,
            WIDTH - 130,
            HEIGHT - 16,
            hwnd,
            null_mut(),
            hinst,
            null(),
        );
        let button_class = wide("BUTTON");
        let end = wide("End session");
        let button = CreateWindowExW(
            0,
            button_class.as_ptr(),
            end.as_ptr(),
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
            WIDTH - 112,
            5,
            100,
            HEIGHT - 12,
            hwnd,
            ID_END as HMENU,
            hinst,
            null(),
        );
        for h in [label, button] {
            SendMessageW(h, WM_SETFONT, font as WPARAM, 1);
        }
        LABEL.store(label as isize, Ordering::Release);
        WINDOW.store(hwnd as isize, Ordering::Release);
        // A session may have started before the window existed.
        PostMessageW(hwnd, WM_REFRESH, 0, 0);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_REFRESH => {
                match TEXT.lock().unwrap().clone() {
                    Some(text) => {
                        let label = LABEL.load(Ordering::Acquire) as HWND;
                        let w = wide(&text);
                        SetWindowTextW(label, w.as_ptr());
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        // Stay above windows that became topmost meanwhile.
                        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                    None => {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
                0
            }
            WM_COMMAND if (wparam & 0xffff) == ID_END => {
                if let Some(f) = ON_END.get() {
                    f();
                }
                0
            }
            WM_CTLCOLORSTATIC => {
                SetBkMode(wparam as _, TRANSPARENT as _);
                GetSysColorBrush(COLOR_INFOBK) as LRESULT
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
