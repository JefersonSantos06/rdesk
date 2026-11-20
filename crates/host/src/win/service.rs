//! The `rdesk` Windows service. It runs as SYSTEM and does one thing: keep the host
//! (`rdesk host run --agent`) running in the session at the physical screen, as SYSTEM, so
//! the computer is reachable before anyone signs in, on the lock screen and through UAC
//! prompts. It follows that session when people sign out or switch users.
//!
//! Installing copies the program to `C:\Program Files\rdesk` (only administrators may
//! change it, since it runs as SYSTEM) and moves the host's files (identity, registration,
//! settings) to `C:\ProgramData\rdesk`, readable only by SYSTEM and Administrators.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rdesk_common::config;
use rdesk_common::{t, tf};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use super::proc::{self, Child, Env};

pub const NAME: &str = "rdesk";
const DISPLAY_NAME: &str = "rdesk host";
const DESCRIPTION: &str =
    "Lets people you allowed on your rdesk server reach this computer, also before sign-in and on the lock screen.";

/// `C:\Program Files\rdesk`.
pub fn program_dir() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("rdesk")
}

pub fn program_exe() -> PathBuf {
    program_dir().join("rdesk.exe")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    NotInstalled,
    Stopped,
    Running,
    /// Starting or stopping.
    Changing,
}

pub fn state() -> Result<State> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => return Ok(State::NotInstalled),
        Err(e) => return Err(e.into()),
    };
    Ok(match service.query_status()?.current_state {
        ServiceState::Running => State::Running,
        ServiceState::Stopped => State::Stopped,
        _ => State::Changing,
    })
}

fn require_admin() -> Result<()> {
    if !crate::elevation::is_elevated() {
        bail!(t!(
            "this needs administrator rights: run it from an elevated terminal (or the app's button)",
            "isto precisa de direitos de administrador: rode a partir de um terminal elevado (ou pelo botão do app)"
        ));
    }
    Ok(())
}

fn wait_for(service: &windows_service::service::Service, want: ServiceState, max: Duration) -> Result<()> {
    let start = Instant::now();
    while service.query_status()?.current_state != want {
        if start.elapsed() > max {
            bail!("the service did not reach {want:?} in {} s", max.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// The host files that `install` moves to the machine folder and `uninstall` moves back.
fn move_host_files(from: &Path, to: &Path) -> Result<Vec<&'static str>> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let mut moved = Vec::new();
    for name in config::HOST_FILES {
        let (src, dst) = (from.join(name), to.join(name));
        if !src.is_file() || dst.exists() {
            continue;
        }
        std::fs::copy(&src, &dst).with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        std::fs::remove_file(&src).with_context(|| format!("removing {}", src.display()))?;
        moved.push(name);
    }
    Ok(moved)
}

/// Create `%ProgramData%\rdesk` readable only by SYSTEM and Administrators.
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Protected (no inherited entries): SYSTEM and Administrators, full control.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)".encode_utf16().chain(Some(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            bail!("building the folder's permissions: {}", std::io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let path: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        if CreateDirectoryW(path.as_ptr(), &sa) == 0 {
            bail!("creating {}: {}", dir.display(), std::io::Error::last_os_error());
        }
    }
    Ok(())
}

const FIREWALL_RULE: &str = "rdesk host";

fn netsh(args: &str) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("netsh").raw_arg(args).creation_flags(0x0800_0000).output()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        bail!("netsh {args}: {text}");
    }
    Ok(text)
}

/// Let the host be reached on its UDP port, unasked, only from the local network and
/// Tailscale. Connections it opens itself (hole punching with the relay's help) are
/// answered by Windows as replies, from any address.
fn firewall_allow(exe: &Path) -> Result<()> {
    firewall_remove();
    netsh(&format!(
        "advfirewall firewall add rule name=\"{FIREWALL_RULE}\" dir=in action=allow protocol=UDP \
         program=\"{}\" remoteip=localsubnet,100.64.0.0/10,fd7a:115c:a1e0::/48 profile=any enable=yes",
        exe.display()
    ))?;
    Ok(())
}

fn firewall_remove() {
    let _ = netsh(&format!("advfirewall firewall delete rule name=\"{FIREWALL_RULE}\""));
}

