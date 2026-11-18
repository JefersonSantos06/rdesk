//! One authenticated client session: wires the QUIC streams to capture,
//! input, clipboard and files.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use rdesk_common::auth::Authenticated;
use rdesk_common::clipboard::ClipboardSync;
use rdesk_common::config::HostConfig;
use rdesk_common::files;
use rdesk_proto::{
    ClientMsg, FrameAck, HostMsg, InputEvent, MAX_CONTROL_MSG, MAX_SMALL_MSG, MonitorInfo, StreamOpen, expect_msg,
    read_msg, write_msg,
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
}

pub fn pick_initial(monitors: &[MonitorDesc]) -> Option<&MonitorDesc> {
    monitors.iter().find(|m| m.info.primary).or_else(|| monitors.first())
}

pub fn infos(monitors: &[MonitorDesc]) -> Vec<MonitorInfo> {
    monitors.iter().map(|m| m.info.clone()).collect()
}

pub async fn run(
    conn: quinn::Connection,
    mut ctrl_w: quinn::SendStream,
    mut ctrl_r: quinn::RecvStream,
    auth: Authenticated,
    shared: Arc<Shared>,
    monitors: Vec<MonitorDesc>,
) -> Result<()> {
    let peer = conn.remote_address();
    tracing::info!("session {} started: {} ({:?}) from {peer}", hex(&auth.session_id), auth.client_name, auth.kind);
    let _awake = KeepAwake::start();
    let injector = shared.injector.clone();
    injector.event(InputEvent::ReleaseAll);

    let initial = pick_initial(&monitors).ok_or_else(|| anyhow!("no monitors"))?.clone();
    injector.send(InputCmd::Monitor(initial.input_rect));
    let monitors = Arc::new(tokio::sync::Mutex::new(monitors));

    let (settings_tx, settings_rx) = watch::channel(VideoSettings {
        monitor: initial.info.id,
        max_fps: shared.cfg.max_fps,
        jpeg_quality: shared.cfg.jpeg_quality,
        epoch: 0,
    });
    let (ack_tx, ack_rx) = mpsc::unbounded_channel::<u64>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostMsg>();
    let stats = Arc::new(std::sync::Mutex::new(VideoStats { frames: 0, bytes: 0 }));

    let clipboard = if shared.cfg.clipboard {
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
                    ClientMsg::ListDir { req, path } => {
                        let out = out.clone();
                        tokio::spawn(async move {
                            let (path, result) = match files::list_dir(&path).await {
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
                                    injector.event(ev);
                                }
                            }
                            StreamOpen::Acks => {
                                while let Some(a) = read_msg::<_, FrameAck>(&mut s, MAX_SMALL_MSG).await? {
                                    let _ = ack_tx.send(a.frame_id);
                                }
                            }
                            StreamOpen::File => return Err(anyhow!("file stream must be bidirectional")),
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
        tasks.spawn(async move {
            loop {
                let (mut w, mut r) = match conn.accept_bi().await {
                    Ok(s) => s,
                    Err(_) => return Ok("bi acceptor"),
                };
                let inbox = inbox.clone();
                tokio::spawn(async move {
                    let res: Result<()> = async {
                        match expect_msg::<_, StreamOpen>(&mut r, MAX_SMALL_MSG).await? {
                            StreamOpen::File => files::serve(&mut r, &mut w, &inbox).await,
                            other => Err(anyhow!("unexpected bidirectional stream {other:?}")),
                        }
                    }
                    .await;
                    match res {
                        Ok(()) => {
                            let _ = w.finish();
                        }
                        Err(e) => tracing::warn!("file transfer failed: {e:#}"),
                    }
                });
            }
        });
    }

    // Video.
    {
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
                    conn.rtt().as_millis()
                );
            }
        });
    }

    // Tell the client what it can choose from.
    {
        let list = monitors.lock().await;
        let _ = out_tx.send(HostMsg::Monitors { monitors: infos(&list), active: initial.info.id });
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

pub fn hex(b: &[u8]) -> String {
    b.iter().take(4).map(|x| format!("{x:02x}")).collect()
}
