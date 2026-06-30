//! Native menu bar — an OS seam (design §7). The app talks to this in **semantic
//! actions** ([`MenuAction`]); the per-platform implementation is hidden here.
//!
//! macOS uses `muda` to install a real `NSMenu`. Other platforms get an inert
//! stub (winit has no menu API; muda's Linux backend is GTK and can't attach to a
//! winit window, GNOME has no menu bar, and KDE/XFCE want DBusMenu) — there the
//! in-app Cmd+/ overlay remains the portable path. Either way the app code is
//! identical: `build_menu()` then poll `menu_action()` each tick.

/// A menu item the user chose, in platform-agnostic terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Help → Keyboard Shortcuts (also bound to Cmd+/).
    ShowShortcuts,
    /// Ember → About Ember.
    About,
    /// Ember → Settings… (also bound to Cmd+,).
    Settings,
}

#[cfg(target_os = "macos")]
pub use imp::{AppMenu, build_menu, menu_action};

#[cfg(target_os = "macos")]
mod imp {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

    use super::MenuAction;

    /// Owns the installed menu (kept alive for the app's life) + the ids needed to
    /// map muda events back to [`MenuAction`]s.
    pub struct AppMenu {
        _menu: Menu,
        about_id: MenuId,
        settings_id: MenuId,
        shortcuts_id: MenuId,
    }

    /// Build + install the menu bar as the NSApp main menu. Call once on the main
    /// thread after the app is initialized (e.g. winit `resumed`).
    pub fn build_menu() -> AppMenu {
        let menu = Menu::new();

        // App menu (first submenu = bold app menu on macOS).
        let app_menu = Submenu::new("Ember", true);
        let about = MenuItem::new("About Ember", true, None);
        let about_id = about.id().clone();
        let _ = app_menu.append(&about);
        let _ = app_menu.append(&PredefinedMenuItem::separator());
        let settings = MenuItem::new(
            "Settings…",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::Comma)),
        );
        let settings_id = settings.id().clone();
        let _ = app_menu.append(&settings);
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
            about_id,
            settings_id,
            shortcuts_id,
        }
    }

    /// Drain pending menu events; return the most recent recognized action.
    pub fn menu_action(menu: &AppMenu) -> Option<MenuAction> {
        let mut action = None;
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            if event.id == menu.about_id {
                action = Some(MenuAction::About);
            } else if event.id == menu.settings_id {
                action = Some(MenuAction::Settings);
            } else if event.id == menu.shortcuts_id {
                action = Some(MenuAction::ShowShortcuts);
            }
        }
        action
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::{AppMenu, build_menu, menu_action};

#[cfg(not(target_os = "macos"))]
mod stub {
    use super::MenuAction;

    /// Inert menu handle on platforms without a native menu bar.
    pub struct AppMenu;

    pub fn build_menu() -> AppMenu {
        AppMenu
    }

    pub fn menu_action(_menu: &AppMenu) -> Option<MenuAction> {
        None
    }
}
