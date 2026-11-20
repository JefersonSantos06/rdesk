//! The computer the app runs on, as the server sees it.
//!
//! The app proves it runs on this computer with a presence proof signed by the host key
//! (`rdesk_proto::api::HereProof`): whoever is signed in here then sees it in the list, with
//! its owner and who it is shared with, even when someone else registered it. Managing it
//! that way (sharing, renaming, removing it from the server) first asks the system for an
//! administrator: on Windows the app runs as administrator ("Manage as administrator"),
//! on macOS and Linux the system asks for an administrator's password. That check is the
//! app's own, except when the Windows service runs the host: then its key is readable by
//! administrators only, and the service gives a "manage" proof to administrators only.

use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rdesk_common::account::{Api, Session};
use rdesk_common::t;
use rdesk_proto::api::{Here, Machine};

/// How long an administrator's confirmation counts.
const ADMIN_FOR: Duration = Duration::from_secs(10 * 60);

static ADMIN_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// `s.api()`, with a presence proof of `scope` when given. Without a proof the server
/// treats the request as coming from anywhere: fine to look, but managing must fail loudly.
pub(super) async fn api(s: &Session, scope: Option<Here>) -> anyhow::Result<Api> {
    let api = s.api()?;
    let Some(scope) = scope else { return Ok(api) };
    match proof(s.user.id, scope).await {
        Ok(p) => Ok(api.with_here(p)),
        Err(e) if scope == Here::Manage => Err(e),
        Err(_) => Ok(api),
    }
}

#[cfg(feature = "host")]
async fn proof(user_id: i64, scope: Here) -> anyhow::Result<String> {
    rdesk_host::admin::here_proof(user_id, scope).await
}

#[cfg(not(feature = "host"))]
async fn proof(_: i64, _: Here) -> anyhow::Result<String> {
    anyhow::bail!("this build has no host")
}

/// The proof the server needs to manage `m` from here: only when this user does not
/// manage it anyway (owner, managers) and is at the computer.
pub(super) fn manage_scope(m: &Machine) -> Option<Here> {
    (m.here.is_some() && !m.can_manage).then_some(Here::Manage)
}

/// Likewise for unregistering (owners only, or whoever manages it at the computer).
pub(super) fn remove_scope(m: &Machine) -> Option<Here> {
    (m.here.is_some() && !m.is_owner).then_some(Here::Manage)
}

/// The proof to look at `m` (who it is shared with) from here.
pub(super) fn view_scope(m: &Machine) -> Option<Here> {
    m.here.map(|_| Here::View)
}

pub(super) fn admin_confirmed() -> bool {
    ADMIN_UNTIL.lock().unwrap().is_some_and(|t| t > Instant::now())
}

fn remember_admin() {
    *ADMIN_UNTIL.lock().unwrap() = Some(Instant::now() + ADMIN_FOR);
}

/// What the system said.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AdminAnswer {
    Confirmed,
    /// Windows: this app must run as administrator (it can restart itself so).
    #[cfg_attr(not(windows), allow(dead_code))]
    NeedsRestart,
    // Windows does not refuse: the app restarts as administrator instead.
    #[cfg_attr(windows, allow(dead_code))]
    Refused(String),
}

/// Ask the system for an administrator. Blocks (a password prompt): run it off the UI thread.
fn confirm_admin() -> AdminAnswer {
    if admin_confirmed() {
        return AdminAnswer::Confirmed;
    }
    let a = imp::confirm();
    if a == AdminAnswer::Confirmed {
        remember_admin();
    }
    a
}

#[cfg(windows)]
mod imp {
    use super::AdminAnswer;

