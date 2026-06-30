//! Native macOS menu bar (via `muda`). Adds an app menu (About/Quit) and a Help
//! menu with **Keyboard Shortcuts (Cmd+/)** that fires a menu event the app turns
//! into the cheat-sheet overlay — so the shortcut is discoverable and shown
//! natively in the menu.
//!
//! macOS only. winit has no menu API, and muda's Linux backend is GTK (which
//! can't attach to a winit window; GNOME has no menu bar; KDE/XFCE want DBusMenu),
//! so on every other platform this is an inert stub and the in-app Cmd+/ keybind
//! remains the portable path.

#[cfg(target_os = "macos")]
mod imp {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

    /// Owns the menu (kept alive for the app's lifetime) + the id of the
    /// "Keyboard Shortcuts" item so its clicks can be recognized.
    pub struct AppMenu {
        _menu: Menu,
        shortcuts_id: MenuId,
    }

    /// Build + install the menu bar as the NSApp main menu. Call once, on the main
    /// thread, after the app is initialized (e.g. winit `resumed`).
    pub fn build() -> AppMenu {
        let menu = Menu::new();

        // App menu (the first submenu becomes the bold app menu on macOS).
        let app_menu = Submenu::new("Ember", true);
        let _ = app_menu.append(&PredefinedMenuItem::about(Some("Ember"), None));
        let _ = app_menu.append(&PredefinedMenuItem::separator());
        let _ = app_menu.append(&PredefinedMenuItem::quit(None));
        let _ = menu.append(&app_menu);

        // Help menu with the cheat-sheet shortcut (Cmd+/).
        let help = Submenu::new("Help", true);
        let shortcuts = MenuItem::new(
            "Keyboard Shortcuts",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::Slash)),
        );
        let shortcuts_id = shortcuts.id().clone();
        let _ = help.append(&shortcuts);
        let _ = menu.append(&help);

        menu.init_for_nsapp();
        AppMenu {
            _menu: menu,
            shortcuts_id,
        }
    }

    /// Drain pending menu events; return `true` if the Shortcuts item was chosen.
    pub fn take_shortcuts_event(menu: &AppMenu) -> bool {
        let mut chosen = false;
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            if event.id == menu.shortcuts_id {
                chosen = true;
            }
        }
        chosen
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// Inert menu handle on non-macOS platforms.
    pub struct AppMenu;

    pub fn build() -> AppMenu {
        AppMenu
    }

    pub fn take_shortcuts_event(_menu: &AppMenu) -> bool {
        false
    }
}

pub use imp::{AppMenu, build, take_shortcuts_event};
