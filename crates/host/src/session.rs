//! One authenticated client session: wires the QUIC streams to capture,
//! input, clipboard and files.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use rdesk_common::auth::Authenticated;
use rdesk_common::clipboard::ClipboardSync;
use rdesk_common::config::HostConfig;
use rdesk_common::files;
use rdesk_common::transport;
use rdesk_proto::{
    ClientMsg, FrameAck, HostMsg, InputEvent, MAX_CONTROL_MSG, MAX_SMALL_MSG, MonitorInfo, StreamOpen, TermClient,
    expect_msg, read_msg, write_msg,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::capture::{Capturer, MonitorDesc};
use crate::input::{Injector, InputCmd};
use crate::power::KeepAwake;
use crate::video::{self, VideoSettings, VideoStats};

pub struct Shared {
    pub cfg: HostConfig,
    pub capturer: Capturer,
    pub injector: Injector,
    /// Run by the Windows service (as SYSTEM): files go through a helper started as the
    /// signed-in user (see `win::files`).
    pub files_as_user: bool,
    /// For "… opened a terminal" in the activity and on the session bar.
    pub reporter: crate::control::Reporter,
}

pub fn pick_initial(monitors: &[MonitorDesc]) -> Option<&MonitorDesc> {
    monitors.iter().find(|m| m.info.primary).or_else(|| monitors.first())
}

pub fn infos(monitors: &[MonitorDesc]) -> Vec<MonitorInfo> {
    monitors.iter().map(|m| m.info.clone()).collect()
}

pub async fn run(
    conn: transport::Connection,
    mut ctrl_w: transport::SendStream,
    mut ctrl_r: transport::RecvStream,
    auth: Authenticated,
    shared: Arc<Shared>,
    monitors: Vec<MonitorDesc>,
) -> Result<()> {
    let peer = transport::describe_peer(&conn);
    let access = auth.access;
    let files_as_user = shared.files_as_user;
    let reporter = shared.reporter.clone();
    let who = match &auth.user {
        Some(u) => u.label(),
        None => auth.client_name.clone(),
    };
    tracing::info!(
        "session {} started: {} ({:?}, {}) from {peer}",
        hex(&auth.session_id),
        auth.client_name,
        auth.kind,
        access.label()
    );
    let _awake = KeepAwake::start();
    let injector = shared.injector.clone();
    injector.event(InputEvent::ReleaseAll);

    // A terminal- or files-only session (or a computer without a screen) needs no monitor.
    let initial = match pick_initial(&monitors) {
        Some(m) => Some(m.clone()),
        None if access.screen => return Err(anyhow!("no monitors")),
        None => None,
    };
    if let Some(m) = &initial {
        injector.send(InputCmd::Monitor(m.input_rect));
    }
    let initial_id = initial.as_ref().map(|m| m.info.id).unwrap_or(0);
    let monitors = Arc::new(tokio::sync::Mutex::new(monitors));

    let (settings_tx, settings_rx) = watch::channel(VideoSettings {
        monitor: initial_id,
        max_fps: shared.cfg.max_fps,
        jpeg_quality: shared.cfg.jpeg_quality,
        epoch: 0,
    });
    let (ack_tx, ack_rx) = mpsc::unbounded_channel::<u64>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostMsg>();
    let stats = Arc::new(std::sync::Mutex::new(VideoStats { frames: 0, bytes: 0 }));

    // The clipboard goes both ways, so it needs full control.
    let clipboard = if shared.cfg.clipboard && access.input {
        let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<String>();
        let sync = ClipboardSync::spawn(clip_tx);
        let out = out_tx.clone();
        tokio::spawn(async move {
            while let Some(text) = clip_rx.recv().await {
                if out.send(HostMsg::Clipboard { text }).is_err() {
                    break;
                }
            }
        });
        sync
    } else {
        None
    };

    let mut tasks: JoinSet<Result<&'static str>> = JoinSet::new();

    // Control writer.
    tasks.spawn(async move {
        while let Some(m) = out_rx.recv().await {
            write_msg(&mut ctrl_w, &m).await?;
        }
        Ok("control writer")
    });

    // Control reader.
    {
        let out = out_tx.clone();
        let injector = injector.clone();
        let monitors = monitors.clone();
        let capturer = shared.capturer.clone();
        tasks.spawn(async move {
            loop {
                let Some(msg) = read_msg::<_, ClientMsg>(&mut ctrl_r, MAX_CONTROL_MSG).await? else {
                    return Ok("control closed");
                };
                match msg {
                    ClientMsg::Ping { t_us } => {
                        let _ = out.send(HostMsg::Pong { t_us });
                    }
                    ClientMsg::SelectMonitor { id } => {
                        let mut list = monitors.lock().await;
                        if !list.iter().any(|m| m.info.id == id)
                            && let Ok(fresh) = capturer.monitors().await
                        {
                            *list = fresh;
                        }
                        if let Some(m) = list.iter().find(|m| m.info.id == id) {
                            injector.send(InputCmd::Monitor(m.input_rect));
                            settings_tx.send_modify(|s| s.monitor = id);
                            let _ = out.send(HostMsg::Monitors { monitors: infos(&list), active: id });
                        } else {
                            let _ = out.send(HostMsg::Notice { text: format!("monitor {id} not found") });
                        }
                    }
                    ClientMsg::SetQuality { max_fps, jpeg_quality } => {
                        settings_tx.send_modify(|s| {
                            s.max_fps = max_fps.clamp(1, 60);
                            s.jpeg_quality = jpeg_quality.clamp(10, 100);
                        });
                    }
                    ClientMsg::RequestFullFrame => settings_tx.send_modify(|s| s.epoch += 1),
                    ClientMsg::Clipboard { text } => {
                        if let Some(c) = &clipboard {
                            c.set(text);
                        }
                    }
                    ClientMsg::ListDir { req, path } if !access.files => {
                        let result = Err("this session may not access files".to_string());
                        let _ = out.send(HostMsg::DirListing { req, path, result });
                    }
                    ClientMsg::ListDir { req, path } => {
                        let out = out.clone();
                        tokio::spawn(async move {
                            let listed =
                                if files_as_user { list_as_user(&path).await } else { files::list_dir(&path).await };
                            let (path, result) = match listed {
                                Ok((p, entries)) => (p, Ok(entries)),
                                Err(e) => (path, Err(e)),
                            };
                            let _ = out.send(HostMsg::DirListing { req, path, result });
                        });
                    }
                    other => return Err(anyhow!("unexpected message after auth: {other:?}")),
                }
            }
        });
    }

    // Tell the remote user when the host cannot deliver their input (e.g. UIPI on Windows).
    {
        let mut problems = injector.subscribe();
        let out = out_tx.clone();
        tasks.spawn(async move {
            loop {
                match problems.recv().await {
                    Ok(text) => {
                        let _ = out.send(HostMsg::Notice { text });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    // The injector lives as long as the host; never end the session over this.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
                }
            }
        });
    }

    // Client-opened unidirectional streams: input and acks.
    {
        let conn = conn.clone();
        let injector = injector.clone();
        tasks.spawn(async move {
            loop {
                let mut s = match conn.accept_uni().await {
                    Ok(s) => s,
                    Err(_) => return Ok("uni acceptor"),
                };
                let injector = injector.clone();
                let ack_tx = ack_tx.clone();
                tokio::spawn(async move {
                    let r: Result<()> = async {
                        match expect_msg::<_, StreamOpen>(&mut s, MAX_SMALL_MSG).await? {
                            StreamOpen::Input => {
                                while let Some(ev) = read_msg::<_, InputEvent>(&mut s, MAX_SMALL_MSG).await? {
                                    // View-only sessions: everything is read and dropped.
                                    if access.input {
                                        injector.event(ev);
                                    }
                                }
                            }
                            StreamOpen::Acks => {
                                while let Some(a) = read_msg::<_, FrameAck>(&mut s, MAX_SMALL_MSG).await? {
                                    let _ = ack_tx.send(a.frame_id);
                                }
                            }
                            StreamOpen::File | StreamOpen::Terminal => {
                                return Err(anyhow!("file and terminal streams must be bidirectional"));
                            }
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(e) = r {
                        tracing::debug!("uni stream ended: {e:#}");
                    }
                });
            }
        });
    }

    // Client-opened bidirectional streams: file transfers.
    {
        let conn = conn.clone();
        let inbox = shared.cfg.inbox_dir();
        let chosen_inbox = shared.cfg.inbox_dir.clone();
        // Administrator rights in a terminal: only with every other right too.
        let admin_refused = if !shared.cfg.admin_terminal {
            Some("this computer does not allow a terminal as administrator")
        } else if access != rdesk_proto::Access::FULL {
            Some("a terminal as administrator needs full control")
        } else {
            None
        };
        tasks.spawn(async move {
            loop {
                let (mut w, mut r) = match conn.accept_bi().await {
                    Ok(s) => s,
                    Err(_) => return Ok("bi acceptor"),
                };
                let (inbox, chosen_inbox) = (inbox.clone(), chosen_inbox.clone());
                let (reporter, who) = (reporter.clone(), who.clone());
                tokio::spawn(async move {
                    let res: Result<()> = async {
                        match expect_msg::<_, StreamOpen>(&mut r, MAX_SMALL_MSG).await? {
                            StreamOpen::File if !access.files => Err(anyhow!("this session may not access files")),
                            StreamOpen::File if files_as_user => {
                                serve_as_user(&mut r, &mut w, chosen_inbox.as_deref()).await
                            }
                            StreamOpen::File => files::serve(&mut r, &mut w, &inbox).await,
                            StreamOpen::Terminal if !access.terminal => {
                                let reason = "this session may not open a terminal".to_string();
                                write_msg(&mut w, &rdesk_proto::TermHost::Error(reason)).await?;
                                Ok(())
                            }
                            StreamOpen::Terminal => {
                                let first = expect_msg::<_, TermClient>(&mut r, MAX_SMALL_MSG).await?;
                                let admin = first.start().is_some_and(|s| s.2);
                                if admin && let Some(why) = admin_refused {
                                    write_msg(&mut w, &rdesk_proto::TermHost::Error(why.into())).await?;
                                    return Ok(());
                                }
                                let what = if admin { "a terminal as administrator" } else { "a terminal" };
                                reporter.event(format!("{who} opened {what}"), false);
                                reporter.terminal(1, admin);
                                let r2 = if files_as_user {
                                    terminal_as_user(first, &mut r, &mut w).await
                                } else {
                                    crate::terminal::serve_from(first, &mut r, &mut w).await
                                };
                                reporter.terminal(-1, admin);
                                r2
                            }
                            other => Err(anyhow!("unexpected bidirectional stream {other:?}")),
                        }
                    }
                    .await;
                    match res {
                        Ok(()) => {
                            let _ = w.finish();
                        }
                        Err(e) => tracing::warn!("file transfer or terminal failed: {e:#}"),
                    }
                });
            }
        });
    }

    // Video (not for files-only sessions).
    if access.screen {
        let conn = conn.clone();
        let capturer = shared.capturer.clone();
        let stats = stats.clone();
        tasks.spawn(async move {
            video::run(conn, capturer, settings_rx, ack_rx, stats).await?;
            Ok("video")
        });
    }

    // Periodic log line.
    {
        let stats = stats.clone();
        let conn = conn.clone();
        tasks.spawn(async move {
            let mut last = (0u64, 0u64);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    _ = conn.closed() => return Ok("stats"),
                }
                let s = stats.lock().unwrap();
                let (df, db) = (s.frames - last.0, s.bytes - last.1);
                last = (s.frames, s.bytes);
                tracing::info!(
                    "video: {:.1} fps, {:.0} kbit/s, rtt {} ms",
                    df as f64 / 30.0,
                    db as f64 * 8.0 / 30.0 / 1000.0,
                    transport::rtt(&conn).as_millis()
                );
            }
        });
    }

    // Tell the client what it can choose from.
    if access.screen {
        let list = monitors.lock().await;
        let _ = out_tx.send(HostMsg::Monitors { monitors: infos(&list), active: initial_id });
    }

    // The session lives as long as the connection; the first task to finish ends it.
    let reason = tokio::select! {
        r = tasks.join_next() => match r {
            Some(Ok(Ok(what))) => what.to_string(),
            Some(Ok(Err(e))) => format!("error: {e:#}"),
            Some(Err(e)) => format!("task panicked: {e}"),
            None => "no tasks".into(),
        },
        e = conn.closed() => format!("connection closed: {e}"),
    };
    tasks.abort_all();
    injector.event(InputEvent::ReleaseAll);
    tracing::info!("session {} ended ({reason})", hex(&auth.session_id));
    Ok(())
}

#[cfg(windows)]
use crate::win::files::{list_as_user, serve_as_user, terminal_as_user};

#[cfg(not(windows))]
async fn list_as_user(_: &str) -> Result<(String, Vec<rdesk_proto::DirEntry>), String> {
    Err("not available".into())
}

#[cfg(not(windows))]
async fn serve_as_user<R, W>(_: &mut R, _: &mut W, _: Option<&std::path::Path>) -> Result<()> {
    Err(anyhow!("not available"))
}

#[cfg(not(windows))]
async fn terminal_as_user<R, W>(_: TermClient, _: &mut R, _: &mut W) -> Result<()> {
    Err(anyhow!("not available"))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().take(4).map(|x| format!("{x:02x}")).collect()
}
