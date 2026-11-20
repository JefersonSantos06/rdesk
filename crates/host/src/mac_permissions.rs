//! macOS: the two permissions a host needs, Screen Recording (to see the screen) and
//! Accessibility (to move the mouse and type). The system asks the person once when the host
//! starts; until both are given, the app's "This computer" page says what is missing and
//! opens the right pane of System Settings.

use std::ffi::c_void;
use std::sync::Once;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    // Opaque structs: only their addresses are used.
    static kCFTypeDictionaryKeyCallBacks: [u8; 0];
    static kCFTypeDictionaryValueCallBacks: [u8; 0];
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// Screen Recording is allowed: captures show the screen (not just the wallpaper).
pub fn screen_allowed() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Accessibility is allowed: remote clicks and keys work.
pub fn input_allowed() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Ask the system for what is still missing (it shows its own dialogs, once per process).
pub fn request() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if !screen_allowed() {
            unsafe {
                CGRequestScreenCaptureAccess();
            }
        }
        if !input_allowed() {
            unsafe {
                let keys = [kAXTrustedCheckOptionPrompt];
                let values = [kCFBooleanTrue];
                let options = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &raw const kCFTypeDictionaryKeyCallBacks as *const c_void,
                    &raw const kCFTypeDictionaryValueCallBacks as *const c_void,
                );
                AXIsProcessTrustedWithOptions(options);
                if !options.is_null() {
                    CFRelease(options);
                }
            }
        }
    });
}

/// Which pane of System Settings to open.
#[derive(Clone, Copy, Debug)]
pub enum Pane {
    ScreenRecording,
    Accessibility,
}

/// Open System Settings on that pane (where the person adds rdesk and turns it on).
pub fn open_settings(pane: Pane) {
    let anchor = match pane {
        Pane::ScreenRecording => "Privacy_ScreenCapture",
        Pane::Accessibility => "Privacy_Accessibility",
    };
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("opening System Settings: {e}");
    }
}
