//! Tokens and processes in a given Windows session: the service starts the host there as
//! SYSTEM, and the host starts file helpers there as the signed-in user.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use windows_sys::Win32::Foundation::{
    GetLastError, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, IsWellKnownSid, SECURITY_ATTRIBUTES, SecurityImpersonation,
    SetTokenInformation, TOKEN_ALL_ACCESS, TOKEN_QUERY, TOKEN_USER, TokenPrimary, TokenSessionId, TokenUser,
    WinLocalSystemSid,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

fn last_error(what: &str) -> anyhow::Error {
    anyhow!("{what}: {}", std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32))
}

fn owned(h: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(h) }
}

/// The session at the physical screen and keyboard, if any (none during a session switch).
pub fn console_session() -> Option<u32> {
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// Is this process running as LocalSystem (started by the service)?
pub fn is_system() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let token = owned(token);
        let mut buf = [0u8; 256];
        let mut len = 0u32;
        if GetTokenInformation(token.as_raw_handle(), TokenUser, buf.as_mut_ptr().cast(), buf.len() as u32, &mut len)
            == 0
        {
            return false;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        IsWellKnownSid(user.User.Sid, WinLocalSystemSid) != 0
    }
}

/// A copy of this process's (SYSTEM) token, moved into `session`.
pub fn system_token_in(session: u32) -> Result<OwnedHandle> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token) == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        let token = owned(token);
        let mut dup: HANDLE = null_mut();
        if DuplicateTokenEx(
            token.as_raw_handle(),
            TOKEN_ALL_ACCESS,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        ) == 0
        {
            return Err(last_error("DuplicateTokenEx"));
        }
        let dup = owned(dup);
        if SetTokenInformation(dup.as_raw_handle(), TokenSessionId, (&session as *const u32).cast(), 4) == 0 {
            return Err(last_error("SetTokenInformation"));
        }
        Ok(dup)
    }
}

/// The token of whoever is signed in on `session` (not elevated). Needs SYSTEM.
pub fn user_token(session: u32) -> Result<OwnedHandle> {
    let mut token: HANDLE = null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        bail!("nobody is signed in on this computer");
    }
    Ok(owned(token))
}

/// A started process; closing it does not kill it.
pub struct Child {
    process: OwnedHandle,
    pub pid: u32,
}

impl Child {
    /// The exit code, if it has ended.
    pub fn try_wait(&self) -> Option<u32> {
        self.wait_timeout(Duration::ZERO)
    }

    pub fn wait_timeout(&self, d: Duration) -> Option<u32> {
        unsafe {
            match WaitForSingleObject(self.process.as_raw_handle(), d.as_millis().min(u32::MAX as u128) as u32) {
                WAIT_OBJECT_0 => {
                    let mut code = 0u32;
                    GetExitCodeProcess(self.process.as_raw_handle(), &mut code);
                    Some(code)
                }
                WAIT_TIMEOUT => None,
                _ => Some(u32::MAX),
            }
        }
    }

    pub fn kill(&self) {
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
    }
}

pub struct Spawned {
    pub child: Child,
    /// Our end of its standard input (closing it is its signal to stop).
    pub stdin: Option<File>,
    /// Our end of its standard output.
    pub stdout: Option<File>,
}

/// Where the environment of the new process comes from.
pub enum Env<'a> {
    /// This process's environment plus some variables.
    Inherit(&'a [(&'a str, OsString)]),
    /// The default environment of the token's user (profile folders, TEMP…).
    OfUser,
}

/// An anonymous pipe: (read end, write end); `inherit` says which end the child gets.
fn pipe(child_reads: bool) -> Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w): (HANDLE, HANDLE) = (null_mut(), null_mut());
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(last_error("CreatePipe"));
    }
    let (r, w) = (owned(r), owned(w));
    // Only the child's end may be inherited.
    let ours = if child_reads { &w } else { &r };
    unsafe { SetHandleInformation(ours.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) };
    Ok((r, w))
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// Quote one command-line argument the way the C runtime parses it.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Start `exe args…` with `token`, on the interactive desktop of the token's session.
pub fn spawn(
    token: &OwnedHandle,
    exe: &std::path::Path,
    args: &[&str],
    env: Env,
    stdin: bool,
    stdout: bool,
) -> Result<Spawned> {
    let mut cmdline = quote_arg(&exe.to_string_lossy());
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(&quote_arg(a));
    }
    let mut cmdline: Vec<u16> = OsStr::new(&cmdline).encode_wide().chain(Some(0)).collect();
    let exe_w = wide(exe.as_os_str());
    let mut desktop = wide(OsStr::new("winsta0\\default"));

    let stdin_pipe = if stdin { Some(pipe(true)?) } else { None };
    let stdout_pipe = if stdout { Some(pipe(false)?) } else { None };

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop.as_mut_ptr();
    if stdin || stdout {
        si.dwFlags = STARTF_USESTDHANDLES;
        if let Some((r, _)) = &stdin_pipe {
            si.hStdInput = r.as_raw_handle();
        }
        if let Some((_, w)) = &stdout_pipe {
            si.hStdOutput = w.as_raw_handle();
        }
    }

    // The environment block: UTF-16 "NAME=value\0…\0".
    let mut user_block: *mut core::ffi::c_void = null_mut();
    let mut own_block: Vec<u16> = Vec::new();
    match env {
        Env::Inherit(extra) => {
            let mut vars: Vec<(OsString, OsString)> =
                std::env::vars_os().filter(|(k, _)| !extra.iter().any(|(e, _)| k.eq_ignore_ascii_case(e))).collect();
            vars.extend(extra.iter().map(|(k, v)| (OsString::from(k), v.clone())));
            vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
            for (k, v) in vars {
                own_block.extend(k.encode_wide());
                own_block.push('=' as u16);
                own_block.extend(v.encode_wide());
                own_block.push(0);
            }
            own_block.push(0);
        }
        Env::OfUser => {
            if unsafe { CreateEnvironmentBlock(&mut user_block, token.as_raw_handle(), 0) } == 0 {
                return Err(last_error("CreateEnvironmentBlock"));
            }
        }
    }
    let env_ptr: *const core::ffi::c_void =
        if user_block.is_null() { own_block.as_ptr().cast() } else { user_block.cast_const() };

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessAsUserW(
            token.as_raw_handle(),
            exe_w.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            (stdin || stdout) as i32,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            env_ptr,
            null(),
            &si,
            &mut pi,
        )
    };
    let err = (ok == 0).then(|| last_error("CreateProcessAsUser"));
    if !user_block.is_null() {
        unsafe { DestroyEnvironmentBlock(user_block) };
    }
    if let Some(e) = err {
        return Err(e);
    }
    drop(owned(pi.hThread));
    // The child's ends are closed here, so EOF reaches us when the child exits.
    Ok(Spawned {
        child: Child { process: owned(pi.hProcess), pid: pi.dwProcessId },
        stdin: stdin_pipe.map(|(_, w)| File::from(w)),
        stdout: stdout_pipe.map(|(r, _)| File::from(r)),
    })
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("run"), "run");
        assert_eq!(quote_arg(r"C:\Program Files\rdesk\rdesk.exe"), r#""C:\Program Files\rdesk\rdesk.exe""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r#"a "b""#), r#""a \"b\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }
}
