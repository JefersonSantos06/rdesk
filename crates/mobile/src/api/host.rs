//! This phone as a host: other devices connect to it. On Android it shares the screen (the
//! app's MediaProjection, see [`crate::screen`]); touches from the other side are only
//! logged for now. Elsewhere (and without a shared screen) it sends a test picture.

use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow};
use flutter_rust_bridge::frb;
use rdesk_common::account::{Registration, Session};
use rdesk_common::config::format_host_id;
use rdesk_host::{HostHandle, HostState, Reporter, RunOpts, admin};

use crate::frb_generated::StreamSink;
use crate::runtime::{rt, run};

/// What the host is doing, for the "This phone" page.
#[derive(Debug, Clone, Default)]
pub struct HostView {
    pub running: bool,
    /// "stopped", "starting", "listening" or "failed".
    pub state: String,
    /// Why it stopped, when `state` is "failed".
    pub error: Option<String>,
    /// This phone's host ID (as people type it), once known.
    pub host_id: Option<String>,
    /// "name on server" when registered.
    pub registration: Option<String>,
    /// The server refused this phone, or could not be reached.
    pub registration_error: Option<String>,
    /// Who is connected now.
    pub session_client: Option<String>,
    /// The phone's screen is being shared (Android); otherwise the test picture goes out.
    pub sharing_screen: bool,
    pub has_password: bool,
    /// Newest last.
    pub events: Vec<String>,
}

struct Host {
    reporter: Reporter,
    handle: Mutex<Option<HostHandle>>,
    watcher: Mutex<Option<StreamSink<HostView>>>,
}

fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| Host {
        reporter: Reporter::new(|| {
            let h = host();
            if let Some(sink) = h.watcher.lock().unwrap().as_ref() {
                let _ = sink.add(view(h));
            }
        }),
        handle: Mutex::new(None),
        watcher: Mutex::new(None),
    })
}

fn view(h: &Host) -> HostView {
    let st = h.reporter.snapshot();
    let running = h.handle.lock().unwrap().as_ref().is_some_and(|x| !x.is_finished());
    let (state, error) = match &st.state {
        HostState::Stopped => ("stopped", None),
        HostState::Starting => ("starting", None),
        HostState::Listening(_) => ("listening", None),
        HostState::Failed(e) => ("failed", Some(e.clone())),
    };
    let host_id = st.fingerprint.or_else(|| admin::fingerprint().ok()).map(|fp| format_host_id(&fp));
    let (registration, registration_error) = match st.registration {
        Some(Ok(r)) => (Some(r), None),
        Some(Err(e)) => (None, Some(e)),
        None => (Registration::load().map(|r| format!("{} · {}", r.name, r.server_name)), None),
    };
    HostView {
        running,
        state: state.into(),
        error,
        host_id,
        registration,
        registration_error,
        session_client: st.session.map(|s| s.client),
        sharing_screen: crate::screen::sharing(),
        has_password: admin::has_password(),
        events: st.events.into_iter().map(|e| e.text).collect(),
    }
}

/// The host's status now.
#[frb(sync)]
pub fn host_status() -> HostView {
    view(host())
}

/// Send the status to `sink` now and whenever it changes (one watcher: a new call replaces it).
pub fn watch_host(sink: StreamSink<HostView>) -> Result<()> {
    let h = host();
    sink.add(view(h)).map_err(|e| anyhow!("{e:?}"))?;
    *h.watcher.lock().unwrap() = Some(sink);
    Ok(())
}

/// Start accepting connections (nothing happens if it is running already). On Android, share
/// the screen first (the app's `ScreenShare.start`): the host sends what it shows.
pub fn start_host() -> Result<()> {
    let h = host();
    admin::ensure_initialized()?;
    if h.handle.lock().unwrap().as_ref().is_some_and(|x| !x.is_finished()) {
        return Ok(());
    }
    let screen = crate::screen::sharing();
    if screen {
        rdesk_host::capture::set_screen_source(|| Box::new(crate::screen::PhoneScreen));
    }
    // Touches are not injected yet: they are only logged.
    let opts = RunOpts { fake_capture: !screen, fake_input: true, ..Default::default() };
    // Started outside the lock: the reporter's first update reads the status (and the lock).
    let started = HostHandle::start(opts, rt().handle(), h.reporter.clone());
    *h.handle.lock().unwrap() = Some(started);
    h.reporter.event(
        if screen {
            rdesk_common::t!(
                "Sharing this phone's screen (view only: touches are not passed on yet)",
                "Compartilhando a tela deste celular (só visualização: os toques ainda não são repassados)"
            )
        } else {
            rdesk_common::t!("Test picture: the screen is not shared", "Imagem de teste: a tela não está compartilhada")
        },
        false,
    );
    Ok(())
}

/// The screen stopped being shared (from the app's Kotlin): the host stops with it, since
/// nobody could see the phone any more. `by_person`: from the notification's Stop button.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn screen_stopped(by_person: bool) {
    let h = host();
    if h.handle.lock().unwrap().as_ref().is_none_or(|x| x.is_finished()) {
        return;
    }
    h.reporter.event(
        if by_person {
            rdesk_common::t!("Screen sharing stopped from the notification", "Compartilhamento da tela parado pela notificação")
        } else {
            rdesk_common::t!(
                "Screen sharing ended (the screen locked, or it was stopped from the system): turn access on again",
                "O compartilhamento da tela acabou (a tela bloqueou, ou ele foi parado pelo sistema): ligue o acesso de novo"
            )
        },
        !by_person,
    );
    rt().spawn(stop_host());
}

/// Stop accepting connections and end the current session.
pub async fn stop_host() {
    let handle = host().handle.lock().unwrap().take();
    if let Some(handle) = handle {
        let _ = run(async move {
            handle.shutdown().await;
            Ok(())
        })
        .await;
    }
}

/// End the current session (the host keeps listening).
pub fn disconnect_host_session() {
    if let Some(handle) = host().handle.lock().unwrap().as_ref() {
        handle.disconnect();
    }
}

/// Set this phone's own password (for direct access without the server); takes about a second.
/// The host must be restarted to use it.
pub async fn set_host_password(password: String) -> Result<()> {
    run(async move { tokio::task::spawn_blocking(move || admin::set_password(&password)).await? }).await
}

/// A strong random password to offer.
#[frb(sync)]
pub fn generate_host_password() -> String {
    admin::generate_password(16)
}

pub fn clear_host_password() -> Result<()> {
    admin::clear_password()
}

/// Add this phone to the signed-in account's server under the phone's name, visible only to
/// its owner. The host must be restarted to use it.
pub async fn register_this_phone() -> Result<String> {
    let s = Session::load().ok_or_else(|| anyhow!(rdesk_common::t!("Not signed in.", "Você não entrou.")))?;
    let name = rdesk_common::local_hostname();
    let reg = run(async move { admin::register(&s, &name, rdesk_proto::api::Visibility::Private).await }).await?;
    Ok(format!("{} · {}", reg.name, reg.server_name))
}

/// Remove this phone from its server.
pub async fn unregister_this_phone() -> Result<()> {
    let s = Session::load().ok_or_else(|| anyhow!(rdesk_common::t!("Not signed in.", "Você não entrou.")))?;
    run(async move { admin::unregister(&s).await }).await
}
