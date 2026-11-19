//! "Start with the system", and restarting the app as administrator (Windows).
//!
//! * Windows: `HKCU\…\Run` for a normal start at login; a Task Scheduler task with
//!   "highest privileges" (and no 3-day time limit) to start as administrator.
//! * macOS: a LaunchAgent in `~/Library/LaunchAgents`.
//! * Linux: an XDG autostart entry in `~/.config/autostart`.

use std::path::Path;

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autostart {
    Off,
    Login,
    /// Windows only: at login, elevated, so admin windows can be controlled.
    LoginAdmin,
}

impl Autostart {
    pub fn arg(self) -> &'static str {
        match self {
            Autostart::Off => "off",
            Autostart::Login => "login",
            Autostart::LoginAdmin => "login-admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Autostart::Off),
            "login" => Some(Autostart::Login),
            "login-admin" => Some(Autostart::LoginAdmin),
            _ => None,
        }
    }
}

/// The app's own command line when started automatically.
const ARGS: [&str; 2] = ["ui", "--minimized"];

pub fn supports_admin() -> bool {
    cfg!(windows)
}

pub fn current() -> Autostart {
    imp::current()
}

/// Apply directly (the caller must already be elevated where that is needed).
pub fn apply(mode: Autostart, exe: &Path) -> Result<()> {
    imp::apply(mode, exe)
}

/// Apply, asking Windows for administrator rights (UAC) when the change needs them.
/// Blocks until done: run it off the UI thread.
pub fn apply_with_elevation(mode: Autostart, exe: &Path) -> Result<Autostart> {
    imp::apply_with_elevation(mode, exe)?;
    let now = current();
    if now != mode {
        bail!("the change did not take effect (now: {})", now.arg());
    }
    Ok(now)
}

/// Install (`true`) or remove the rdesk Windows service, asking for administrator
/// permission when needed.
#[cfg(all(windows, feature = "host"))]
pub fn service_action(exe: &Path, install: bool) -> Result<String> {
    imp::service_action(exe, install)
}

