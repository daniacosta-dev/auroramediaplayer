mod app;
mod i18n;
mod mpris;
mod player;
mod state;
mod thumbnails;
mod ui;
mod library;

use app::AuroraMediaApp;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Install dev-time icon + .desktop so the compositor can find them
    // before the Wayland surface is created.  In a snap/flatpak the source
    // asset won't exist so this block is a no-op.
    install_dev_assets();

    AuroraMediaApp::run()
}

fn install_dev_assets() {
    let icon_src = std::env::current_dir()
        .unwrap_or_default()
        .join("app/src/assets/aurora_media_player_icon.svg");

    if !icon_src.exists() {
        return;
    }

    let Some(data_dir) = dirs::data_dir() else { return };

    // Icon → ~/.local/share/icons/hicolor/scalable/apps/
    let icon_dir = data_dir.join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icon_dir).ok();
    std::fs::copy(
        &icon_src,
        icon_dir.join("io.github.daniacosta_dev.AuroraMediaPlayer.svg"),
    )
    .ok();

    // .desktop → ~/.local/share/applications/
    // GNOME Shell uses this to resolve the window app-id → icon name.
    // We must use the absolute path to the current binary in Exec= so that
    // GIO can verify the executable exists; GIO silently drops desktop files
    // whose Exec= command is not found in PATH.
    let exe_path = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("aurora-media"));
    let apps_dir = data_dir.join("applications");
    std::fs::create_dir_all(&apps_dir).ok();
    std::fs::write(
        apps_dir.join("io.github.daniacosta_dev.AuroraMediaPlayer.desktop"),
        format!(
            "[Desktop Entry]\n\
             Name=Aurora Media Player\n\
             Exec={} %U\n\
             Icon=io.github.daniacosta_dev.AuroraMediaPlayer\n\
             Type=Application\n\
             Categories=AudioVideo;Player;\n\
             MimeType=video/mp4;video/x-matroska;audio/mpeg;audio/flac;\n",
            exe_path.display()
        ),
    )
    .ok();

    // Refresh caches so GNOME Shell's AppSystem and GTK icon theme pick up the
    // newly installed files without requiring a logout.
    std::process::Command::new("update-desktop-database")
        .arg(apps_dir)
        .status()
        .ok();
    // gtk-update-icon-cache needs the theme root (~/.local/share/icons/hicolor),
    // NOT the scalable/apps subdir. Passing the wrong path silently did nothing.
    let hicolor_dir = data_dir.join("icons/hicolor");
    std::process::Command::new("gtk-update-icon-cache")
        .args(["-f", "-t", &hicolor_dir.to_string_lossy()])
        .status()
        .ok();
}
