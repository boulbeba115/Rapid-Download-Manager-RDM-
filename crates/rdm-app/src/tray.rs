//! Notification-area icon with a small menu.

use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrayAction {
    /// Left click / double click on the icon, or "Open".
    Show,
    AddUrl,
    StartQueue,
    StopAll,
    Exit,
}

pub struct Tray {
    // Dropping this removes the icon.
    _icon: TrayIcon,
    actions: Receiver<TrayAction>,
}

impl Tray {
    /// `None` if the icon cannot be created (the app then behaves as if there were no tray).
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        let menu = Menu::new();
        let open = MenuItem::new("Open RDM", true, None);
        let add = MenuItem::new("Add URL…", true, None);
        let start = MenuItem::new("Start main queue", true, None);
        let stop = MenuItem::new("Stop all downloads", true, None);
        let exit = MenuItem::new("Exit", true, None);
        menu.append(&open).ok()?;
        menu.append(&add).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&start).ok()?;
        menu.append(&stop).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&exit).ok()?;

        let icon = Icon::from_rgba(crate::icon::render(32), 32, 32).ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Rapid Download Manager")
            .with_icon(icon)
            .build()
            .ok()?;

        // Events arrive on the event-loop thread while the window may be
        // hidden; forward them and wake the app so it can react.
        let (tx, actions) = channel();
        let by_id = [
            (open.id().clone(), TrayAction::Show),
            (add.id().clone(), TrayAction::AddUrl),
            (start.id().clone(), TrayAction::StartQueue),
            (stop.id().clone(), TrayAction::StopAll),
            (exit.id().clone(), TrayAction::Exit),
        ];
        let (menu_tx, menu_ctx) = (tx.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some((_, action)) = by_id.iter().find(|(id, _)| *id == event.id) {
                let _ = menu_tx.send(*action);
                menu_ctx.request_repaint();
            }
        }));
        let icon_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let show = matches!(
                event,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if show {
                let _ = tx.send(TrayAction::Show);
                icon_ctx.request_repaint();
            }
        }));
        Some(Self { _icon: tray, actions })
    }

    pub fn poll(&self) -> Vec<TrayAction> {
        self.actions.try_iter().collect()
    }

    /// What hovering over the icon shows.
    pub fn set_tooltip(&self, text: &str) {
        let _ = self._icon.set_tooltip(Some(text));
    }
}
