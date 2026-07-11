# Linux desktop integration

Freedesktop assets so Ember shows up in the GNOME app grid, KDE launcher,
and docks: a desktop entry plus hicolor icons. The window's X11 `WM_CLASS`
and Wayland `app_id` are both `ember-term` (set in `ember-platform`), which
matches the desktop file's basename and `StartupWMClass`, so running windows
group under the launcher icon.

Distro packages should install these into the standard system paths:

- `ember-term.desktop` -> `/usr/share/applications/`
- `icons/hicolor/<size>/apps/ember-term.png` -> `/usr/share/icons/hicolor/<size>/apps/`

For a user-local install (no sudo), from the repository root:

```sh
mkdir -p ~/.local/share/applications ~/.local/share/icons
cp extra/linux/ember-term.desktop ~/.local/share/applications/
cp -r extra/linux/icons/hicolor ~/.local/share/icons/
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

`Exec=ember-term` assumes the binary is on `PATH`. If it is not (for example
a Homebrew prefix that GNOME does not search), edit the copied desktop
file's `Exec=` lines to the absolute path of `ember-term`.
