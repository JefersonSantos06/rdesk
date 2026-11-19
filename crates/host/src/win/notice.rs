//! The bar at the top of the screen while someone is connected: who it is, and a button
//! that ends the session. Whoever sits at the computer always knows, and can say no.
//!
//! It stays out of the way: a small pill for a few seconds when a session starts, then a
//! thin tab at the top edge that opens again under the mouse. The pill can be dragged.

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, Ellipse, EndPaint,
    FW_NORMAL, FW_SEMIBOLD, FillRect, GetDC, GetStockObject, GetTextExtentPoint32W, HDC, HFONT, HGDIOBJ,
    InvalidateRect, NULL_PEN, OUT_DEFAULT_PRECIS, PAINTSTRUCT, ReleaseDC, RoundRect, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, GetSystemMetrics, GetWindowRect,
    HTCAPTION, HWND_TOPMOST, IDC_ARROW, IDC_HAND, KillTimer, LoadCursorW, MSG, PostMessageW, RegisterClassW,
    SM_CXSCREEN, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SendMessageW, SetCursor, SetTimer, SetWindowPos, ShowWindow,
    TranslateMessage, WM_APP, WM_DPICHANGED, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_NCLBUTTONDOWN, WM_PAINT, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

const WM_REFRESH: u32 = WM_APP + 1;
/// From `Win32_UI_Controls`, not worth the feature.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_COLLAPSE: usize = 1;
/// How long the pill stays open when a session starts.
const OPEN_MS: u32 = 5000;
/// How long it stays open after the mouse leaves it.
const LEAVE_MS: u32 = 1500;

// Sizes at 96 DPI.
const PILL_H: i32 = 28;
const PAD: i32 = 10;
const DOT: i32 = 8;
const GAP: i32 = 8;
const BUTTON_W: i32 = 48;
const BUTTON_H: i32 = 20;
const MAX_TEXT_W: i32 = 300;
const TAB_W: i32 = 48;
const TAB_H: i32 = 8;
const FONT_H: i32 = 12;

const BG: u32 = rgb(32, 32, 32);
const FG: u32 = rgb(240, 240, 240);
/// The dot in the pill and the collapsed tab.
const ACCENT: u32 = rgb(234, 128, 32);
const END: u32 = rgb(196, 43, 28);
const END_HOVER: u32 = rgb(226, 66, 50);

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

static WINDOW: AtomicIsize = AtomicIsize::new(0);
static TEXT: Mutex<Option<String>> = Mutex::new(None);
static ON_END: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

struct State {
    text: String,
    shown: bool,
    open: bool,
    /// Top center of the bar in screen pixels (dragging moves it).
    anchor: (i32, i32),
    dpi: i32,
    font: HFONT,
    bold: HFONT,
    text_w: i32,
    /// The mouse is over the window (a leave notification is pending).
    tracking: bool,
    over_button: bool,
    pressed: bool,
}

impl State {
    fn px(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }

    fn size(&self) -> (i32, i32) {
        if self.open {
            (self.px(PAD + DOT + GAP) + self.text_w + self.px(GAP + BUTTON_W + 4), self.px(PILL_H))
        } else {
            (self.px(TAB_W), self.px(TAB_H))
        }
    }

    fn button(&self) -> RECT {
        let (w, _) = self.size();
        let top = self.px((PILL_H - BUTTON_H) / 2);
        RECT { left: w - self.px(4 + BUTTON_W), top, right: w - self.px(4), bottom: top + self.px(BUTTON_H) }
    }

    fn on_button(&self, x: i32, y: i32) -> bool {
        let b = self.button();
        self.open && x >= b.left && x < b.right && y >= b.top && y < b.bottom
    }

    fn set_dpi(&mut self, dpi: i32) {
        self.dpi = dpi.max(96);
        unsafe {
            for f in [self.font, self.bold] {
                if !f.is_null() {
                    DeleteObject(f as HGDIOBJ);
                }
            }
        }
        self.font = font(self.px(FONT_H), FW_NORMAL as i32);
        self.bold = font(self.px(FONT_H), FW_SEMIBOLD as i32);
        self.measure();
    }

    fn measure(&mut self) {
        let w = wide(&self.text);
        let mut size = SIZE { cx: 0, cy: 0 };
        unsafe {
            let dc = GetDC(null_mut());
            let old = SelectObject(dc, self.font as HGDIOBJ);
            GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32 - 1, &mut size);
            SelectObject(dc, old);
            ReleaseDC(null_mut(), dc);
        }
        self.text_w = size.cx.min(self.px(MAX_TEXT_W));
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Run `f` on the window's state. Never call into Windows from inside `f` in a way that
/// can send a message back to the window (it would find the state borrowed).
fn with<R: Default>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| s.borrow_mut().as_mut().map(f).unwrap_or_default())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn font(height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            0,
            face.as_ptr(),
        )
    }
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
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let title = wide("rdesk");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            tracing::warn!("session notice: could not create the window");
            return;
        }
        // Windows 11 rounds it; earlier versions ignore this.
        let corners = DWMWCP_ROUNDSMALL;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corners as *const _ as *const _,
            size_of_val(&corners) as u32,
        );
        let mut state = State {
            text: String::new(),
            shown: false,
            open: false,
            anchor: (GetSystemMetrics(SM_CXSCREEN) / 2, 0),
            dpi: 96,
            font: null_mut(),
            bold: null_mut(),
            text_w: 0,
            tracking: false,
            over_button: false,
            pressed: false,
        };
        state.set_dpi(GetDpiForWindow(hwnd) as i32);
        STATE.with(|s| *s.borrow_mut() = Some(state));
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