    pub fn confirm() -> AdminAnswer {
        #[cfg(feature = "host")]
        if rdesk_host::elevation::is_elevated() {
            return AdminAnswer::Confirmed;
        }
        AdminAnswer::NeedsRestart
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use rdesk_common::tf;

    pub fn confirm() -> AdminAnswer {
        let prompt = t!(
            "rdesk needs an administrator to manage this computer on the server.",
            "O rdesk precisa de um administrador para gerenciar este computador no servidor."
        )
        .replace('"', "'");
        let script =
            format!("do shell script \"/usr/bin/true\" with prompt \"{prompt}\" with administrator privileges");
        match std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).output() {
            Ok(o) if o.status.success() => AdminAnswer::Confirmed,
            // -128: the person cancelled.
            Ok(o) if String::from_utf8_lossy(&o.stderr).contains("-128") => {
                AdminAnswer::Refused(t!("Cancelled.", "Cancelado.").into())
            }
            Ok(o) => AdminAnswer::Refused(tf!(
                "The administrator's password was not accepted: {}",
                "A senha do administrador não foi aceita: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => AdminAnswer::Refused(format!("osascript: {e}")),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;

    pub fn confirm() -> AdminAnswer {
        match std::process::Command::new("pkexec").arg("/bin/true").status() {
            Ok(s) if s.success() => AdminAnswer::Confirmed,
            // 126: the dialog was dismissed; 127: not authorized.
            Ok(_) => AdminAnswer::Refused(
                t!("An administrator did not confirm it.", "Um administrador não confirmou.").into(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AdminAnswer::Refused(
                t!(
                    "Managing this computer here asks for an administrator through pkexec (polkit), which is \
                     not installed.",
                    "Gerenciar este computador aqui pede um administrador pelo pkexec (polkit), que não está \
                     instalado."
                )
                .into(),
            ),
            Err(e) => AdminAnswer::Refused(format!("pkexec: {e}")),
        }
    }
}

/// An action that waits for an administrator's confirmation.
pub(super) struct AdminGate<A> {
    pending: Option<A>,
    job: Option<Receiver<AdminAnswer>>,
}

/// What [`AdminGate::poll`] has.
pub(super) enum GateEvent<A> {
    Nothing,
    /// Go ahead.
    Go(A),
    /// Windows: offer to restart the app as administrator.
    NeedsRestart,
    Refused(String),
}

impl<A> AdminGate<A> {
    pub(super) fn new() -> Self {
        Self { pending: None, job: None }
    }

    /// `a` at once if no confirmation is needed (`needed` false, or one given recently);
    /// otherwise the system is asked and [`AdminGate::poll`] hands it back.
    pub(super) fn request(&mut self, ctx: &egui::Context, needed: bool, a: A) -> Option<A> {
        if !needed || admin_confirmed() {
            return Some(a);
        }
        self.pending = Some(a);
        if self.job.is_none() {
            self.job = Some(super::account::spawn(ctx, || async { confirm_admin() }));
        }
        None
    }

    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn poll(&mut self) -> GateEvent<A> {
        let Some(rx) = &self.job else { return GateEvent::Nothing };
        let Ok(answer) = rx.try_recv() else { return GateEvent::Nothing };
        self.job = None;
        let pending = self.pending.take();
        match (answer, pending) {
            (AdminAnswer::Confirmed, Some(a)) => GateEvent::Go(a),
            (AdminAnswer::Confirmed, None) => GateEvent::Nothing,
            (AdminAnswer::NeedsRestart, _) => GateEvent::NeedsRestart,
            (AdminAnswer::Refused(e), _) => GateEvent::Refused(e),
        }
    }
}

/// The question when Windows needs this app to run as administrator.
pub(super) fn restart_question() -> (&'static str, &'static str) {
    (
        t!("Manage as administrator?", "Gerenciar como administrador?"),
        t!(
            "Managing this computer here, when someone else registered it, needs administrator rights. rdesk \
             opens again as administrator (Windows asks first).",
            "Gerenciar este computador aqui, quando outra pessoa o cadastrou, exige direitos de administrador. \
             O rdesk abre de novo como administrador (o Windows pergunta antes)."
        ),
    )
}
