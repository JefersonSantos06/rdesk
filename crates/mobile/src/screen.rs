//! The phone's screen for the host: Android's MediaProjection (in the app's Kotlin,
//! `ScreenCaptureService`) draws into an `ImageReader`, and each new picture comes here
//! through JNI (`ScreenBridge`). The host's capture thread takes the newest one.
//!
//! A still screen produces no new pictures, so the last one is kept and handed out again
//! (the video loop needs one for a full refresh, e.g. after a reconnection).

// Elsewhere nothing feeds it (only the JNI calls below do), but it builds and is tested.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use rdesk_common::tiles::Frame;
use rdesk_host::capture::{Backend, InputRect, MonitorDesc};
use rdesk_proto::MonitorInfo;

/// How long a request waits for a new picture before the last one is used again.
const STILL_WAIT: Duration = Duration::from_millis(200);

#[derive(Default)]
struct State {
    /// The screen being shared (pixels), `None` when it is not.
    size: Option<(u32, u32)>,
    /// The newest picture, not handed out yet.
    fresh: Option<Frame>,
    /// The last picture handed out.
    last: Option<Frame>,
}

static STATE: Mutex<State> = Mutex::new(State { size: None, fresh: None, last: None });

/// Tests that share the screen take turns (the state is global).
#[cfg(test)]
pub(crate) static TEST_TURN: Mutex<()> = Mutex::new(());
static NEW: Condvar = Condvar::new();

/// Sharing started, or the screen turned (a new size).
pub fn started(width: u32, height: u32) {
    let mut st = STATE.lock().unwrap();
    st.size = Some((width, height));
    st.fresh = None;
    tracing::info!("sharing the screen: {width}x{height}");
}

/// Sharing ended (the person stopped it, the screen locked, or the app stopped it).
pub fn stopped() {
    let mut st = STATE.lock().unwrap();
    *st = State::default();
    NEW.notify_all();
}

pub fn sharing() -> bool {
    STATE.lock().unwrap().size.is_some()
}

/// A new picture: `data` is RGBA rows `row_stride` bytes apart.
pub fn push(width: u32, height: u32, row_stride: usize, data: &[u8]) {
    let row = width as usize * 4;
    if width == 0 || height == 0 || row_stride < row || data.len() < row_stride * (height as usize - 1) + row {
        tracing::warn!(
            "screen picture of an unexpected shape: {width}x{height}, stride {row_stride}, {} bytes",
            data.len()
        );
        return;
    }
    let mut st = STATE.lock().unwrap();
    if st.size.is_none() {
        return;
    }
    // Reuse the allocation of a picture nobody took yet.
    let mut pixels = st.fresh.take().map(|f| f.data).unwrap_or_default();
    pixels.clear();
    pixels.reserve(row * height as usize);
    if row_stride == row {
        pixels.extend_from_slice(&data[..row * height as usize]);
    } else {
        for r in data.chunks(row_stride).take(height as usize) {
            pixels.extend_from_slice(&r[..row]);
        }
    }
    st.fresh = Some(Frame { width, height, data: pixels });
    NEW.notify_all();
}

/// The host's view of the phone's screen: one monitor, the whole display.
pub struct PhoneScreen;

impl Backend for PhoneScreen {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        let Some((w, h)) = STATE.lock().unwrap().size else {
            bail!(rdesk_common::t!("the screen is not being shared", "a tela não está sendo compartilhada"))
        };
        Ok(vec![MonitorDesc {
            info: MonitorInfo {
                id: 1,
                name: rdesk_common::local_hostname(),
                x: 0,
                y: 0,
                width: w,
                height: h,
                scale: 1.0,
                primary: true,
            },
            input_rect: InputRect { x: 0, y: 0, w, h },
        }])
    }

    fn grab(&mut self, _id: u32) -> Result<Frame> {
        let mut st = STATE.lock().unwrap();
        if st.fresh.is_none() && st.size.is_some() {
            st = NEW.wait_timeout(st, STILL_WAIT).unwrap().0;
        }
        if st.size.is_none() {
            bail!(rdesk_common::t!("the screen is not being shared", "a tela não está sendo compartilhada"))
        }
        if let Some(f) = st.fresh.take() {
            st.last = Some(Frame { width: f.width, height: f.height, data: f.data.clone() });
            return Ok(f);
        }
        match &st.last {
            Some(f) => Ok(Frame { width: f.width, height: f.height, data: f.data.clone() }),
            None => bail!("no picture of the screen yet"),
        }
    }
}

/// What the app's Kotlin calls (`dev.rdesk.rdesk.ScreenBridge`).
#[cfg(target_os = "android")]
mod jni_exports {
    use jni::JNIEnv;
    use jni::objects::{JByteBuffer, JClass};
    use jni::sys::{jboolean, jint};

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_ScreenBridge_started(_: JNIEnv, _: JClass, width: jint, height: jint) {
        super::started(width.max(0) as u32, height.max(0) as u32);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_ScreenBridge_frame(
        env: JNIEnv,
        _: JClass,
        buf: JByteBuffer,
        width: jint,
        height: jint,
        row_stride: jint,
    ) {
        let (Ok(ptr), Ok(len)) = (env.get_direct_buffer_address(&buf), env.get_direct_buffer_capacity(&buf)) else {
            tracing::warn!("screen picture without a direct buffer");
            return;
        };
        if ptr.is_null() {
            return;
        }
        // SAFETY: the buffer is the image plane the Kotlin side holds open (and does not touch)
        // until this call returns; `len` is its capacity.
        let data = unsafe { std::slice::from_raw_parts(ptr, len) };
        super::push(width.max(0) as u32, height.max(0) as u32, row_stride.max(0) as usize, data);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_ScreenBridge_stopped(_: JNIEnv, _: JClass, by_person: jboolean) {
        super::stopped();
        crate::api::host::screen_stopped(by_person != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test: the state is global.
    #[test]
    fn pictures_go_out_newest_first_and_the_last_one_again() {
        let _turn = TEST_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let mut src = PhoneScreen;
        assert!(src.list().is_err(), "not sharing yet");
        started(2, 2);
        assert_eq!(src.list().unwrap()[0].info.width, 2);
        // Rows 12 bytes apart: 4 bytes of padding each.
        let padded: Vec<u8> = (0..24).collect();
        push(2, 2, 12, &padded);
        let f = src.grab(1).unwrap();
        assert_eq!(f.data, [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19]);
        // Nothing new: the same picture again.
        assert_eq!(src.grab(1).unwrap().data, f.data);
        // Too short for its size: ignored.
        push(2, 2, 8, &[0; 10]);
        assert_eq!(src.grab(1).unwrap().data, f.data);
        stopped();
        assert!(src.grab(1).is_err());
        assert!(!sharing());
    }
}
