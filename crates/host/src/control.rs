//! Running the host inside another program (the desktop app): start/stop, live
//! status, recent events and "disconnect the current session".

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::sync::mpsc;

use crate::server::{self, RunOpts};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum HostState {
    #[default]
    Stopped,
    Starting,
    /// Running; the local UDP sockets for direct connections.
    Listening(Vec<SocketAddr>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub client: String,
    /// Where it came from: an IP for direct connections, "relay (…)" otherwise.
    pub peer: String,
    /// "key SHA256:…", "password" or "resume".
    pub method: String,
    pub since: SystemTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostEvent {
    pub at: SystemTime,
    pub text: String,
    pub warning: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostStatus {
    pub state: HostState,
    /// The host ID.
    pub fingerprint: Option<[u8; 32]>,
    /// `None`: no relay configured (or not known yet); `Ok(url)`: registered; `Err`: unreachable.
    pub relay: Option<Result<String, String>>,
    pub session: Option<SessionInfo>,
    /// Newest last; at most [`MAX_EVENTS`].
    pub events: VecDeque<HostEvent>,
}

pub const MAX_EVENTS: usize = 100;

/// Where the server writes its status; cheap to clone.
#[derive(Clone)]
pub struct Reporter {
    cell: Arc<Mutex<HostStatus>>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Reporter {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self { cell: Arc::new(Mutex::new(HostStatus::default())), notify: Arc::new(notify) }
    }

    /// A reporter nobody watches (command-line `host run`).
    pub fn silent() -> Self {
        Self::new(|| {})
    }

    pub fn snapshot(&self) -> HostStatus {
        self.cell.lock().unwrap().clone()
    }

    fn update(&self, f: impl FnOnce(&mut HostStatus)) {
        f(&mut self.cell.lock().unwrap());
        (self.notify)();
    }

    pub fn state(&self, s: HostState) {
        self.update(|st| st.state = s);
    }

    pub fn fingerprint(&self, fp: [u8; 32]) {
        self.update(|st| st.fingerprint = Some(fp));
    }

    pub fn relay(&self, r: Option<Result<String, String>>) {
        self.update(|st| st.relay = r);
    }

    pub fn session(&self, s: Option<SessionInfo>) {
        self.update(|st| st.session = s);
    }

    /// Clear the session only if it is still `peer`'s (a newer one may have replaced it).
    pub fn session_ended(&self, peer: &str, since: SystemTime) {
        self.update(|st| {
            if st.session.as_ref().is_some_and(|s| s.peer == peer && s.since == since) {
                st.session = None;
            }
        });
    }

    pub fn event(&self, text: impl Into<String>, warning: bool) {
        let text = text.into();
        if warning {
            tracing::warn!("{text}");
        } else {
            tracing::info!("{text}");
        }
        self.update(|st| {
            st.events.push_back(HostEvent { at: SystemTime::now(), text, warning });
            while st.events.len() > MAX_EVENTS {
                st.events.pop_front();
            }
        });
    }
}

pub enum Ctl {
    Stop,
    /// End the current session; the client is told not to reconnect.
    Disconnect,
}

/// A host running on a tokio runtime, controlled from a UI.
pub struct HostHandle {
    reporter: Reporter,
    ctl: mpsc::UnboundedSender<Ctl>,
    task: tokio::task::JoinHandle<()>,
}

impl HostHandle {
    pub fn start(opts: RunOpts, rt: &tokio::runtime::Handle, reporter: Reporter) -> Self {
        let (ctl, rx) = mpsc::unbounded_channel();
        let r = reporter.clone();
        reporter.state(HostState::Starting);
        let task = rt.spawn(async move {
            match server::serve(opts, r.clone(), rx).await {
                Ok(()) => r.state(HostState::Stopped),
                Err(e) => {
                    r.event(format!("Host stopped: {e:#}"), true);
                    r.state(HostState::Failed(format!("{e:#}")));
                }
            }
            r.session(None);
            r.relay(None);
        });
        Self { reporter, ctl, task }
    }

    pub fn status(&self) -> HostStatus {
        self.reporter.snapshot()
    }

    pub fn stop(&self) {
        let _ = self.ctl.send(Ctl::Stop);
    }

    pub fn disconnect(&self) {
        let _ = self.ctl.send(Ctl::Disconnect);
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Stop and wait (bounded) until the sockets are closed, e.g. before restarting.
    pub async fn shutdown(self) {
        self.stop();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
    }
}