/// Start a new elevated copy of the app that takes over from this one.
#[cfg_attr(not(feature = "host"), allow(dead_code))]
pub fn restart_as_admin(exe: &Path) -> Result<()> {
    imp::restart_as_admin(exe)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::anyhow;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "rdesk";
    const TASK: &str = "rdesk";

    fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    fn command_line(exe: &Path) -> String {
        format!("\"{}\" {}", exe.display(), ARGS.join(" "))
    }

    fn run_key_exists() -> bool {
        let (k, v) = (wide(RUN_KEY), wide(VALUE));
        let mut size = 0u32;
        // SAFETY: querying the size only; all pointers are valid or null.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            ) == ERROR_SUCCESS
        }
    }

    fn set_run_key(exe: &Path) -> Result<()> {
        let (k, v, data) = (wide(RUN_KEY), wide(VALUE), wide(command_line(exe)));
        // SAFETY: `data` is a NUL-terminated UTF-16 string of the given byte length.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc != ERROR_SUCCESS {
            bail!("writing the Run registry key failed (error {rc})");
        }
        Ok(())
    }

    fn delete_run_key() {
        let (k, v) = (wide(RUN_KEY), wide(VALUE));
        // SAFETY: valid NUL-terminated strings; a missing value is fine.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) };
    }

    fn schtasks(args: &[&str]) -> Result<std::process::Output> {
        use std::os::windows::process::CommandExt as _;
        Ok(std::process::Command::new("schtasks").args(args).creation_flags(0x0800_0000).output()?)
    }

    fn task_exists() -> bool {
        schtasks(&["/Query", "/TN", TASK]).is_ok_and(|o| o.status.success())
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn create_task(exe: &Path) -> Result<()> {
        let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(d), Ok(u)) => format!("{d}\\{u}"),
            (_, Ok(u)) => u,
            _ => bail!("cannot tell the current user name"),
        };
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>rdesk: remote desktop host and launcher</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>{args}</Arguments></Exec></Actions>
</Task>
"#,
            user = xml_escape(&user),
            exe = xml_escape(&exe.display().to_string()),
            args = ARGS.join(" "),
        );
        // Task Scheduler wants UTF-16 with a BOM.
        let mut bytes = vec![0xFF, 0xFE];
        for u in xml.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let file = std::env::temp_dir().join(format!("rdesk-task-{}.xml", std::process::id()));
        std::fs::write(&file, bytes)?;
        let out = schtasks(&["/Create", "/TN", TASK, "/XML", &file.display().to_string(), "/F"]);
        let _ = std::fs::remove_file(&file);
        let out = out?;
        if !out.status.success() {
            bail!("schtasks: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn delete_task() -> Result<()> {
        if !task_exists() {
            return Ok(());
        }
        let out = schtasks(&["/Delete", "/TN", TASK, "/F"])?;
        if !out.status.success() {
            bail!("schtasks: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    pub fn current() -> Autostart {
        if task_exists() {
            Autostart::LoginAdmin
        } else if run_key_exists() {
            Autostart::Login
        } else {
            Autostart::Off
        }
    }

    pub fn apply(mode: Autostart, exe: &Path) -> Result<()> {
        match mode {
            Autostart::Off => {
                delete_run_key();
                delete_task()
            }
            Autostart::Login => {
                delete_task()?;
                set_run_key(exe)
            }
            Autostart::LoginAdmin => {
                delete_run_key();
                create_task(exe)
            }
        }
    }

    /// Run `exe args` elevated (UAC prompt); with `wait`, until it exits.
    fn run_elevated(exe: &Path, args: &str, show: bool, wait: bool) -> Result<u32> {
        let (verb, file, params) = (wide("runas"), wide(exe), wide(args));
        // SAFETY: all-zero is a valid SHELLEXECUTEINFOW; the strings outlive the call.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = if show { SW_SHOWNORMAL } else { SW_HIDE };
        // SAFETY: `info` is fully initialised above.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            let e = std::io::Error::last_os_error();
            // ERROR_CANCELLED: the user said no to the UAC prompt.
            if e.raw_os_error() == Some(1223) {
                bail!("administrator permission was not given");
            }
            return Err(anyhow!(e).context("starting as administrator"));
        }
        let mut code = 0u32;
        if !info.hProcess.is_null() {
            // SAFETY: hProcess is a valid process handle we own (SEE_MASK_NOCLOSEPROCESS).
            unsafe {
                if wait && WaitForSingleObject(info.hProcess, 120_000) == WAIT_OBJECT_0 {
                    GetExitCodeProcess(info.hProcess, &mut code);
                }
                CloseHandle(info.hProcess);
            }
        }
        Ok(code)
    }

    fn elevated() -> bool {
        #[cfg(feature = "host")]
        {
            rdesk_host::elevation::is_elevated()
        }
        #[cfg(not(feature = "host"))]
        {
            false
        }
    }

    pub fn apply_with_elevation(mode: Autostart, exe: &Path) -> Result<()> {
        let touches_task = mode == Autostart::LoginAdmin || task_exists();
        if !touches_task || elevated() {
            return apply(mode, exe);
        }
        let code = run_elevated(exe, &format!("autostart {}", mode.arg()), false, true)?;
        if code != 0 {
            bail!("the elevated helper failed (exit code {code})");
        }
        Ok(())
    }

    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub fn restart_as_admin(exe: &Path) -> Result<()> {
        run_elevated(exe, "ui --takeover", true, false).map(|_| ())
    }

    #[cfg(feature = "host")]
    pub fn service_action(exe: &Path, install: bool) -> Result<String> {
        use rdesk_host::win::service;
        if elevated() {
            let msg = if install { service::install()? } else { service::uninstall()? };
            if install && current() == Autostart::LoginAdmin {
                apply(Autostart::Login, exe)?;
            }
            return Ok(msg);
        }
        let code = run_elevated(exe, if install { "service install" } else { "service uninstall" }, false, true)?;
        if code != 0 {
            bail!("the elevated helper failed (exit code {code}); `rdesk service status` in a terminal says more");
        }
        Ok(if install { "The rdesk service is installed and running." } else { "The rdesk service is removed." }.into())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use anyhow::Context;

    fn plist() -> Result<std::path::PathBuf> {
        let home = directories::BaseDirs::new().context("no home folder")?.home_dir().to_path_buf();
        Ok(home.join("Library/LaunchAgents/dev.rdesk.app.plist"))
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn current() -> Autostart {
        if plist().is_ok_and(|p| p.exists()) { Autostart::Login } else { Autostart::Off }
    }

    pub fn apply(mode: Autostart, exe: &Path) -> Result<()> {
        let p = plist()?;
        match mode {
            Autostart::Off => {
                if p.exists() {
                    std::fs::remove_file(&p)?;
                }
                Ok(())
            }
            Autostart::Login => {
                let args: String = ARGS.iter().map(|a| format!("<string>{a}</string>")).collect();
                let xml = format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.rdesk.app</string>
  <key>ProgramArguments</key><array><string>{}</string>{args}</array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict></plist>
"#,
                    esc(&exe.display().to_string())
                );
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, xml)?;
                Ok(())
            }
            Autostart::LoginAdmin => bail!("not available on macOS"),
        }
    }

    pub fn apply_with_elevation(mode: Autostart, exe: &Path) -> Result<()> {
        apply(mode, exe)
    }

    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub fn restart_as_admin(_exe: &Path) -> Result<()> {
        bail!("not available on macOS")
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    use anyhow::Context;

    fn entry() -> Result<std::path::PathBuf> {
        let cfg = directories::BaseDirs::new().context("no home folder")?.config_dir().to_path_buf();
        Ok(cfg.join("autostart/rdesk.desktop"))
    }

    pub fn current() -> Autostart {
        if entry().is_ok_and(|p| p.exists()) { Autostart::Login } else { Autostart::Off }
    }

    pub fn apply(mode: Autostart, exe: &Path) -> Result<()> {
        let p = entry()?;
        match mode {
            Autostart::Off => {
                if p.exists() {
                    std::fs::remove_file(&p)?;
                }
                Ok(())
            }
            Autostart::Login => {
                let text = format!(
                    "[Desktop Entry]\nType=Application\nName=rdesk\nComment=Remote desktop\nExec=\"{}\" {}\nX-GNOME-Autostart-enabled=true\n",
                    exe.display(),
                    ARGS.join(" ")
                );
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, text)?;
                Ok(())
            }
            Autostart::LoginAdmin => bail!("not available on Linux"),
        }
    }

    pub fn apply_with_elevation(mode: Autostart, exe: &Path) -> Result<()> {
        apply(mode, exe)
    }

    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub fn restart_as_admin(_exe: &Path) -> Result<()> {
        bail!("not available on Linux")
    }
}