/// Install (or update) and start the service. Run elevated, by the person whose host
/// settings should move to the service.
pub fn install() -> Result<String> {
    require_admin()?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_CONFIG;

    // An update: stop it first, so the program file is free.
    if let Ok(existing) = manager.open_service(NAME, access)
        && existing.query_status()?.current_state != ServiceState::Stopped
    {
        let _ = existing.stop();
        wait_for(&existing, ServiceState::Stopped, Duration::from_secs(20))?;
    }

    // The program, where only administrators can change it.
    let exe = program_exe();
    std::fs::create_dir_all(program_dir())?;
    let me = std::env::current_exe()?;
    if !same_file(&me, &exe) {
        std::fs::copy(&me, &exe).with_context(|| format!("copying the program to {}", exe.display()))?;
    }

    // The host's files.
    let machine = config::service_dir().context("%ProgramData% is not set")?;
    create_private_dir(&machine)?;
    let moved = move_host_files(&config::config_dir()?, &machine)?;

    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = match manager.open_service(NAME, access) {
        Ok(s) => {
            s.change_config(&info)?;
            s
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    service.set_description(DESCRIPTION)?;
    let restart = ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) };
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart.clone(), restart.clone(), restart]),
    })?;
    let firewall = firewall_allow(&exe);
    start_menu_shortcut(&exe);
    service.start::<&str>(&[])?;
    wait_for(&service, ServiceState::Running, Duration::from_secs(20))?;

    let mut msg = tf!(
        "The rdesk service is installed and running ({}).",
        "O serviço do rdesk está instalado e rodando ({}).",
        exe.display()
    );
    match firewall {
        Ok(()) => msg.push_str(t!(
            " Firewall: reachable from the local network and Tailscale.",
            " Firewall: acessível pela rede local e pelo Tailscale."
        )),
        Err(e) => msg.push_str(&tf!(
            " Firewall rule not created ({e:#}): other computers may not reach it.",
            " Regra de firewall não criada ({e:#}): outros computadores podem não conseguir acessá-lo."
        )),
    }
    if !moved.is_empty() {
        msg.push_str(&tf!(
            " Moved this computer's {0} to {1}.",
            " Arquivos deste computador movidos para {1}: {0}.",
            moved.join(", "),
            machine.display()
        ));
    }
    Ok(msg)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Stop and remove the service; the host's files go back to this user's folder.
pub fn uninstall() -> Result<String> {
    require_admin()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    match manager.open_service(NAME, access) {
        Ok(service) => {
            if service.query_status()?.current_state != ServiceState::Stopped {
                let _ = service.stop();
                wait_for(&service, ServiceState::Stopped, Duration::from_secs(20))?;
            }
            service.delete()?;
        }
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => {}
        Err(e) => return Err(e.into()),
    }
    firewall_remove();
    super::sas::revert();
    let mut msg = t!("The rdesk service is removed.", "O serviço do rdesk foi removido.").to_string();
    if let Some(machine) = config::service_dir().filter(|d| d.is_dir()) {
        let back = config::config_dir()?;
        let moved = move_host_files(&machine, &back)?;
        if !moved.is_empty() {
            msg.push_str(&tf!(
                " Moved this computer's {0} back to {1}.",
                " Arquivos deste computador devolvidos para {1}: {0}.",
                moved.join(", "),
                back.display()
            ));
        }
        std::fs::remove_dir_all(&machine).with_context(|| format!("removing {}", machine.display()))?;
    }
    let exe = program_exe();
    if exe.exists() && !same_file(&std::env::current_exe()?, &exe) {
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_dir(program_dir());
    }
    Ok(msg)
}

// ---- running as the service ------------------------------------------------------

define_windows_service!(ffi_service_main, service_main);

/// Entry point when Windows starts the service (`rdesk service run`).
pub fn run() -> Result<()> {
    service_dispatcher::start(NAME, ffi_service_main)
        .context("not started by Windows (use `rdesk service install`)")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service: {e:#}");
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn run_service() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handler = move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let handle = service_control_handler::register(NAME, handler)?;
    handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ))?;
    tracing::info!("service started");
    if let Err(e) = super::sas::allow() {
        tracing::warn!("Ctrl+Alt+Del will not work: {e:#}");
    }
    supervise(&stop_rx);
    tracing::info!("service stopped");
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}

struct Agent {
    child: Child,
    /// Closing it asks the host to stop.
    stdin: Option<std::fs::File>,
    session: u32,
    started: Instant,
}

impl Agent {
    fn stop(mut self) {
        drop(self.stdin.take());
        if self.child.wait_timeout(Duration::from_secs(8)).is_none() {
            tracing::warn!("the host did not stop in time; ending it");
            self.child.kill();
        }
    }
}

fn launch(session: u32) -> Result<Agent> {
    let token = proc::system_token_in(session)?;
    let exe = std::env::current_exe()?;
    let machine = config::service_dir().context("%ProgramData% is not set")?;
    let env = [("RDESK_HOST_DIR", machine.into_os_string())];
    let mut sp = proc::spawn(&token, &exe, &["host", "run", "--agent"], Env::Inherit(&env), true, false)?;
    tracing::info!("started the host in session {session} (pid {})", sp.child.pid);
    Ok(Agent { child: sp.child, stdin: sp.stdin.take(), session, started: Instant::now() })
}

