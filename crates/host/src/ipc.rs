//! The host's control pipe, for when the Windows service runs the host: the desktop app
//! asks it for its status, ends the session, or restarts it after a settings change
//! (administrators only). One JSON request line, one JSON response line.

use std::time::Duration;

use anyhow::{Result, anyhow};
use rdesk_common::t;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::control::{Ctl, HostState, HostStatus, Reporter};

pub const PIPE: &str = r"\\.\pipe\rdesk-host";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    /// End the current session (anyone at the computer may).
    Disconnect,
    /// Stop; the service starts the host again with the new settings. Administrators only.
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Response {
    Status(Box<HostStatus>),
    Ok,
    Error(String),
}

/// Ask the host run by the service.
pub async fn request(req: &Request) -> Result<Response> {
    #[cfg(windows)]
    {
        imp::request(req).await
    }
    #[cfg(not(windows))]
    {
        let _ = req;
        Err(anyhow!(t!("the rdesk service exists only on Windows", "o serviço do rdesk só existe no Windows")))
    }
}

async fn expect_ok(req: Request) -> Result<()> {
    match request(&req).await? {
        Response::Ok => Ok(()),
        Response::Error(e) => Err(anyhow!(e)),
        Response::Status(_) => Err(anyhow!(t!("unexpected answer from the host", "resposta inesperada do host"))),
    }
}

/// The app's view of the host run by the service: polls its status into `reporter`.
pub struct RemoteHost {
    task: tokio::task::JoinHandle<()>,
}

impl RemoteHost {
    pub fn start(rt: &tokio::runtime::Handle, reporter: Reporter) -> Self {
        let task = rt.spawn(async move {
            loop {
                let status = match tokio::time::timeout(Duration::from_secs(3), request(&Request::Status)).await {
                    Ok(Ok(Response::Status(s))) => *s,
                    _ => HostStatus {
                        state: HostState::Failed(
                            t!(
                                "The rdesk service is not answering (it may be starting, or stopped).",
                                "O serviço do rdesk não está respondendo (pode estar iniciando ou parado)."
                            )
                            .into(),
                        ),
                        ..Default::default()
                    },
                };
                reporter.replace(status);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Self { task }
    }

    pub async fn disconnect(&self) -> Result<()> {
        expect_ok(Request::Disconnect).await
    }

    /// Apply changed settings: the service restarts the host.
    pub async fn restart(&self) -> Result<()> {
        expect_ok(Request::Restart).await
    }
}

impl Drop for RemoteHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answer the app on the control pipe until the process ends.
pub fn serve(reporter: Reporter, ctl: mpsc::UnboundedSender<Ctl>) {
    #[cfg(windows)]
    tokio::spawn(async move {
        if let Err(e) = imp::serve(reporter, ctl).await {
            tracing::warn!("control pipe: {e:#}");
        }
    });
    #[cfg(not(windows))]
    let _ = (reporter, ctl);
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::null_mut;

    use anyhow::{Result, anyhow, bail};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use tokio::sync::mpsc;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        WinBuiltinAdministratorsSid,
    };
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    use super::{PIPE, Request, Response};
    use crate::control::{Ctl, Reporter};

    const ERROR_PIPE_BUSY: i32 = 231;

    pub async fn request(req: &Request) -> Result<Response> {
        let mut tries = 0;
        let pipe = loop {
            match ClientOptions::new().open(PIPE) {
                Ok(p) => break p,
                Err(e) if tries < 20 && e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let mut pipe = BufReader::new(pipe);
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        pipe.get_mut().write_all(line.as_bytes()).await?;
        let mut answer = String::new();
        pipe.read_line(&mut answer).await?;
        Ok(serde_json::from_str(&answer)?)
    }

    /// SYSTEM and Administrators: everything; people signed in at the computer: read and write.
    fn security_descriptor() -> Result<usize> {
        let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)".encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        };
        if ok == 0 {
            bail!("building the pipe's permissions: {}", std::io::Error::last_os_error());
        }
        // Lives as long as the process (one per host run).
        Ok(sd as usize)
    }

    fn create(sd: usize, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd as PSECURITY_DESCRIPTOR,
            bInheritHandle: 0,
        };
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(PIPE, (&mut sa as *mut SECURITY_ATTRIBUTES).cast())
        }
    }

    /// Is the program on the other end of the pipe running elevated as an administrator?
    fn client_is_admin(pipe: &NamedPipeServer) -> bool {
        unsafe {
            if ImpersonateNamedPipeClient(pipe.as_raw_handle() as HANDLE) == 0 {
                return false;
            }
            let mut token: HANDLE = null_mut();
            let opened = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token);
            RevertToSelf();
            if opened == 0 {
                return false;
            }
            let token = OwnedHandle::from_raw_handle(token);
            let mut sid = [0u8; 68];
            let mut size = sid.len() as u32;
            if CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), sid.as_mut_ptr().cast(), &mut size) == 0 {
                return false;
            }
            let mut member = 0;
            CheckTokenMembership(token.as_raw_handle(), sid.as_mut_ptr().cast(), &mut member) != 0 && member != 0
        }
    }

    async fn answer(mut pipe: NamedPipeServer, reporter: Reporter, ctl: mpsc::UnboundedSender<Ctl>) -> Result<()> {
        let mut line = String::new();
        BufReader::new(&mut pipe).read_line(&mut line).await?;
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Status) => Response::Status(Box::new(reporter.snapshot())),
            Ok(Request::Disconnect) => {
                let _ = ctl.send(Ctl::Disconnect);
                Response::Ok
            }
            Ok(Request::Restart) if client_is_admin(&pipe) => {
                reporter.event(
                    rdesk_common::t!("Restarting with the new settings", "Reiniciando com as novas configurações"),
                    false,
                );
                let _ = ctl.send(Ctl::Stop);
                Response::Ok
            }
            Ok(Request::Restart) => Response::Error(
                rdesk_common::t!(
                    "only an administrator can restart the host",
                    "só um administrador pode reiniciar o host"
                )
                .into(),
            ),
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        pipe.write_all(out.as_bytes()).await?;
        pipe.flush().await?;
        let _ = pipe.disconnect();
        Ok(())
    }

    pub async fn serve(reporter: Reporter, ctl: mpsc::UnboundedSender<Ctl>) -> Result<()> {
        let sd = security_descriptor()?;
        // `first`: refuse to start if someone else already owns the name.
        let mut next = create(sd, true).map_err(|e| anyhow!("creating {PIPE}: {e}"))?;
        loop {
            next.connect().await?;
            let connected = std::mem::replace(&mut next, create(sd, false)?);
            let (reporter, ctl) = (reporter.clone(), ctl.clone());
            tokio::spawn(async move {
                if let Err(e) = answer(connected, reporter, ctl).await {
                    tracing::debug!("control pipe client: {e:#}");
                }
            });
        }
    }
}
