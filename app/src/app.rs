use adw::prelude::*;
use adw::Application;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use gtk4 as gtk;
use gtk4::prelude::StyleContextExt;
use gio::ApplicationFlags;
use gdk4::Display;

use crate::ui::window::MediaWindow;

const APP_ID: &str = "io.github.daniacosta_dev.AuroraMediaPlayer";

// Embed and register the compiled gresource bundle at startup.
fn init_resources() {
    gio::resources_register_include!("aurora-media.gresource")
        .expect("Failed to register gresources");
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_resource("/io/github/daniacosta_dev/AuroraMediaPlayer/style.css");
    if let Some(display) = Display::default() {
        gtk::StyleContext::add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Override accent color from host gsettings if available.
        // Needed in snap environments where the gnome-46-2404 platform snap's
        // xdg-desktop-portal doesn't forward the host accent color to libadwaita.
        apply_system_accent_color(&display);
    }
}

/// Reads `org.gnome.desktop.interface accent-color` (added in GNOME 47) from
/// gsettings and injects it as a CSS `@define-color` override so the snap
/// matches the host accent color.  Does nothing if the key is absent or the
/// value is the plain default ("default" / empty string).
fn apply_system_accent_color(display: &Display) {
    let Some(source) = gio::SettingsSchemaSource::default() else { return };
    let Some(schema) = source.lookup("org.gnome.desktop.interface", true) else { return };
    if !schema.has_key("accent-color") { return; }

    let settings = gio::Settings::new("org.gnome.desktop.interface");
    let hex = match settings.string("accent-color").as_str() {
        "blue"   => "#3584e4",
        "teal"   => "#2190a4",
        "green"  => "#3a944a",
        "yellow" => "#c88800",
        "orange" => "#ed5b00",
        "red"    => "#e62d42",
        "pink"   => "#d56199",
        "purple" => "#9141ac",
        "slate"  => "#6f8396",
        _        => return, // "default" or unknown — libadwaita already handles it
    };

    let css = format!(
        "@define-color accent_color {hex}; \
         @define-color accent_bg_color {hex}; \
         @define-color accent_fg_color #ffffff;"
    );
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);
    gtk::StyleContext::add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
}

/// Snap confinement may block registering the application's D-Bus name, which
/// a single-instance app needs in order to start, so Snap keeps separate windows.
pub fn single_instance_supported() -> bool {
    std::env::var_os("SNAP").is_none()
}

fn application_flags(allow_multiple_windows: bool, single_instance_supported: bool) -> ApplicationFlags {
    if allow_multiple_windows || !single_instance_supported {
        ApplicationFlags::HANDLES_OPEN | ApplicationFlags::NON_UNIQUE
    } else {
        ApplicationFlags::HANDLES_OPEN
    }
}

/// Brings the existing window to the front, or creates one if there is none.
fn present_window(app: &Application, current: &RefCell<Option<Rc<MediaWindow>>>) -> Rc<MediaWindow> {
    let existing = current.borrow().as_ref().filter(|w| w.is_open()).cloned();
    if let Some(window) = existing {
        window.present();
        return window;
    }

    let window = Rc::new(MediaWindow::new(app));
    window.present();
    // Apply after present() so our provider is installed last and wins
    // over libadwaita's per-window colour providers.
    crate::ui::headerbar::apply_custom_colors();
    *current.borrow_mut() = Some(window.clone());
    window
}

pub struct AuroraMediaApp;

impl AuroraMediaApp {
    pub fn run() -> anyhow::Result<()> {
        init_resources();

        let app = Application::builder()
            .application_id(APP_ID)
            .flags(application_flags(
                crate::ui::headerbar::load_app_settings().allow_multiple_windows,
                single_instance_supported(),
            ))
            .build();

        app.connect_startup(|_| {
            adw::init().expect("Failed to initialize libadwaita");
            // Register the bundled icon so GTK finds it via set_default_icon_name,
            // regardless of whether the app is installed on the system.
            if let Some(display) = Display::default() {
                gtk::IconTheme::for_display(&display)
                    .add_resource_path("/io/github/daniacosta_dev/AuroraMediaPlayer/icons");
            }
            gtk::Window::set_default_icon_name(APP_ID);
            load_css();
            // Apply saved language before any widget is built.
            let saved_lang = crate::ui::headerbar::load_app_settings()
                .language
                .unwrap_or_else(|| "en".into());
            crate::i18n::set(crate::i18n::Lang::from_code(&saved_lang));
        });

        let current_window: Rc<RefCell<Option<Rc<MediaWindow>>>> = Rc::default();

        app.connect_activate({
            let current_window = current_window.clone();
            move |app| {
                present_window(app, &current_window);
            }
        });

        // Handle file opens from CLI or file manager, including those forwarded
        // by later launches when running as a single instance.
        app.connect_open(move |app, files, _hint| {
            let window = present_window(app, &current_window);
            let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
            window.open_files(&paths);
        });

        let exit_code = app.run();
        if exit_code != glib::ExitCode::SUCCESS {
            anyhow::bail!("Application exited with error");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_is_the_default() {
        assert!(!application_flags(false, true).contains(ApplicationFlags::NON_UNIQUE));
    }

    #[test]
    fn the_setting_allows_multiple_windows() {
        assert!(application_flags(true, true).contains(ApplicationFlags::NON_UNIQUE));
    }

    #[test]
    fn snap_always_keeps_separate_windows() {
        assert!(application_flags(false, false).contains(ApplicationFlags::NON_UNIQUE));
    }

    #[test]
    fn opening_files_is_always_handled() {
        for (multiple, supported) in [(false, true), (true, true), (false, false)] {
            assert!(application_flags(multiple, supported).contains(ApplicationFlags::HANDLES_OPEN));
        }
    }
}
