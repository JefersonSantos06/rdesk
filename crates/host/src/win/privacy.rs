//! Privacy mode: a black screen for whoever is at the computer while the session sees the
//! desktop as it is. A window covers every monitor and asks Windows to leave it out of
//! screen capture (`WDA_EXCLUDEFROMCAPTURE`, Windows 10 2004 and newer), so the capture sees
//! what is under it. Clicks go through it (layered and transparent to input), so the remote
//! side still controls the desktop. Together with blocking the local keyboard and mouse it is
//! the "black screen" of other remote desktops.

use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{Result, anyhow};
use rdesk_common::t;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint,
    FillRect, HGDIOBJ, PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW, GetSystemMetrics,
    LWA_ALPHA, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNOACTIVATE, SetLayeredWindowAttributes, SetWindowDisplayAffinity,
    ShowWindow, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Not in windows-sys's constants for this version of the feature set.
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

static WINDOW: AtomicIsize = AtomicIsize::new(0);
static LOCK: Mutex<()> = Mutex::new(());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Windows 10 2004 (build 19041) or newer: windows can be left out of capture.
pub fn supported() -> bool {
    // RtlGetVersion tells the truth (GetVersionEx lies to unmanifested programs).
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfo) -> i32;
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
        let lib = LoadLibraryA(c"ntdll.dll".as_ptr() as _);
        let Some(f) = GetProcAddress(lib, c"RtlGetVersion".as_ptr() as _) else { return false };
        let f: RtlGetVersion = std::mem::transmute(f);
        let mut v: OsVersionInfo = std::mem::zeroed();
        v.size = size_of::<OsVersionInfo>() as u32;
        f(&mut v) == 0 && (v.major > 10 || (v.major == 10 && v.build >= 19041))
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut r);
            let black = CreateSolidBrush(0);
            FillRect(dc, &r, black);
            DeleteObject(black as HGDIOBJ);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00A0A0A0);
            let mut text = wide(t!(
                "This computer is being used remotely (rdesk). Ctrl+Alt+Del gives back the keyboard and mouse.",
                "Este computador está sendo usado remotamente (rdesk). Ctrl+Alt+Del devolve o teclado e o mouse."
            ));
            DrawTextW(dc, text.as_mut_ptr(), -1, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            EndPaint(hwnd, &ps);
            0
        },
        WM_CLOSE => unsafe {
            DestroyWindow(hwnd);
            0
        },
        WM_DESTROY => unsafe {
            PostQuitMessage(0);
            0
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Show or hide the black screen.
pub fn set(on: bool) -> Result<()> {
    let _g = LOCK.lock().unwrap();
    let current = WINDOW.load(Ordering::Acquire);
    if !on {
        if current != 0 {
            unsafe { PostMessageW(current as HWND, WM_CLOSE, 0, 0) };
            WINDOW.store(0, Ordering::Release);
        }
        return Ok(());
    }
    if current != 0 {
        return Ok(());
    }
    if !supported() {
        return Err(anyhow!("needs Windows 10 version 2004 or newer"));
    }
    let (tx, rx) = std::sync::mpsc::channel::<Result<isize>>();
    std::thread::Builder::new().name("rdesk-privacy".into()).spawn(move || {
        // On the desktop with the keyboard (the host may run as SYSTEM).
        let _ = crate::win::desktop::follow_input_desktop();
        unsafe {
            let class = wide("rdesk-privacy");
            let inst = GetModuleHandleW(null_mut());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: inst,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                x,
                y,
                w,
                h,
                null_mut(),
                null_mut(),
                inst,
                null_mut(),
            );
            if hwnd.is_null() {
                let _ = tx.send(Err(anyhow!("could not create the window")));
                return;
            }
            SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
            // Without this the session would see black too: refuse rather than blind it.
            if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) == 0 {
                DestroyWindow(hwnd);
                let _ = tx.send(Err(anyhow!("Windows refused to leave the window out of the capture")));
                return;
            }
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = tx.send(Ok(hwnd as isize));
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    })?;
    let hwnd = rx.recv().map_err(|_| anyhow!("privacy window thread ended"))??;
    WINDOW.store(hwnd, Ordering::Release);
    Ok(())
}
