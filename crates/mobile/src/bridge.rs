//! What the host on the phone asks of the app while the screen is shared: the app's Kotlin
//! (`ScreenCaptureService`, through `HostBridge`) takes these one by one and answers.
//!
//! Requests are text: a kind, a newline, then its payload:
//! - `clip` + text: put it on the phone's clipboard (from the other side);
//! - `ask` + id, newline, question: ask the person at the phone whether a connection may start
//!   (a notification with Allow and Deny); the answer comes back with [`answer`];
//! - `unask` + id: nobody answered in time, take the question away.

// Elsewhere nothing reads it (only the JNI calls below do), but it builds and is tested.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use rdesk_host::consent::Answer;

const QUEUE_MAX: usize = 64;

static QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static READY: Condvar = Condvar::new();

/// Answers to `ask`, by id.
static ANSWERS: Mutex<Option<HashMap<u32, bool>>> = Mutex::new(None);
static ANSWERED: Condvar = Condvar::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn push(cmd: String) {
    let mut q = QUEUE.lock().unwrap();
    if q.len() >= QUEUE_MAX {
        q.pop_front();
    }
    q.push_back(cmd);
    READY.notify_all();
}

/// The next request, waiting up to `timeout`.
pub fn next(timeout: Duration) -> Option<String> {
    let mut q = QUEUE.lock().unwrap();
    if q.is_empty() {
        q = READY.wait_timeout(q, timeout).unwrap().0;
    }
    q.pop_front()
}

/// The host's clipboard setter on the phone.
pub fn set_clipboard(text: String) {
    if crate::screen::sharing() {
        push(format!("clip\n{text}"));
    }
}

/// The host's consent asker on the phone: a notification, answered or not within `timeout`.
pub fn ask(question: &str, timeout: Duration) -> Answer {
    // Only the sharing service shows it; without it nobody could answer.
    if !crate::screen::sharing() {
        return Answer::NoAnswer;
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // The host's question speaks of a computer; this is a phone.
    let question = question.replace("this computer", "this phone").replace("este computador", "este celular");
    push(format!("ask\n{id}\n{question}"));
    let deadline = Instant::now() + timeout;
    let mut answers = ANSWERS.lock().unwrap();
    loop {
        if let Some(allowed) = answers.get_or_insert_default().remove(&id) {
            return if allowed { Answer::Allowed } else { Answer::Denied };
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            drop(answers);
            push(format!("unask\n{id}"));
            return Answer::NoAnswer;
        }
        answers = ANSWERED.wait_timeout(answers, left).unwrap().0;
    }
}

/// The person answered question `id`.
pub fn answer(id: u32, allowed: bool) {
    ANSWERS.lock().unwrap().get_or_insert_default().insert(id, allowed);
    ANSWERED.notify_all();
}

/// What the app's Kotlin calls (`dev.rdesk.rdesk.HostBridge`).
#[cfg(target_os = "android")]
mod jni_exports {
    use std::time::Duration;

    use jni::JNIEnv;
    use jni::objects::{JClass, JString};
    use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jstring};

    /// Start the host without the app's screen (the phone started, or was unlocked, with
    /// "start on its own" on; the screen is already shared). `true` if it started.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_HostBridge_startHost(
        mut env: JNIEnv,
        _: JClass,
        data_dir: JString,
        device_name: JString,
        portuguese: jboolean,
    ) -> jboolean {
        let (Ok(dir), Ok(name)) = (env.get_string(&data_dir), env.get_string(&device_name)) else {
            return JNI_FALSE;
        };
        let (dir, name): (String, String) = (dir.into(), name.into());
        let r = crate::api::app::start(dir, name, portuguese != 0).and_then(|()| crate::api::host::start_host());
        match r {
            Ok(()) => JNI_TRUE,
            Err(e) => {
                tracing::warn!("starting the host on its own: {e:#}");
                JNI_FALSE
            }
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_HostBridge_next(env: JNIEnv, _: JClass, timeout_ms: jint) -> jstring {
        match super::next(Duration::from_millis(timeout_ms.max(0) as u64)) {
            Some(cmd) => env.new_string(cmd).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut()),
            None => std::ptr::null_mut(),
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_rdesk_rdesk_HostBridge_answer(_: JNIEnv, _: JClass, id: jint, allowed: jboolean) {
        super::answer(id.max(0) as u32, allowed != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test: the queue and the screen state are global.
    #[test]
    fn questions_are_answered_or_taken_back() {
        let _turn = crate::screen::TEST_TURN.lock().unwrap_or_else(|e| e.into_inner());
        // Not sharing: nobody to ask, nothing queued.
        assert_eq!(ask("Ana?", Duration::from_millis(10)), Answer::NoAnswer);
        assert!(next(Duration::ZERO).is_none());

        crate::screen::started(16, 16);
        set_clipboard("hello\nworld".into());
        assert_eq!(next(Duration::ZERO).as_deref(), Some("clip\nhello\nworld"));

        // Answered from "the phone" while the host waits.
        let waiter = std::thread::spawn(|| ask("Ana wants to connect", Duration::from_secs(5)));
        let req = next(Duration::from_secs(5)).unwrap();
        let mut lines = req.splitn(3, '\n');
        assert_eq!(lines.next(), Some("ask"));
        let id: u32 = lines.next().unwrap().parse().unwrap();
        assert_eq!(lines.next(), Some("Ana wants to connect"));
        answer(id, false);
        assert_eq!(waiter.join().unwrap(), Answer::Denied);

        // Nobody answers: taken back.
        assert_eq!(ask("Bia?", Duration::from_millis(20)), Answer::NoAnswer);
        assert!(next(Duration::ZERO).unwrap().starts_with("ask\n"));
        assert!(next(Duration::ZERO).unwrap().starts_with("unask\n"));
        crate::screen::stopped();
    }
}
