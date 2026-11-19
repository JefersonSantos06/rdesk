//! The tray (notification area / menu bar) icon and its menu.

use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::AppCmd;

#[cfg_attr(not(feature = "host"), allow(dead_code))]
pub struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    toggle: MenuItem,
    last: (String, bool),
}

impl Tray {
    /// Must run on the main thread once the event loop is running (eframe's app creator).
    pub fn create(tx: Sender<AppCmd>, ctx: egui::Context, host_supported: bool) -> Result<Self> {
        let open = MenuItem::with_id("open", "Open rdesk", true, None);
        let status = MenuItem::with_id("status", "Host: off", false, None);
        let toggle = MenuItem::with_id("toggle", "Turn host on", host_supported, None);
        let fp = MenuItem::with_id("fingerprint", "Copy host fingerprint", host_supported, None);
        let quit = MenuItem::with_id("quit", "Quit rdesk", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &status,
            &toggle,
            &fp,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| anyhow!("tray menu: {e}"))?;

        let size = 32;
        let icon = Icon::from_rgba(super::icon::rgba(size), size, size).map_err(|e| anyhow!("tray icon: {e}"))?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("rdesk")
            .with_icon(icon)
            .build()
            .map_err(|e| anyhow!("tray icon: {e}"))?;

        let (tx2, ctx2) = (tx.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let cmd = match e.id.as_ref() {
                "open" => AppCmd::Show,
                "toggle" => AppCmd::ToggleHost,
                "fingerprint" => AppCmd::CopyFingerprint,
                "quit" => AppCmd::Quit,
                _ => return,
            };
            let _ = tx2.send(cmd);
            ctx2.request_repaint();
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let show = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if show {
                let _ = tx.send(AppCmd::Show);
                ctx.request_repaint();
            }
        }));
        Ok(Self { icon, status, toggle, last: (String::new(), false) })
    }

    /// Reflect the host state in the menu and tooltip (cheap to call every frame).
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub fn update(&mut self, summary: &str, host_on: bool) {
        if self.last.0 == summary && self.last.1 == host_on {
            return;
        }
        self.last = (summary.to_string(), host_on);
        self.status.set_text(format!("Host: {summary}"));
        self.toggle.set_text(if host_on { "Turn host off" } else { "Turn host on" });
        let _ = self.icon.set_tooltip(Some(format!("rdesk — host {summary}")));
    }
}