/// Size and place the window for its state, above other topmost windows.
unsafe fn place(hwnd: HWND) {
    let Some((x, y, w, h)) = STATE.with(|s| {
        s.borrow().as_ref().filter(|s| s.shown).map(|s| {
            let (w, h) = s.size();
            (s.anchor.0 - w / 2, s.anchor.1, w, h)
        })
    }) else {
        return;
    };
    unsafe {
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        InvalidateRect(hwnd, null(), 0);
    }
}

unsafe fn paint(s: &State, dc: HDC, rc: &RECT) {
    unsafe {
        let fill_round = |r: &RECT, color: u32, radius: i32| {
            let brush = CreateSolidBrush(color);
            let old_brush = SelectObject(dc, brush as HGDIOBJ);
            let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
            RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, radius, radius);
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(brush as HGDIOBJ);
        };
        let bg = CreateSolidBrush(if s.open { BG } else { ACCENT });
        FillRect(dc, rc, bg);
        DeleteObject(bg as HGDIOBJ);
        if !s.open {
            return;
        }

        // The dot.
        let d = s.px(DOT);
        let x = s.px(PAD);
        let y = (rc.bottom - d) / 2;
        let dot = CreateSolidBrush(ACCENT);
        let old_brush = SelectObject(dc, dot as HGDIOBJ);
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        Ellipse(dc, x, y, x + d + 1, y + d + 1);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(dot as HGDIOBJ);

        // Who is connected.
        SetBkMode(dc, TRANSPARENT as _);
        SetTextColor(dc, FG);
        let old_font = SelectObject(dc, s.font as HGDIOBJ);
        let button = s.button();
        let mut text_rc = RECT { left: x + d + s.px(GAP), top: 0, right: button.left - s.px(GAP), bottom: rc.bottom };
        let mut text = wide(&s.text);
        DrawTextW(
            dc,
            text.as_mut_ptr(),
            text.len() as i32 - 1,
            &mut text_rc,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        // The button.
        fill_round(&button, if s.over_button { END_HOVER } else { END }, s.px(6));
        SelectObject(dc, s.bold as HGDIOBJ);
        let mut label = wide("End");
        let mut label_rc = button;
        DrawTextW(
            dc,
            label.as_mut_ptr(),
            label.len() as i32 - 1,
            &mut label_rc,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );
        SelectObject(dc, old_font);
    }
}

fn point(lparam: LPARAM) -> (i32, i32) {
    ((lparam & 0xffff) as i16 as i32, ((lparam >> 16) & 0xffff) as i16 as i32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_REFRESH => {
                match TEXT.lock().unwrap().clone() {
                    Some(text) => {
                        let opened = with(|s| {
                            let new = !s.shown || s.text != text;
                            if new {
                                s.text = text;
                                s.measure();
                                s.shown = true;
                                s.open = true;
                            }
                            new
                        });
                        if opened {
                            SetTimer(hwnd, TIMER_COLLAPSE, OPEN_MS, None);
                        }
                        // Also stays above windows that became topmost meanwhile.
                        place(hwnd);
                    }
                    None => {
                        with(|s| s.shown = false);
                        KillTimer(hwnd, TIMER_COLLAPSE);
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
                0
            }
            WM_TIMER if wparam == TIMER_COLLAPSE => {
                KillTimer(hwnd, TIMER_COLLAPSE);
                let collapse = with(|s| {
                    let collapse = s.open && !s.tracking;
                    if collapse {
                        s.open = false;
                        s.over_button = false;
                    }
                    collapse
                });
                if collapse {
                    place(hwnd);
                }
                0
            }
            WM_MOUSEMOVE => {
                let (x, y) = point(lparam);
                let (track, reopen, redraw) = with(|s| {
                    let track = !s.tracking;
                    s.tracking = true;
                    let reopen = !s.open;
                    s.open = true;
                    let over = !reopen && s.on_button(x, y);
                    let redraw = over != s.over_button;
                    s.over_button = over;
                    (track, reopen, redraw)
                });
                if track {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut tme);
                }
                KillTimer(hwnd, TIMER_COLLAPSE);
                if reopen {
                    place(hwnd);
                } else if redraw {
                    InvalidateRect(hwnd, null(), 0);
                }
                0
            }
            WM_MOUSELEAVE => {
                with(|s| {
                    s.tracking = false;
                    s.over_button = false;
                    s.pressed = false;
                });
                SetTimer(hwnd, TIMER_COLLAPSE, LEAVE_MS, None);
                InvalidateRect(hwnd, null(), 0);
                0
            }
            WM_LBUTTONDOWN => {
                let (x, y) = point(lparam);
                let (on_button, open) = with(|s| {
                    let on = s.on_button(x, y);
                    s.pressed = on;
                    (on, s.open)
                });
                if open && !on_button {
                    // Drag the pill (the system's move loop returns when the button is released).
                    ReleaseCapture();
                    SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as WPARAM, 0);
                    let mut r: RECT = std::mem::zeroed();
                    GetWindowRect(hwnd, &mut r);
                    with(|s| s.anchor = (r.left + (r.right - r.left) / 2, r.top));
                }
                0
            }
            WM_LBUTTONUP => {
                let (x, y) = point(lparam);
                let end = with(|s| std::mem::take(&mut s.pressed) && s.on_button(x, y));
                if end && let Some(f) = ON_END.get() {
                    f();
                }
                0
            }
            WM_SETCURSOR if with(|s| s.over_button) => {
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                1
            }
            WM_DPICHANGED => {
                with(|s| s.set_dpi((wparam & 0xffff) as i32));
                place(hwnd);
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                STATE.with(|s| {
                    if let Some(s) = s.borrow().as_ref() {
                        paint(s, dc, &rc);
                    }
                });
                EndPaint(hwnd, &ps);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
