//! System tray icon.
//!
//! The tray lives on its own thread with its own message pump (tray-icon
//! delivers click/menu events through a hidden window's message procedure,
//! so that thread must pump messages). Two channels connect it to the app:
//!   - `TrayCommand` (tray -> app): Show (clicked) / Quit (menu)
//!   - `TrayMsg`    (app -> tray): tooltip updates (TrayIcon is Rc-based,
//!     so it can only be touched from the tray thread)

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

/// Commands from the tray to the app.
pub enum TrayCommand {
    Show,
    Quit,
    ClockIn,
    ClockOut,
    ToggleAutostart,
}

/// Commands from the app to the tray thread.
pub enum TrayMsg {
    SetTooltip(String),
    /// Enable/disable the Clock in / Clock out menu items.
    SetClockState { clocked_in: bool },
    /// Update the autostart checkbox in the menu.
    SetAutostartChecked(bool),
}

pub fn spawn_tray(
    command_tx: mpsc::Sender<TrayCommand>,
    msg_rx: mpsc::Receiver<TrayMsg>,
    autostart: bool,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let menu_command_tx = command_tx.clone();
        // Global event handlers; they fire on this thread via the tray
        // window's message procedure.
        TrayIconEvent::set_event_handler(Some(move |event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => {
                let _ = command_tx.send(TrayCommand::Show);
            }
            _ => {}
        }));

        let show_item = MenuItem::new("Show", true, None);
        let clock_in_item = MenuItem::new("Clock in", true, None);
        let clock_out_item = MenuItem::new("Clock out", false, None);
        let autostart_item = CheckMenuItem::new("Start with Windows", true, autostart, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        if let Err(err) = menu.append_items(&[
            &clock_in_item,
            &clock_out_item,
            &PredefinedMenuItem::separator(),
            &show_item,
            &autostart_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ]) {
            log::error!("failed to build tray menu: {err}");
        }
        let clock_in_id = clock_in_item.id().clone();
        let clock_out_id = clock_out_item.id().clone();
        let show_id = show_item.id().clone();
        let autostart_id = autostart_item.id().clone();
        let quit_id = quit_item.id().clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == clock_in_id {
                let _ = menu_command_tx.send(TrayCommand::ClockIn);
            } else if event.id == clock_out_id {
                let _ = menu_command_tx.send(TrayCommand::ClockOut);
            } else if event.id == show_id {
                let _ = menu_command_tx.send(TrayCommand::Show);
            } else if event.id == autostart_id {
                let _ = menu_command_tx.send(TrayCommand::ToggleAutostart);
            } else if event.id == quit_id {
                let _ = menu_command_tx.send(TrayCommand::Quit);
            }
        }));

        let icon = match load_icon() {
            Ok(icon) => icon,
            Err(err) => {
                log::error!("failed to load tray icon: {err:#}");
                return;
            }
        };
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip("Derrick")
            // Left click = show the window, not the menu.
            .with_menu_on_left_click(false)
            .build()
        {
            Ok(tray) => tray,
            Err(err) => {
                log::error!("failed to create tray icon: {err}");
                return;
            }
        };
        log::info!("tray icon created");

        // Message pump for the tray window + command drain.
        let mut msg = unsafe { std::mem::zeroed::<windows_sys::Win32::UI::WindowsAndMessaging::MSG>() };
        loop {
            unsafe {
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PM_REMOVE, PeekMessageW, TranslateMessage,
                };
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            while let Ok(msg) = msg_rx.try_recv() {
                match msg {
                    TrayMsg::SetTooltip(tooltip) => {
                        let _ = tray.set_tooltip(Some(tooltip));
                    }
                    TrayMsg::SetClockState { clocked_in } => {
                        let _ = clock_in_item.set_enabled(!clocked_in);
                        let _ = clock_out_item.set_enabled(clocked_in);
                    }
                    TrayMsg::SetAutostartChecked(checked) => {
                        let _ = autostart_item.set_checked(checked);
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    })
}

fn load_icon() -> anyhow::Result<Icon> {
    let png = include_bytes!("../assets/tray_icon.png");
    let img = image::load_from_memory(png)?.to_rgba8();
    let (width, height) = img.dimensions();
    Ok(Icon::from_rgba(img.into_raw(), width, height)?)
}
