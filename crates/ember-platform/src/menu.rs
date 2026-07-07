//! Native menu bar — an OS seam (design §7). The app talks to this in **semantic
//! actions** ([`MenuAction`]); the per-platform implementation is hidden here.
//!
//! macOS uses `muda` to install a real `NSMenu`. Other platforms get an inert
//! stub (winit has no menu API; muda's Linux backend is GTK and can't attach to a
//! winit window, GNOME has no menu bar, and KDE/XFCE want DBusMenu) — there the
//! in-app Cmd+/ overlay remains the portable path. Either way the app code is
//! identical: `build_menu()` then poll `menu_action()` each tick.

/// A menu item the user chose, in platform-agnostic terms. Exhaustive on purpose:
/// it's matched only in the app, so the compiler flags an unhandled item when a
/// variant is added (more useful than downstream compat here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Help → Keyboard Shortcuts (also bound to Cmd+/).
    ShowShortcuts,
    /// Ember → About Ember.
    About,
    /// Ember → Settings… (also bound to Cmd+,).
    Settings,
    /// Ember → Quit Ember (Cmd+Q). Routed through the app (NOT muda's
    /// predefined quit, which terminates NSApp directly and would bypass the
    /// running-process confirmation and session shutdown entirely).
    Quit,
    /// File → New Tab (Cmd+T).
    NewTab,
    /// File → New Window (Cmd+N).
    NewWindow,
    /// File → Close Tab / pane (Cmd+W).
    Close,
    /// Edit → Copy (Cmd+C). Also lets macOS route to Services/dictation.
    Copy,
    /// Edit → Paste (Cmd+V).
    Paste,
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
        quit_id: MenuId,
        new_tab_id: MenuId,
        new_window_id: MenuId,
        close_id: MenuId,
        copy_id: MenuId,
        paste_id: MenuId,
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
        let quit = MenuItem::new(
            "Quit Ember",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyQ)),
        );
        let quit_id = quit.id().clone();
        let _ = app_menu.append(&quit);
        let _ = menu.append(&app_menu);

        // File menu.
        let file = Submenu::new("File", true);
        let new_tab = MenuItem::new(
            "New Tab",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyT)),
        );
        let new_tab_id = new_tab.id().clone();
        let new_window = MenuItem::new(
            "New Window",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyN)),
        );
        let new_window_id = new_window.id().clone();
        let close = MenuItem::new(
            "Close",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyW)),
        );
        let close_id = close.id().clone();
        let _ = file.append(&new_tab);
        let _ = file.append(&new_window);
        let _ = file.append(&close);
        let _ = menu.append(&file);

        // Edit menu — Copy/Paste (also exposes Ember to macOS Services).
        let edit = Submenu::new("Edit", true);
        let copy = MenuItem::new(
            "Copy",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyC)),
        );
        let copy_id = copy.id().clone();
        let paste = MenuItem::new(
            "Paste",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyV)),
        );
        let paste_id = paste.id().clone();
        let _ = edit.append(&copy);
        let _ = edit.append(&paste);
        let _ = menu.append(&edit);

        // Window menu — native minimize/zoom (predefined; no app routing needed).
        let window = Submenu::new("Window", true);
        let _ = window.append(&PredefinedMenuItem::minimize(None));
        let _ = window.append(&PredefinedMenuItem::maximize(None));
        let _ = menu.append(&window);

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
            quit_id,
            new_tab_id,
            new_window_id,
            close_id,
            copy_id,
            paste_id,
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
            } else if event.id == menu.quit_id {
                action = Some(MenuAction::Quit);
            } else if event.id == menu.new_tab_id {
                action = Some(MenuAction::NewTab);
            } else if event.id == menu.new_window_id {
                action = Some(MenuAction::NewWindow);
            } else if event.id == menu.close_id {
                action = Some(MenuAction::Close);
            } else if event.id == menu.copy_id {
                action = Some(MenuAction::Copy);
            } else if event.id == menu.paste_id {
                action = Some(MenuAction::Paste);
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
