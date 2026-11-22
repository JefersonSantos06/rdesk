//! One authenticated client session: wires the QUIC streams to capture,
//! input, clipboard and files.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use rdesk_common::auth::Authenticated;
use rdesk_common::clipboard::ClipboardSync;
use rdesk_common::config::HostConfig;
use rdesk_common::files;
use rdesk_common::transport;
use rdesk_common::{t, tf};
use rdesk_proto::{
    ClientMsg, EXT_REV, ExtClient, ExtHost, Feature, FrameAck, HostMsg, InputEvent, MAX_CONTROL_MSG, MAX_SMALL_MSG,
    MonitorInfo, StreamOpen, TermClient, expect_msg, read_msg, write_msg,
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
        max_size: (0, 0),
        auto: false,
    });
    let settings_tx = Arc::new(settings_tx);
    let (ack_tx, ack_rx) = mpsc::unbounded_channel::<u64>();
    // The picture starts when the client opens its acks stream: a client that only wants a
    // terminal (or files) leaves it closed, and the screen is never captured.
    let acks_opened = Arc::new(tokio::sync::Notify::new());
    // Which frame format the acks stream asked for (`None`: the first).
    let video_caps = Arc::new(std::sync::Mutex::new(None::<rdesk_proto::VideoCaps>));
    // What automatic quality chose, for the extensions stream to pass on.
    let (report_tx, report_rx) = watch::channel((0u8, 0u8, 0u8));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostMsg>();
    let stats = Arc::new(std::sync::Mutex::new(VideoStats { frames: 0, bytes: 0 }));

    // The clipboard goes both ways, so it needs full control.
    // Images copied here, for the extensions stream once the client asks for them.
    let (img_tx, img_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let clip_images = Arc::new(std::sync::Mutex::new(Some(img_rx)));
    let clipboard = if shared.cfg.clipboard && access.input {
        let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<String>();
        let sync = ClipboardSync::spawn(clip_tx, Some(img_tx)).map(Arc::new);
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
        let settings_tx = settings_tx.clone();
        let clipboard = clipboard.clone();
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
                            let _ = out.send(HostMsg::Notice {
                                text: tf!("monitor {id} not found", "tela {id} não encontrada"),
                            });
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
                        let result =
                            Err(t!("this session may not access files", "esta sessão não pode acessar arquivos")
                                .to_string());
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
        let acks_opened = acks_opened.clone();
        let video_caps = video_caps.clone();
        tasks.spawn(async move {
            loop {
                let mut s = match conn.accept_uni().await {
                    Ok(s) => s,
                    Err(_) => return Ok("uni acceptor"),
                };
                let injector = injector.clone();
                let ack_tx = ack_tx.clone();
                let acks_opened = acks_opened.clone();
                let video_caps = video_caps.clone();
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
                            open @ (StreamOpen::Acks | StreamOpen::Acks2(_)) => {
                                if let StreamOpen::Acks2(caps) = open {
                                    *video_caps.lock().unwrap() = Some(caps);
                                }
                                acks_opened.notify_one();
                                while let Some(a) = read_msg::<_, FrameAck>(&mut s, MAX_SMALL_MSG).await? {
                                    let _ = ack_tx.send(a.frame_id);
                                }
                            }
                            _ => {
                                return Err(anyhow!("file, terminal and extension streams must be bidirectional"));
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

    // Client-opened bidirectional streams: file transfers, terminals, extensions.
    {
        let conn = conn.clone();
        let injector = injector.clone();
        let settings_tx = settings_tx.clone();
        let (clipboard, clip_images) = (clipboard.clone(), clip_images.clone());
        let capturer = shared.capturer.clone();
        let inbox = shared.cfg.inbox_dir();
        let chosen_inbox = shared.cfg.inbox_dir.clone();
        // Administrator rights in a terminal: only with every other right too.
        let admin_refused = if !shared.cfg.admin_terminal {
            Some(t!(
                "this computer does not allow a terminal as administrator",
                "este computador não permite um terminal como administrador"
            ))
        } else if access != rdesk_proto::Access::FULL {
            Some(t!(
                "a terminal as administrator needs full control",
                "um terminal como administrador precisa de controle total"
            ))
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
                let (injector, settings_tx) = (injector.clone(), settings_tx.clone());
                let (report_rx, capturer) = (report_rx.clone(), capturer.clone());
                let (clipboard, clip_images) = (clipboard.clone(), clip_images.clone());
                tokio::spawn(async move {
                    let res: Result<()> = async {
                        match expect_msg::<_, StreamOpen>(&mut r, MAX_SMALL_MSG).await? {
                            StreamOpen::File if !access.files => Err(anyhow!("this session may not access files")),
                            StreamOpen::File if files_as_user => {
                                serve_as_user(&mut r, &mut w, chosen_inbox.as_deref()).await
                            }
                            StreamOpen::File => files::serve(&mut r, &mut w, &inbox).await,
                            StreamOpen::Terminal if !access.terminal => {
                                let reason = t!(
                                    "this session may not open a terminal",
                                    "esta sessão não pode abrir um terminal"
                                )
                                .to_string();
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
                                let what = if admin {
                                    t!("a terminal as administrator", "um terminal como administrador")
                                } else {
                                    t!("a terminal", "um terminal")
                                };
                                reporter.event(tf!("{who} opened {what}", "{who} abriu {what}"), false);
                                reporter.terminal(1, admin);
                                let r2 = if files_as_user {
                                    terminal_as_user(first, &mut r, &mut w).await
                                } else {
                                    crate::terminal::serve_from(first, &mut r, &mut w).await
                                };
                                reporter.terminal(-1, admin);
                                r2
                            }
                            StreamOpen::Audio => serve_audio(&mut r, &mut w, access.screen).await,
                            StreamOpen::Tunnel => {
                                let full = access == rdesk_proto::Access::FULL;
                                crate::tunnel::serve(&mut r, &mut w, full, |to| {
                                    reporter.event(
                                        tf!("{who} opened a tunnel to {to}", "{who} abriu um túnel para {to}"),
                                        false,
                                    )
                                })
                                .await
                            }
                            StreamOpen::Extensions => {
                                let ext = ExtCtx {
                                    settings: &settings_tx,
                                    injector,
                                    screen: access.screen,
                                    report: report_rx,
                                    capturer,
                                    clipboard,
                                    clip_images,
                                    full: access == rdesk_proto::Access::FULL,
                                    who: who.clone(),
                                };
                                serve_extensions(&mut r, &mut w, ext).await
                            }
                            other => Err(anyhow!("unexpected bidirectional stream {other:?}")),
                        }
                    }
                    .await;
                    match res {
                        Ok(()) => {
                            let _ = w.finish();
                        }
                        Err(e) => tracing::warn!("file transfer, terminal or extensions stream failed: {e:#}"),
                    }
                });
            }
        });
    }

    // Video (not for files-only sessions), once the client wants the picture.
    if access.screen {
        let conn = conn.clone();
        let capturer = shared.capturer.clone();
        let stats = stats.clone();
        tasks.spawn(async move {
            acks_opened.notified().await;
            let caps = *video_caps.lock().unwrap();
            let opts = video::VideoOpts { caps, report: Some(report_tx) };
            video::run(conn, capturer, settings_rx, ack_rx, stats, opts).await?;
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

/// How often the pointer is looked at while the client follows it.
const POINTER_EVERY: std::time::Duration = std::time::Duration::from_millis(33);

/// The computer's sound ([`StreamOpen::Audio`]), for sessions that see the screen, until the
/// client closes the stream.
async fn serve_audio(r: &mut transport::RecvStream, w: &mut transport::SendStream, screen: bool) -> Result<()> {
    use rdesk_proto::AudioHost;
    if !screen {
        let why = t!("this session may not hear the computer", "esta sessão não pode ouvir o computador");
        write_msg(w, &AudioHost::Error(why.into())).await?;
        return Ok(());
    }
    let mut packets = match crate::audio::start() {
        Ok(rx) => rx,
        Err(e) => {
            write_msg(w, &AudioHost::Error(format!("{e:#}"))).await?;
            return Ok(());
        }
    };
    let _ = w.set_priority(15);
    write_msg(w, &AudioHost::Start { sample_rate: crate::audio::RATE, channels: crate::audio::CHANNELS }).await?;
    let mut buf = [0u8; 64];
    loop {
        tokio::select! {
            p = packets.recv() => match p {
                Some(p) => write_msg(w, &AudioHost::Packet(p)).await?,
                None => return Ok(()),
            },
            // The client closed its side: no more sound.
            n = r.read(&mut buf) => if !matches!(n, Ok(Some(_))) { return Ok(()) },
        }
    }
}

/// What the extensions stream works with.
struct ExtCtx<'a> {
    settings: &'a watch::Sender<VideoSettings>,
    injector: Injector,
    screen: bool,
    report: watch::Receiver<(u8, u8, u8)>,
    capturer: Capturer,
    /// The clipboard (full control only), and images copied here waiting for a client that
    /// takes them.
    clipboard: Option<Arc<ClipboardSync>>,
    clip_images: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>>,
    /// Full control (restart, black screen, blocking the local keyboard).
    full: bool,
    /// Who is connected (for the restart's reason).
    who: String,
}

/// The extensions stream ([`StreamOpen::Extensions`]): the picture size limit, the pointer
/// (its position for clients that draw their own, its image for those that show it), the
/// second frame format and automatic quality.
async fn serve_extensions(r: &mut transport::RecvStream, w: &mut transport::SendStream, ctx: ExtCtx<'_>) -> Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    let ExtCtx { settings, injector, screen, mut report, capturer, clipboard, clip_images, full, who } = ctx;
    let (mut privacy_on, mut blocked) = (false, false);
    let ExtClient::Hello { rev } = expect_msg::<_, ExtClient>(r, MAX_SMALL_MSG).await? else {
        return Err(anyhow!("extensions stream must start with Hello"));
    };
    let mut features = Vec::new();
    if screen && rev >= 1 {
        features.extend([Feature::MaxSize, Feature::Cursor]);
    }
    if screen && rev >= 2 {
        features.extend([Feature::Video2, Feature::AutoQuality]);
        if crate::cursor::supported() {
            features.push(Feature::CursorShape);
        }
        if crate::audio::supported() {
            features.push(Feature::Audio);
        }
    }
    if rev >= 2 && clipboard.is_some() {
        features.push(Feature::ClipboardImage);
    }
    // Restarting, the black screen, blocking the local keyboard, tunnels: full control only.
    if rev >= 2 && full {
        features.push(Feature::Tunnel);
        if crate::restart::supported() {
            features.push(Feature::Power);
        }
        #[cfg(windows)]
        if crate::elevation::is_elevated() {
            features.push(Feature::BlockInput);
            if crate::win::privacy::supported() {
                features.push(Feature::Privacy);
            }
        }
    }
    write_msg(w, &ExtHost::Hello { rev: rev.min(EXT_REV), features }).await?;

    // 0: off; 1: on; 2: on, and send the next position even if it did not move.
    let follow = Arc::new(AtomicU8::new(0));
    let shapes_on = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = mpsc::channel::<ExtHost>(4);
    let images_tx = tx.clone();
    let notices = tx.clone();
    let injector_local = injector.clone();
    let mut images_task: Option<tokio::task::JoinHandle<()>> = None;
    let poller = {
        let (follow, tx) = (follow.clone(), tx.clone());
        tokio::spawn(async move {
            let mut last = None;
            loop {
                tokio::time::sleep(POINTER_EVERY).await;
                let state = follow.load(Ordering::Acquire);
                if state == 0 {
                    continue;
                }
                let p = injector.pointer().await;
                if state == 2 {
                    follow.store(1, Ordering::Release);
                    last = None;
                }
                if last != Some(p) {
                    last = Some(p);
                    if tx.send(ExtHost::Cursor { x: p.x, y: p.y, visible: p.visible }).await.is_err() {
                        return;
                    }
                }
            }
        })
    };
    // The pointer's image, looked at as often as its position. Each image goes once; after
    // that only its id.
    let shapes = {
        let (on, tx) = (shapes_on.clone(), tx);
        tokio::spawn(async move {
            let mut sent = std::collections::HashSet::new();
            let mut last: Option<u64> = None;
            loop {
                tokio::time::sleep(POINTER_EVERY).await;
                if !on.load(Ordering::Acquire) {
                    last = None;
                    continue;
                }
                let Some((id, image)) = capturer.cursor().await else { continue };
                if last == Some(id) {
                    continue;
                }
                last = Some(id);
                let image = if id == 0 || sent.contains(&id) { None } else { image };
                if image.is_some() {
                    sent.insert(id);
                }
                if tx.send(ExtHost::CursorShape { id, image }).await.is_err() {
                    return;
                }
            }
        })
    };
    let read = async {
        // Images make some messages big.
        while let Some(m) = read_msg::<_, ExtClient>(r, MAX_CONTROL_MSG).await? {
            match m {
                ExtClient::MaxSize { width, height } if screen => {
                    let size = if width == 0 || height == 0 { (0, 0) } else { (width.max(64), height.max(64)) };
                    settings.send_if_modified(|s| std::mem::replace(&mut s.max_size, size) != size);
                }
                ExtClient::Cursor { on } if screen => {
                    follow.store(if on { 2 } else { 0 }, Ordering::Release);
                    // It draws the pointer itself: the capture leaves it out where it can.
                    crate::pointer::set_client_draws(on);
                }
                ExtClient::AutoQuality { on } if screen => {
                    settings.send_if_modified(|s| std::mem::replace(&mut s.auto, on) != on);
                }
                ExtClient::CursorShape { on } if screen => shapes_on.store(on, Ordering::Release),
                ExtClient::ClipboardImages { on } => {
                    if let Some(c) = &clipboard {
                        c.set_images(on);
                        if on
                            && images_task.is_none()
                            && let Some(mut rx) = clip_images.lock().unwrap().take()
                        {
                            let tx = images_tx.clone();
                            images_task = Some(tokio::spawn(async move {
                                while let Some(png) = rx.recv().await {
                                    if tx.send(ExtHost::ClipImage(png)).await.is_err() {
                                        return;
                                    }
                                }
                            }));
                        }
                    }
                }
                ExtClient::ClipImage(png) => {
                    if let Some(c) = &clipboard {
                        c.set_image(png);
                    }
                }
                ExtClient::Power(action) if full => {
                    let safe = action == rdesk_proto::PowerAction::RestartSafeMode;
                    let who = who.clone();
                    let r = tokio::task::spawn_blocking(move || crate::restart::restart(safe, &who)).await?;
                    let text = match r {
                        Ok(()) if safe => t!(
                            "Restarting in safe mode with networking; reconnecting once it is back.",
                            "Reiniciando em modo de segurança com rede; reconectando quando voltar."
                        )
                        .to_string(),
                        Ok(()) => {
                            t!("Restarting; reconnecting once it is back.", "Reiniciando; reconectando quando voltar.")
                                .to_string()
                        }
                        Err(e) => tf!("Could not restart: {e:#}", "Não foi possível reiniciar: {e:#}"),
                    };
                    let _ = notices.send(ExtHost::Notice(text)).await;
                }
                ExtClient::Privacy { on } if full => {
                    #[cfg(windows)]
                    let r = tokio::task::spawn_blocking(move || crate::win::privacy::set(on)).await?;
                    #[cfg(not(windows))]
                    let r: Result<()> = Err(anyhow!("only on Windows computers"));
                    match r {
                        Ok(()) => privacy_on = on,
                        Err(e) => {
                            let text = tf!("No black screen: {e:#}", "Sem tela preta: {e:#}");
                            let _ = notices.send(ExtHost::Notice(text)).await;
                        }
                    }
                }
                ExtClient::BlockInput { on } if full => {
                    let (rtx, rrx) = tokio::sync::oneshot::channel();
                    injector_local.send(InputCmd::BlockLocal(on, rtx));
                    match rrx.await.unwrap_or_else(|_| Err(anyhow!("input thread gone"))) {
                        Ok(()) => blocked = on,
                        Err(e) => {
                            let text = tf!(
                                "Could not block the keyboard and mouse: {e:#}",
                                "Não foi possível bloquear o teclado e o mouse: {e:#}"
                            );
                            let _ = notices.send(ExtHost::Notice(text)).await;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let write = async {
        loop {
            let m = tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                r = report.changed() => {
                    if r.is_err() {
                        break;
                    }
                    let (fps, jpeg_quality, scale_pct) = *report.borrow_and_update();
                    ExtHost::Quality { fps, jpeg_quality, scale_pct }
                }
            };
            write_msg(w, &m).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::select! {
        r = read => r,
        r = write => r,
    };
    poller.abort();
    shapes.abort();
    if let Some(t) = images_task {
        t.abort();
    }
    // Whoever is at the computer gets it back as soon as the session ends.
    if privacy_on {
        #[cfg(windows)]
        let _ = crate::win::privacy::set(false);
    }
    if blocked {
        let (rtx, _) = tokio::sync::oneshot::channel();
        injector_local.send(InputCmd::BlockLocal(false, rtx));
    }
    crate::pointer::set_client_draws(false);
    result
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
