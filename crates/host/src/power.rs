//! Keep the display awake while a session is active (a sleeping display
//! blanks or pauses screen capture).

pub struct KeepAwake {
    #[cfg(windows)]
    stop: Option<std::sync::mpsc::Sender<()>>,
    #[cfg(not(windows))]
    child: Option<std::process::Child>,
}

impl KeepAwake {
    pub fn start() -> Self {
        #[cfg(windows)]
        {
            // SetThreadExecutionState is per thread, so hold it on a parked thread.
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let _ = std::thread::Builder::new().name("rdesk-awake".into()).spawn(move || {
                use windows_sys::Win32::System::Power::{
                    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
                };
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
                let _ = rx.recv();
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            });
            Self { stop: Some(tx) }
        }
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id().to_string();
            let child = std::process::Command::new("caffeinate").args(["-dis", "-w", &pid]).spawn().ok();
            Self { child }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let child = std::process::Command::new("systemd-inhibit")
                .args(["--what=idle:sleep", "--who=rdesk", "--why=remote session", "sleep", "infinity"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok();
            Self { child }
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.stop.take();
        }
        #[cfg(not(windows))]
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
