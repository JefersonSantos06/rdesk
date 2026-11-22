//! The phone's sound for the host: what it plays, captured by the app's Kotlin
//! (`ScreenCaptureService`: `AudioRecord` with playback capture, Android 10+) and encoded there
//! as Opus (`MediaCodec`, 20 ms packets at 48 kHz, stereo), comes here through JNI
//! (`AudioBridge`). Capture runs only while a session listens ([`wanted`]).

// Elsewhere nothing feeds it (only the JNI calls below do), but it builds and is tested.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use tokio::sync::mpsc;

/// Packets waiting to be sent before the newest are dropped (latency over completeness).
const QUEUE: usize = 10;

/// The app can capture the phone's sound now (sharing, Android 10+, the permission given).
static AVAILABLE: AtomicBool = AtomicBool::new(false);
/// Where packets go while a session listens.
static LISTENER: Mutex<Option<mpsc::Sender<Vec<u8>>>> = Mutex::new(None);

pub fn available() -> bool {
    AVAILABLE.load(Ordering::Acquire)
}

/// The host's audio source on the phone: packets until the receiver is dropped.
pub fn start() -> Result<mpsc::Receiver<Vec<u8>>> {
    if !available() {
        bail!(rdesk_common::t!(
            "this phone cannot send its sound (Android 10 or newer, with the microphone permission)",
            "este celular não pode enviar o som (Android 10 ou mais novo, com a permissão do microfone)"
        ));
    }
    let (tx, rx) = mpsc::channel(QUEUE);
    *LISTENER.lock().unwrap() = Some(tx);
    Ok(rx)
}

/// A session listens (the app captures while this is true).
pub fn wanted() -> bool {
    LISTENER.lock().unwrap().as_ref().is_some_and(|tx| !tx.is_closed())
}

/// One Opus packet from the app.
pub fn packet(data: Vec<u8>) {
    let mut l = LISTENER.lock().unwrap();
    if let Some(tx) = l.as_ref() {
        match tx.try_send(data) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => *l = None,
        }
    }
}

pub fn set_available(on: bool) {
    AVAILABLE.store(on, Ordering::Release);
    if !on {
        *LISTENER.lock().unwrap() = None;
    }
}

/// What the app's Kotlin calls (`dev.rdesk.rdesk.AudioBridge`).
#[cfg(target_os = "android")]
mod jni_exports {
    use jni::JNIEnv;
    use jni::objects::{JByteArray, JClass};
    use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint};

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_AudioBridge_available(_: JNIEnv, _: JClass, on: jboolean) {
        super::set_available(on != 0);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_AudioBridge_wanted(_: JNIEnv, _: JClass) -> jboolean {
        if super::wanted() { JNI_TRUE } else { JNI_FALSE }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_AudioBridge_packet(
        env: JNIEnv,
        _: JClass,
        data: JByteArray,
        len: jint,
    ) {
        if let Ok(mut bytes) = env.convert_byte_array(&data) {
            bytes.truncate(len.max(0) as usize);
            super::packet(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_flow_only_while_someone_listens() {
        assert!(start().is_err(), "not available yet");
        set_available(true);
        assert!(!wanted());
        let mut rx = start().unwrap();
        assert!(wanted());
        packet(vec![1, 2, 3]);
        assert_eq!(rx.try_recv().unwrap(), [1, 2, 3]);
        drop(rx);
        assert!(!wanted());
        packet(vec![4]);
        set_available(false);
        assert!(start().is_err());
    }
}
