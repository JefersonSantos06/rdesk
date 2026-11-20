//! The tray (notification area / menu bar) icon and its menu.

use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use rdesk_common::i18n::{self, Lang};
use rdesk_common::t;

use super::AppCmd;

#[cfg_attr(not(feature = "host"), allow(dead_code))]
pub struct Tray {
    icon: TrayIcon,
    open: MenuItem,
    status: MenuItem,
    toggle: MenuItem,
    fingerprint: MenuItem,
    quit: MenuItem,
    /// What the menu shows now: the host summary, whether it is on, and the language.
    last: Option<(String, bool, Lang)>,
}

impl Tray {
    /// Must run on the main thread once the event loop is running (eframe's app creator).
    pub fn create(tx: Sender<AppCmd>, ctx: egui::Context, host_supported: bool) -> Result<Self> {
        // The texts are set by `update` (and again when the language changes).
        let open = MenuItem::with_id("open", "rdesk", true, None);
        let status = MenuItem::with_id("status", "", false, None);
        let toggle = MenuItem::with_id("toggle", "", host_supported, None);
        let fp = MenuItem::with_id("fingerprint", "", host_supported, None);
        let quit = MenuItem::with_id("quit", "", true, None);
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
        let mut tray = Self { icon, open, status, toggle, fingerprint: fp, quit, last: None };
        tray.update(t!("off", "desligado"), false);
        Ok(tray)
    }

    /// Reflect the host state in the menu and tooltip (cheap to call every frame).
    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub fn update(&mut self, summary: &str, host_on: bool) {
        let now = (summary.to_string(), host_on, i18n::lang());
        if self.last.as_ref() == Some(&now) {
            return;
        }
        if self.last.as_ref().is_none_or(|l| l.2 != now.2) {
            self.open.set_text(t!("Open rdesk", "Abrir o rdesk"));
            self.fingerprint.set_text(t!("Copy host ID", "Copiar o ID do host"));
            self.quit.set_text(t!("Quit rdesk", "Sair do rdesk"));
        }
        self.status.set_text(format!("Host: {summary}"));
        self.toggle.set_text(if host_on {
            t!("Turn host off", "Desligar o host")
        } else {
            t!("Turn host on", "Ligar o host")
        });
        let _ = self.icon.set_tooltip(Some(format!("rdesk — host {summary}")));
        self.last = Some(now);
    }
}
