//! Tray icon + menu.

use crate::win::{self, AppEvent};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub const ASK: &str = "ask";
pub const REGION: &str = "region";
pub const SETTINGS: &str = "settings";
pub const RELOAD: &str = "reload";
pub const HISTORY: &str = "history";
pub const AUTOSTART: &str = "autostart";
pub const QUIT: &str = "quit";

pub struct Tray {
    _icon: TrayIcon,
    pub autostart: CheckMenuItem,
}

fn icon() -> tray_icon::Icon {
    let n = 32u32;
    let mut pm = tiny_skia::Pixmap::new(n, n).expect("pixmap");
    let mut paint = tiny_skia::Paint::default();
    paint.anti_alias = true;
    paint.set_color_rgba8(255, 99, 72, 255);
    if let Some(c) = tiny_skia::PathBuilder::from_circle(16.0, 16.0, 14.0) {
        pm.fill_path(
            &c,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }
    paint.set_color_rgba8(255, 255, 255, 255);
    if let Some(c) = tiny_skia::PathBuilder::from_circle(16.0, 16.0, 5.5) {
        pm.fill_path(
            &c,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }
    // tiny-skia is premultiplied; tray-icon wants straight RGBA.
    let rgba: Vec<u8> = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    tray_icon::Icon::from_rgba(rgba, n, n).expect("icon")
}

pub fn create(hotkey: &str, region_hotkey: &str) -> anyhow::Result<Tray> {
    let menu = Menu::new();
    let autostart = CheckMenuItem::with_id(
        AUTOSTART,
        "Start with Windows",
        true,
        win::autostart_enabled(),
        None,
    );
    menu.append_items(&[
        &MenuItem::with_id(ASK, format!("Ask about screen\t{hotkey}"), true, None),
        &MenuItem::with_id(
            REGION,
            format!("Ask about region\t{region_hotkey}"),
            true,
            None,
        ),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(SETTINGS, "Edit settings…", true, None),
        &MenuItem::with_id(RELOAD, "Reload settings", true, None),
        &MenuItem::with_id(HISTORY, "Open history folder", true, None),
        &autostart,
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(QUIT, "Quit", true, None),
    ])?;
    MenuEvent::set_event_handler(Some(|e: MenuEvent| win::push(AppEvent::Menu(e.id.0))));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
        {
            win::push(AppEvent::TrayClick);
        }
    }));
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(format!("screenbuddy — {hotkey}"))
        .with_icon(icon())
        .build()?;
    Ok(Tray {
        _icon: icon,
        autostart,
    })
}

pub enum Passive {
    Handled,
    Quit,
    /// Needs the palette closed first (e.g. starting a new capture).
    NeedsIdle,
}

/// Menu actions that work regardless of whether the palette is open.
pub fn handle_passive(id: &str) -> Passive {
    match id {
        SETTINGS => {
            let _ = crate::config::load(); // ensure the file exists
            win::shell_open(&crate::config::config_path());
            Passive::Handled
        }
        HISTORY => {
            win::shell_open(&crate::config::history_dir());
            Passive::Handled
        }
        AUTOSTART => {
            let on = !win::autostart_enabled();
            if let Err(e) = win::set_autostart(on) {
                win::message_box(&format!("Could not change autostart: {e}"));
            }
            Passive::Handled
        }
        QUIT => Passive::Quit,
        _ => Passive::NeedsIdle,
    }
}