/// Keep one host running in the console session until told to stop.
fn supervise(stop: &mpsc::Receiver<()>) {
    let mut agent: Option<Agent> = None;
    let mut not_before = Instant::now();
    let mut backoff = Duration::ZERO;
    // Once a second, until told to stop (or the control handler is gone).
    while let Err(mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(Duration::from_secs(1)) {
        let console = proc::console_session();
        if let Some(a) = &agent {
            if let Some(code) = a.child.try_wait() {
                // Exit code 0: a requested restart (new settings). Otherwise it failed: wait
                // longer each time it fails right after starting.
                if code != 0 && a.started.elapsed() < Duration::from_secs(30) {
                    backoff = (backoff * 2).clamp(Duration::from_secs(2), Duration::from_secs(60));
                } else {
                    backoff = Duration::ZERO;
                }
                tracing::info!("the host exited (code {code}); starting it again in {} s", backoff.as_secs());
                not_before = Instant::now() + backoff;
                agent = None;
            } else if Some(a.session) != console {
                tracing::info!("the screen moved to session {console:?}; moving the host");
                if let Some(a) = agent.take() {
                    a.stop();
                }
            }
        }
        if agent.is_none()
            && let Some(session) = console
            && Instant::now() >= not_before
        {
            match launch(session) {
                Ok(a) => agent = Some(a),
                Err(e) => {
                    tracing::warn!("starting the host in session {session}: {e:#}");
                    not_before = Instant::now() + Duration::from_secs(5);
                }
            }
        }
    }
    if let Some(a) = agent {
        a.stop();
    }
}

// ---- updates --------------------------------------------------------------------------

/// The previous program, kept for going back (`C:\Program Files\rdesk\rdesk.old.exe`).
fn previous_exe() -> PathBuf {
    program_dir().join("rdesk.old.exe")
}

/// Is the host the service runs up and, if registered, in touch with its relay?
async fn healthy() -> bool {
    use crate::control::HostState;
    use crate::ipc::{Request, Response, request};
    match tokio::time::timeout(Duration::from_secs(3), request(&Request::Status)).await {
        Ok(Ok(Response::Status(s))) => {
            matches!(s.state, HostState::Listening(_)) && (s.registration.is_none() || matches!(s.relay, Some(Ok(_))))
        }
        _ => false,
    }
}

async fn wait_healthy(max: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < max {
        if healthy().await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

/// Restart the service (after changing the host's settings as an administrator).
pub fn restart() -> Result<()> {
    start_stop(false)?;
    start_stop(true)
}

fn start_stop(start: bool) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service =
        manager.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP)?;
    if start {
        service.start::<&str>(&[])?;
        wait_for(&service, ServiceState::Running, Duration::from_secs(30))
    } else {
        if service.query_status()?.current_state != ServiceState::Stopped {
            let _ = service.stop();
        }
        wait_for(&service, ServiceState::Stopped, Duration::from_secs(30))
    }
}

/// `rdesk service apply-update --version V`, run from the downloaded program in
/// `%ProgramData%\rdesk\updates\V` by the service's host (as SYSTEM). Swaps the program and
/// goes back to the previous one if the new host does not come up within two minutes.
pub fn apply_update(version: &str) -> Result<()> {
    require_admin()?;
    let me = std::env::current_exe()?;
    let staging = super::update::staging_dir(version)?;
    ensure!(
        same_file(&me, &staging.join("rdesk.exe")),
        "run from the downloaded update ({}), not from {}",
        staging.display(),
        me.display()
    );
    let (target, old) = (program_exe(), previous_exe());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    tracing::info!("installing rdesk {version}");

    start_stop(false)?;
    // A running copy (the app in the tray) can be renamed, not overwritten.
    if old.exists() && std::fs::remove_file(&old).is_err() {
        let aside = program_dir().join(format!("rdesk.old-{}.exe", std::process::id()));
        let _ = std::fs::rename(&old, aside);
    }
    std::fs::rename(&target, &old).context("setting the current program aside")?;
    if let Err(e) = std::fs::copy(&me, &target) {
        let _ = std::fs::rename(&old, &target);
        let _ = start_stop(true);
        return Err(anyhow::Error::from(e).context("copying the new program"));
    }
    start_stop(true)?;
    if rt.block_on(wait_healthy(Duration::from_secs(120))) {
        std::fs::write(staging.join(super::update::INSTALLED), b"")?;
        tracing::info!("rdesk {version} is running");
        // Earlier downloads are not needed any more.
        if let Some(all) = staging.parent()
            && let Ok(dirs) = std::fs::read_dir(all)
        {
            for d in dirs.flatten().filter(|d| d.path() != staging) {
                let _ = std::fs::remove_dir_all(d.path());
            }
        }
        return Ok(());
    }

    tracing::warn!("rdesk {version} did not come up: going back to the previous program");
    std::fs::write(staging.join(super::update::FAILED), b"")?;
    start_stop(false)?;
    std::fs::remove_file(&target).context("removing the new program")?;
    std::fs::rename(&old, &target).context("putting the previous program back")?;
    start_stop(true)?;
    bail!("rdesk {version} did not start correctly; the previous version is back")
}

/// A Start menu entry for everyone, to the program the service keeps up to date.
fn start_menu_shortcut(exe: &Path) {
    let Some(data) = std::env::var_os("ProgramData") else { return };
    let lnk = PathBuf::from(data).join(r"Microsoft\Windows\Start Menu\Programs\rdesk.lnk");
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath = '{}'; \
         $s.WorkingDirectory = '{}'; $s.Description = 'rdesk'; $s.Save()",
        lnk.display(),
        exe.display(),
        program_dir().display()
    );
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        tracing::warn!("could not create the Start menu entry");
    }
}
