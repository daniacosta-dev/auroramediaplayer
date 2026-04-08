use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;

use adw::{self, HeaderBar};
use adw::prelude::*;
use gtk4::{self as gtk, Button, ToggleButton};
use gtk4::prelude::*;
use gio;

use crate::i18n::t;
use crate::state::SharedState;

// ── Custom colour provider ─────────────────────────────────────────────────────
// Kept in a thread-local so we can remove the old provider before applying new ones.
thread_local! {
    static CUSTOM_COLOR_PROVIDER: RefCell<Option<gtk::CssProvider>> = RefCell::new(None);
}

/// Re-reads `accent_color` / `window_bg_color` from settings and installs a
/// CSS provider that overrides libadwaita's colour tokens.  Calling this with
/// both fields set to `None` (i.e. "use system") removes any previous override.
pub fn apply_custom_colors() {
    let Some(display) = gdk4::Display::default() else { return };
    let settings = load_app_settings();

    log::debug!(
        "apply_custom_colors: accent={:?} bg={:?}",
        settings.accent_color,
        settings.window_bg_color
    );

    // Remove previous custom provider (if any).
    CUSTOM_COLOR_PROVIDER.with(|cell| {
        let old = cell.borrow_mut().take();
        if let Some(old_provider) = old {
            gtk::StyleContext::remove_provider_for_display(&display, &old_provider);
        }
    });

    let mut css = String::new();

    if let Some(accent) = &settings.accent_color {
        // @define-color for libadwaita ≤ 1.5; CSS custom properties for 1.6+
        css.push_str(&format!(
            "@define-color accent_color {accent}; \
             @define-color accent_bg_color {accent}; \
             @define-color accent_fg_color #ffffff; \
             * {{ --accent-color: {accent}; --accent-bg-color: {accent}; --accent-fg-color: #ffffff; }}"
        ));
    }
    if let Some(bg) = &settings.window_bg_color {
        css.push_str(&format!(
            "@define-color window_bg_color {bg}; \
             @define-color view_bg_color {bg}; \
             @define-color headerbar_bg_color {bg}; \
             @define-color headerbar_backdrop_color {bg}; \
             @define-color sidebar_bg_color {bg}; \
             @define-color card_bg_color {bg}; \
             @define-color popover_bg_color {bg}; \
             @define-color dialog_bg_color {bg}; \
             * {{ --window-bg-color: {bg}; --view-bg-color: {bg}; \
                  --headerbar-bg-color: {bg}; --sidebar-bg-color: {bg}; \
                  --card-bg-color: {bg}; --popover-bg-color: {bg}; }}"
        ));
    }
    if let Some(fg) = &settings.text_color {
        // Use the CSS `color` property on leaf elements only — NOT on containers.
        // @window_fg_color is referenced in style.css for backgrounds/borders (seekbar
        // trough, controls bar border, hover highlights) and must NOT be overridden.
        css.push_str(&format!(
            "label {{ color: {fg}; }} \
             image {{ color: {fg}; }} \
             scale:not(.seekbar) slider {{ background-color: {fg}; border-color: {fg}; }}"
        ));
    }

    if !css.is_empty() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&css);
        // Use USER priority (800) — above APPLICATION (600) and any libadwaita
        // internal accent-colour providers which run at SETTINGS+1 (401).
        gtk::StyleContext::add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
        CUSTOM_COLOR_PROVIDER.with(|cell| {
            *cell.borrow_mut() = Some(provider);
        });
        log::debug!("apply_custom_colors: provider installed, CSS={css}");
    }
}

// ── Recent files persistence ──────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RecentEntry {
    path: String,
    title: String,
}

fn recent_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("aurora-media").join("recent.json"))
}

fn load_recent() -> Vec<RecentEntry> {
    let Some(path) = recent_path() else { return vec![] };
    let Ok(data) = std::fs::read_to_string(path) else { return vec![] };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_recent(entries: &[RecentEntry]) {
    let Some(path) = recent_path() else { return };
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).ok(); }
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        std::fs::write(path, json).ok();
    }
}

pub struct MediaHeaderBar {
    header: HeaderBar,
    pub playlist_btn: ToggleButton,
    pub push_recent_fn: Rc<dyn Fn(&std::path::Path, &str)>,
    pub window_title: adw::WindowTitle,
    /// Exposed for keyboard shortcuts — activate() triggers the connected handler.
    pub open_file_btn: Button,
    pub open_url_btn: Button,
    pub open_sub_btn: Button,
    pub settings_btn: Button,
    /// Badge shown when the current content has HDR color metadata.
    /// Clicking it opens Settings scrolled to the Video section.
    pub hdr_badge: Button,
    // ── Labels kept for live relabelling ─────────────────────────────────
    pub file_btn: Button,
    open_file_lbl: gtk::Label,
    open_url_lbl: gtk::Label,
    open_sub_lbl: gtk::Label,
    recent_row_lbl: gtk::Label,
    screenshot_folder_lbl: gtk::Label,
    report_issue_lbl: gtk::Label,
    quit_lbl: gtk::Label,
}

impl MediaHeaderBar {
    /// `on_open_file` is called when the user picks a file via the open dialog.
    /// The callback receives the path and is responsible for handling both
    /// regular media files and M3U playlists (wired in window.rs).
    ///
    /// `on_url_playlist` is called when the user confirms a URL playlist.
    /// Items are `(display_title, url)` pairs — titles come from `#EXTINF` when available.
    ///
    /// `on_open_subtitle` is called when the user picks a subtitle file.
    ///
    /// `on_open_recent` is called when the user clicks a recent file entry.
    pub fn new(
        _state: SharedState,
        on_open_file: impl Fn(PathBuf) + 'static,
        on_url_playlist: impl Fn(Option<String>, Vec<(String, String)>) + 'static,
        on_open_subtitle: impl Fn(PathBuf) + 'static,
        on_open_recent: impl Fn(PathBuf) + 'static,
        on_ui_mode_change: Rc<dyn Fn(&str)>,
        on_language_change: Rc<dyn Fn()>,
        on_tone_mapping_change: Rc<dyn Fn(&str)>,
    ) -> Self {
        let header = HeaderBar::new();

        // ── File menu ─────────────────────────────────────────────────────────────
        let file_btn = Button::builder().label(t("File")).build();

        // Main popover — autohide closes it when clicking outside
        let file_popover = gtk::Popover::new();
        file_popover.set_autohide(true);
        file_popover.set_has_arrow(false);
        file_popover.add_css_class("file-menu-popover");
        file_popover.set_parent(&file_btn);

        let menu_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(4).margin_bottom(4)
            .build();
        file_popover.set_child(Some(&menu_box));

        // Helper: flat button styled as a menu item — returns (Button, inner Label).
        let mk_item = |icon: &str, label: &str| -> (Button, gtk::Label) {
            let btn = Button::new();
            btn.add_css_class("flat");
            btn.add_css_class("file-menu-item");
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_start(8).margin_end(16)
                .margin_top(2).margin_bottom(2)
                .build();
            row.append(&gtk::Image::from_icon_name(icon));
            let lbl = gtk::Label::builder()
                .label(label)
                .halign(gtk::Align::Start)
                .hexpand(true)
                .build();
            row.append(&lbl);
            btn.set_child(Some(&row));
            (btn, lbl)
        };

        let (open_file_btn, open_file_lbl) = mk_item("document-open-symbolic",       t("Open File…"));
        let (open_url_btn,  open_url_lbl)  = mk_item("insert-link-symbolic",          t("Open URL or Playlist…"));
        let (open_sub_btn,  open_sub_lbl)  = mk_item("media-view-subtitles-symbolic", t("Load Subtitle File…"));

        // Recent Files row — right-arrow indicates a submenu
        let (recent_row_btn, recent_row_lbl) = {
            let btn = Button::new();
            btn.add_css_class("flat");
            btn.add_css_class("file-menu-item");
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_start(8).margin_end(16)
                .margin_top(2).margin_bottom(2)
                .build();
            row.append(&gtk::Image::from_icon_name("document-open-recent-symbolic"));
            let lbl = gtk::Label::builder()
                .label(t("Recent Files"))
                .halign(gtk::Align::Start)
                .hexpand(true)
                .build();
            row.append(&lbl);
            row.append(&gtk::Image::from_icon_name("go-next-symbolic"));
            btn.set_child(Some(&row));
            (btn, lbl)
        };

        let (screenshot_folder_btn, screenshot_folder_lbl) = mk_item("camera-photo-symbolic", t("Open Pictures Folder"));
        let (report_issue_btn, report_issue_lbl)           = mk_item("help-about-symbolic",     t("Report Issue"));
        report_issue_btn.set_cursor_from_name(Some("pointer"));
        let (quit_btn, quit_lbl) = mk_item("application-exit-symbolic", t("Close"));
        quit_btn.set_cursor_from_name(Some("pointer"));

        menu_box.append(&open_file_btn);
        menu_box.append(&open_url_btn);
        menu_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu_box.append(&open_sub_btn);
        menu_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu_box.append(&recent_row_btn);
        menu_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu_box.append(&screenshot_folder_btn);
        menu_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu_box.append(&report_issue_btn);
        menu_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu_box.append(&quit_btn);

        // ── Recent sub-popover ─────────────────────────────────────────────────
        // Parented to recent_row_btn (inside file_popover) so autohide on the
        // main popover correctly considers clicks here as "inside the cascade".
        let recent_sub = gtk::Popover::new();
        recent_sub.set_autohide(false);
        recent_sub.set_has_arrow(false);
        recent_sub.add_css_class("file-menu-popover");
        recent_sub.set_parent(&recent_row_btn);
        recent_sub.set_position(gtk::PositionType::Right);

        let recent_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(4).margin_bottom(4)
            .build();
        recent_sub.set_child(Some(&recent_box));

        // ── Hover / auto-close logic ───────────────────────────────────────────
        // - Main popover: stays open; closes on click outside or click "File".
        // - Recent sub:   opens on hover over "Recent Files" row;
        //                 closes only when hovering another menu item.

        // Hover Recent Files row → popup sub
        {
            let rs = recent_sub.downgrade();
            let mc = gtk::EventControllerMotion::new();
            mc.connect_enter(move |_, _, _| {
                if let Some(r) = rs.upgrade() { r.popup(); }
            });
            recent_row_btn.add_controller(mc);
        }

        // Quit → close the window
        {
            let fp = file_popover.downgrade();
            quit_btn.connect_clicked(move |btn| {
                if let Some(f) = fp.upgrade() { f.popdown(); }
                if let Some(win) = btn.root().and_downcast::<gtk::Window>() {
                    win.close();
                }
            });
        }

        // Hover any other item → close sub
        for btn in [&open_file_btn, &open_url_btn, &open_sub_btn, &screenshot_folder_btn, &report_issue_btn, &quit_btn] {
            let rs = recent_sub.downgrade();
            let mc = gtk::EventControllerMotion::new();
            mc.connect_enter(move |_, _, _| {
                if let Some(r) = rs.upgrade() { r.popdown(); }
            });
            btn.add_controller(mc);
        }

        // Report Issue → open feedback dialog
        {
            let fp = file_popover.downgrade();
            report_issue_btn.connect_clicked(move |btn| {
                if let Some(f) = fp.upgrade() { f.popdown(); }
                let Some(parent) = btn.root().and_downcast::<gtk::Window>() else { return };
                show_report_dialog(&parent);
            });
        }

        // Main popover closed → also close sub
        {
            let rs = recent_sub.downgrade();
            file_popover.connect_closed(move |_| {
                if let Some(r) = rs.upgrade() { r.popdown(); }
            });
        }

        // File button click → toggle main popover
        {
            let fp = file_popover.downgrade();
            file_btn.connect_clicked(move |_| {
                if let Some(f) = fp.upgrade() {
                    if f.is_visible() { f.popdown(); } else { f.popup(); }
                }
            });
        }

        // Align left edge of popover with the window edge
        {
            let btn_w = file_btn.downgrade();
            file_popover.connect_show(move |popover| {
                if let Some(btn) = btn_w.upgrade() {
                    if let Some(root) = btn.root() {
                        if let Some((x, _)) = btn.translate_coordinates(&root, 0.0, 0.0) {
                            let btn_center = x + btn.width() as f64 / 2.0;
                            // 140 = half of CSS min-width 280px
                            let offset = ((140.0 - btn_center) as i32).max(0);
                            popover.set_offset(offset, 0);
                        }
                    }
                }
            });
        }

        // file_btn is NOT packed here — window.rs moves it between headerbars
        // depending on which view (library vs player) is currently visible.

        // ── Window title (centered, title + subtitle) ─────────────────────
        let window_title = adw::WindowTitle::builder()
            .title("Aurora Media Player")
            .build();
        header.set_title_widget(Some(&window_title));

        // ── Playlist toggle ───────────────────────────────────────────────
        let playlist_btn = ToggleButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text(t("Toggle playlist"))
            .build();
        header.pack_end(&playlist_btn);

        // ── HDR badge ─────────────────────────────────────────────────────
        let hdr_badge = Button::builder()
            .label("HDR")
            .visible(false)
            .css_classes(["hdr-badge", "flat"])
            .cursor(&gdk4::Cursor::from_name("pointer", None).unwrap())
            .build();
        header.pack_end(&hdr_badge);

        // ── Settings button ───────────────────────────────────────────────
        let settings_btn = Button::builder()
            .icon_name("preferences-system-symbolic")
            .tooltip_text(t("Settings"))
            .build();
        header.pack_end(&settings_btn);

        // Apply persisted color scheme immediately on construction.
        let saved_scheme = load_app_settings()
            .color_scheme
            .unwrap_or_else(|| "system".into());
        adw::StyleManager::default().set_color_scheme(adw_scheme(&saved_scheme));

        // ── Wire: screenshot folder ───────────────────────────────────────
        {
            let fp = file_popover.downgrade();
            screenshot_folder_btn.connect_clicked(move |_| {
                if let Some(f) = fp.upgrade() { f.popdown(); }
                if let Some(pic) = dirs::picture_dir() {
                    let uri = gio::File::for_path(&pic).uri();
                    gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>).ok();
                }
            });
        }

        // ── Pointer cursor on header buttons ──────────────────────────────
        for w in [
            file_btn.upcast_ref::<gtk::Widget>(),
            playlist_btn.upcast_ref(),
            settings_btn.upcast_ref(),
            open_file_btn.upcast_ref(),
            open_url_btn.upcast_ref(),
            open_sub_btn.upcast_ref(),
            recent_row_btn.upcast_ref(),
            screenshot_folder_btn.upcast_ref(),
        ] {
            w.set_cursor_from_name(Some("pointer"));
        }

        // ── Wire: settings ────────────────────────────────────────────────
        settings_btn.connect_clicked({
            let cb = on_ui_mode_change.clone();
            let lang_cb = on_language_change.clone();
            let tm_cb = on_tone_mapping_change.clone();
            move |btn| {
                let Some(parent) = btn.root().and_downcast::<gtk::Window>() else { return };
                // Toggle: if settings is already open, close it.
                for w in gtk::Window::list_toplevels() {
                    if let Some(win) = w.downcast_ref::<gtk::Window>() {
                        if win.widget_name() == "settings-dialog"
                            && win.transient_for().as_ref() == Some(&parent)
                        {
                            win.close();
                            return;
                        }
                    }
                }
                show_settings_dialog(&parent, cb.clone(), lang_cb.clone(), tm_cb.clone(), false);
            }
        });

        // ── Wire: HDR badge → open Settings at Video section ─────────────
        hdr_badge.connect_clicked({
            let cb = on_ui_mode_change.clone();
            let lang_cb = on_language_change.clone();
            let tm_cb = on_tone_mapping_change.clone();
            move |btn| {
                let Some(parent) = btn.root().and_downcast::<gtk::Window>() else { return };
                for w in gtk::Window::list_toplevels() {
                    if let Some(win) = w.downcast_ref::<gtk::Window>() {
                        if win.widget_name() == "settings-dialog"
                            && win.transient_for().as_ref() == Some(&parent)
                        {
                            win.close();
                            return;
                        }
                    }
                }
                show_settings_dialog(&parent, cb.clone(), lang_cb.clone(), tm_cb.clone(), true);
            }
        });

        // ── Wire: open file ───────────────────────────────────────────────
        {
            let on_open_file = Rc::new(on_open_file);
            let fp = file_popover.downgrade();
            let btn_w = file_btn.downgrade();
            open_file_btn.connect_clicked(move |_| {
                if let Some(f) = fp.upgrade() { f.popdown(); }
                let on_open_file = on_open_file.clone();
                let media_filter = gtk::FileFilter::new();
                media_filter.set_name(Some("Media files"));
                for ext in [
                    "mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v", "ts",
                    "mp3", "flac", "ogg", "opus", "aac", "m4a", "wav", "wma",
                    "m3u", "m3u8",
                ] { media_filter.add_suffix(ext); }
                let video_filter = gtk::FileFilter::new();
                video_filter.set_name(Some("Video files"));
                for ext in ["mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v", "ts"] {
                    video_filter.add_suffix(ext);
                }
                let audio_filter = gtk::FileFilter::new();
                audio_filter.set_name(Some("Audio files"));
                for ext in ["mp3", "flac", "ogg", "opus", "aac", "m4a", "wav", "wma"] {
                    audio_filter.add_suffix(ext);
                }
                let playlist_filter = gtk::FileFilter::new();
                playlist_filter.set_name(Some("Playlist files"));
                for ext in ["m3u", "m3u8"] { playlist_filter.add_suffix(ext); }
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&media_filter);
                filters.append(&video_filter);
                filters.append(&audio_filter);
                filters.append(&playlist_filter);
                let dialog = gtk::FileDialog::builder()
                    .title(t("Open Media File")).modal(true).filters(&filters).build();
                let parent = btn_w.upgrade().and_then(|b| b.root()).and_downcast::<gtk::Window>();
                dialog.open(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                    if let Ok(file) = result {
                        if let Some(path) = file.path() { on_open_file(path); }
                    }
                });
            });
        }

        // ── Wire: open URL ────────────────────────────────────────────────
        {
            let on_url_playlist = Rc::new(on_url_playlist);
            let fp = file_popover.downgrade();
            let btn_w = file_btn.downgrade();
            open_url_btn.connect_clicked(move |_| {
                if let Some(f) = fp.upgrade() { f.popdown(); }
                let Some(parent) = btn_w.upgrade()
                    .and_then(|b| b.root())
                    .and_downcast::<gtk::Window>() else { return };
                show_url_playlist_dialog(&parent, on_url_playlist.clone());
            });
        }

        // ── Wire: subtitle ────────────────────────────────────────────────
        {
            let on_open_subtitle = Rc::new(on_open_subtitle);
            let fp = file_popover.downgrade();
            let btn_w = file_btn.downgrade();
            open_sub_btn.connect_clicked(move |_| {
                if let Some(f) = fp.upgrade() { f.popdown(); }
                let on_open_subtitle = on_open_subtitle.clone();
                let sub_filter = gtk::FileFilter::new();
                sub_filter.set_name(Some("Subtitle files"));
                for ext in ["srt", "ass", "ssa", "sub", "vtt", "sup"] {
                    sub_filter.add_suffix(ext);
                }
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&sub_filter);
                let dialog = gtk::FileDialog::builder()
                    .title(t("Open Subtitle File")).modal(true).filters(&filters).build();
                let parent = btn_w.upgrade().and_then(|b| b.root()).and_downcast::<gtk::Window>();
                dialog.open(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                    if let Ok(file) = result {
                        if let Some(path) = file.path() { on_open_subtitle(path); }
                    }
                });
            });
        }

        // ── Recent files ──────────────────────────────────────────────────
        let on_open_recent = Rc::new(on_open_recent);

        // Slot filled after populate_recent is defined, so button click handlers
        // inside populate_recent can call populate_recent without a circular dep.
        let repopulate_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> =
            Rc::new(RefCell::new(None));

        let populate_recent: Rc<dyn Fn()> = {
            let rbox = recent_box.downgrade();
            let fp   = file_popover.downgrade();
            let rs   = recent_sub.downgrade();
            let on_open_recent = on_open_recent.clone();
            let repopulate_slot = repopulate_slot.clone();
            Rc::new(move || {
                let Some(rbox) = rbox.upgrade() else { return };
                while let Some(child) = rbox.first_child() { rbox.remove(&child); }
                let entries = load_recent();
                if entries.is_empty() {
                    let lbl = gtk::Label::builder()
                        .label(t("No recent files"))
                        .margin_start(12).margin_end(12)
                        .margin_top(6).margin_bottom(6)
                        .css_classes(["dim-label"])
                        .build();
                    rbox.append(&lbl);
                } else {
                    for entry in entries {
                        // Outer row — scopes the hover state for the remove button.
                        let outer = gtk::Box::builder()
                            .orientation(gtk::Orientation::Horizontal)
                            .spacing(0)
                            .css_classes(["recent-row"])
                            .build();

                        let btn = Button::new();
                        btn.add_css_class("flat");
                        btn.add_css_class("file-menu-item");
                        btn.set_hexpand(true);
                        let row = gtk::Box::builder()
                            .orientation(gtk::Orientation::Horizontal)
                            .spacing(8)
                            .margin_start(8).margin_end(8)
                            .margin_top(2).margin_bottom(2)
                            .build();
                        row.append(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
                        let lbl = gtk::Label::builder()
                            .label(&entry.title)
                            .halign(gtk::Align::Start)
                            .hexpand(true)
                            .ellipsize(gtk4::pango::EllipsizeMode::End)
                            .build();
                        row.append(&lbl);
                        btn.set_child(Some(&row));
                        btn.set_cursor_from_name(Some("pointer"));
                        let path = std::path::PathBuf::from(&entry.path);
                        let entry_path_str = entry.path.clone();
                        let entry_title_str = entry.title.clone();
                        let on_open_c = on_open_recent.clone();
                        let fp_w = fp.clone();
                        let rs_w = rs.clone();
                        let repopulate_w = repopulate_slot.clone();
                        btn.connect_clicked(move |_| {
                            if let Some(r) = rs_w.upgrade() { r.popdown(); }
                            if let Some(f) = fp_w.upgrade() { f.popdown(); }
                            // Move this entry to the top of the list immediately.
                            let mut entries = load_recent();
                            entries.retain(|e| e.path != entry_path_str);
                            entries.insert(0, RecentEntry {
                                path: entry_path_str.clone(),
                                title: entry_title_str.clone(),
                            });
                            entries.truncate(8);
                            save_recent(&entries);
                            if let Some(f) = &*repopulate_w.borrow() { f(); }
                            on_open_c(path.clone());
                        });

                        // Remove button — revealed on hover via CSS.
                        let remove_btn = Button::from_icon_name("window-close-symbolic");
                        remove_btn.add_css_class("flat");
                        remove_btn.add_css_class("circular");
                        remove_btn.add_css_class("recent-remove-btn");
                        remove_btn.set_valign(gtk::Align::Center);
                        remove_btn.set_cursor_from_name(Some("pointer"));
                        remove_btn.set_tooltip_text(Some(t("Remove from recents")));
                        let entry_path = entry.path.clone();
                        let rbox_w = rbox.clone(); // gtk::Box — already upgraded in this closure
                        let outer_w: glib::WeakRef<gtk::Box> = outer.downgrade();
                        remove_btn.connect_clicked(move |_| {
                            let mut entries = load_recent();
                            entries.retain(|e| e.path != entry_path);
                            save_recent(&entries);
                            if let Some(w) = outer_w.upgrade() { rbox_w.remove(&w); }
                            if rbox_w.first_child().is_none() {
                                let lbl = gtk::Label::builder()
                                    .label("No recent files")
                                    .margin_start(12).margin_end(12)
                                    .margin_top(6).margin_bottom(6)
                                    .css_classes(["dim-label"])
                                    .build();
                                rbox_w.append(&lbl);
                            }
                        });

                        outer.append(&btn);
                        outer.append(&remove_btn);
                        rbox.append(&outer);
                    }
                }
            })
        };
        *repopulate_slot.borrow_mut() = Some(populate_recent.clone());
        populate_recent();

        // ── push_recent_fn ────────────────────────────────────────────────
        let populate_recent_c = populate_recent.clone();
        let push_recent_fn: Rc<dyn Fn(&std::path::Path, &str)> = Rc::new(
            move |path: &std::path::Path, title: &str| {
                // M3U playlists are never added to recent files.
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext.eq_ignore_ascii_case("m3u") || ext.eq_ignore_ascii_case("m3u8") {
                    return;
                }
                let mut entries = load_recent();
                let path_str = path.to_string_lossy().to_string();
                entries.retain(|e| e.path != path_str);
                entries.insert(0, RecentEntry { path: path_str, title: title.to_string() });
                entries.truncate(8);
                save_recent(&entries);
                populate_recent_c();
            },
        );

        Self {
            header, playlist_btn, push_recent_fn, window_title, hdr_badge,
            open_file_btn, open_url_btn, open_sub_btn, settings_btn,
            file_btn, open_file_lbl, open_url_lbl, open_sub_lbl,
            recent_row_lbl, screenshot_folder_lbl, report_issue_lbl, quit_lbl,
        }
    }

    pub fn widget(&self) -> &HeaderBar {
        &self.header
    }

    pub fn relabel(&self) {
        self.file_btn.set_label(t("File"));
        self.open_file_lbl.set_label(t("Open File…"));
        self.open_url_lbl.set_label(t("Open URL or Playlist…"));
        self.open_sub_lbl.set_label(t("Load Subtitle File…"));
        self.recent_row_lbl.set_label(t("Recent Files"));
        self.screenshot_folder_lbl.set_label(t("Open Pictures Folder"));
        self.report_issue_lbl.set_label(t("Report Issue"));
        self.quit_lbl.set_label(t("Close"));
        self.playlist_btn.set_tooltip_text(Some(t("Toggle playlist")));
        self.settings_btn.set_tooltip_text(Some(t("Settings")));
    }

    /// Update the header title/subtitle with the currently playing track.
    /// Pass `None` for `title` when idle to reset to "Aurora".
    pub fn set_now_playing(&self, title: Option<&str>, artist: &str) {
        match title {
            None => {
                self.window_title.set_title("Aurora Media Player");
                self.window_title.set_subtitle("");
            }
            Some(raw) => {
                let is_url_noise = raw.starts_with("http://")
                    || raw.starts_with("https://")
                    || (raw.contains('?') && raw.contains('=') && !raw.contains(' '));
                let display = if is_url_noise || raw.is_empty() { "Loading…" } else { raw };
                self.window_title.set_title(display);
                self.window_title.set_subtitle(artist);
            }
        }
    }
}

// ── Saved URL playlists persistence ──────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SavedPlaylists {
    playlists: Vec<SavedPlaylist>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct SavedPlaylist {
    name: String,
    urls: Vec<String>,
}

fn playlists_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("aurora-media").join("url-playlists.json"))
}

fn load_saved_playlists() -> SavedPlaylists {
    let Some(path) = playlists_path() else { return Default::default() };
    let Ok(data) = std::fs::read_to_string(path) else { return Default::default() };
    serde_json::from_str(&data).unwrap_or_default()
}

fn write_saved_playlists(saved: &SavedPlaylists) {
    let Some(path) = playlists_path() else { return };
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).ok(); }
    if let Ok(json) = serde_json::to_string_pretty(saved) {
        std::fs::write(path, json).ok();
    }
}

// ── App settings persistence ──────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct AppSettings {
    /// "system" | "light" | "dark"
    pub color_scheme: Option<String>,
    /// "en" | "es" — UI language (default: "en")
    pub language: Option<String>,
    #[serde(default = "default_volume")]
    pub volume: f64,
    #[serde(default)]
    pub muted: bool,
    /// "floating" (overlay, auto-hide) | "fixed" (always-visible bottom bar)
    pub ui_mode: Option<String>,
    /// Custom accent colour as a CSS colour string (e.g. "#ed5b00"), None = system
    pub accent_color: Option<String>,
    /// Custom window background colour as a CSS colour string, None = system
    pub window_bg_color: Option<String>,
    /// Custom text/icon foreground colour as a CSS colour string, None = system
    pub text_color: Option<String>,
    /// HDR tone mapping algorithm: "bt.2446a" | "reinhard" | "hable" | "clip"
    /// None = use default ("bt.2446a")
    pub tone_mapping: Option<String>,
}

fn default_volume() -> f64 { 100.0 }

fn settings_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("aurora-media").join("settings.json"))
}

pub fn load_app_settings() -> AppSettings {
    let Some(path) = settings_path() else { return Default::default() };
    let Ok(data) = std::fs::read_to_string(path) else { return Default::default() };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_app_settings(s: &AppSettings) {
    let Some(path) = settings_path() else { return };
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).ok(); }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        std::fs::write(path, json).ok();
    }
}

fn adw_scheme(key: &str) -> adw::ColorScheme {
    match key {
        "light" => adw::ColorScheme::ForceLight,
        "dark"  => adw::ColorScheme::ForceDark,
        _       => adw::ColorScheme::Default,
    }
}

// ── Report / Feedback dialog ──────────────────────────────────────────────────

fn show_report_dialog(parent: &gtk::Window) {
    let dialog = adw::Window::builder()
        .title(t("Send Feedback"))
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .resizable(false)
        .build();
    dialog.set_widget_name("report-dialog");

    let header = adw::HeaderBar::new();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(16)
        .margin_bottom(28)
        .margin_start(24)
        .margin_end(24)
        .halign(gtk::Align::Center)
        .build();

    let title_lbl = gtk::Label::builder()
        .label(t("Help us improve Aurora"))
        .css_classes(["title-2"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .halign(gtk::Align::Center)
        .margin_bottom(4)
        .build();

    let desc_lbl = gtk::Label::builder()
        .label(t("We want Aurora Media Player to be the best it can be. Your feedback and bug reports are incredibly valuable — share your thoughts on Discord or open an issue on GitHub."))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .halign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .margin_bottom(12)
        .build();
    desc_lbl.set_max_width_chars(48);

    let btn_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .halign(gtk::Align::Center)
        .build();

    // ── Discord button ────────────────────────────────────────────────────
    let discord_btn = gtk::Button::builder()
        .css_classes(["flat"])
        .build();
    discord_btn.set_cursor_from_name(Some("pointer"));
    let discord_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(24)
        .margin_end(24)
        .halign(gtk::Align::Center)
        .build();
    let discord_img = gtk::Image::builder()
        .resource("/io/github/daniacosta_dev/AuroraMediaPlayer/icons/scalable/apps/discord-logo.svg")
        .pixel_size(48)
        .build();
    let discord_lbl = gtk::Label::builder()
        .label("Discord")
        .css_classes(["heading"])
        .build();
    discord_inner.append(&discord_img);
    discord_inner.append(&discord_lbl);
    discord_btn.set_child(Some(&discord_inner));
    {
        let d = dialog.downgrade();
        discord_btn.connect_clicked(move |_| {
            if let Some(dlg) = d.upgrade() { dlg.close(); }
            gio::AppInfo::launch_default_for_uri(
                "https://discord.gg/E6NQVFAY",
                gio::AppLaunchContext::NONE,
            ).ok();
        });
    }

    // ── GitHub Issues button ──────────────────────────────────────────────
    let github_btn = gtk::Button::builder()
        .css_classes(["flat"])
        .build();
    github_btn.set_cursor_from_name(Some("pointer"));
    let github_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(24)
        .margin_end(24)
        .halign(gtk::Align::Center)
        .build();
    let github_img = gtk::Image::builder()
        .resource("/io/github/daniacosta_dev/AuroraMediaPlayer/icons/scalable/apps/github-logo.svg")
        .pixel_size(48)
        .build();
    let github_lbl = gtk::Label::builder()
        .label(t("GitHub Issues"))
        .css_classes(["heading"])
        .build();
    github_inner.append(&github_img);
    github_inner.append(&github_lbl);
    github_btn.set_child(Some(&github_inner));
    {
        let d = dialog.downgrade();
        github_btn.connect_clicked(move |_| {
            if let Some(dlg) = d.upgrade() { dlg.close(); }
            gio::AppInfo::launch_default_for_uri(
                "https://github.com/daniacosta-dev/aurora-media-player/issues",
                gio::AppLaunchContext::NONE,
            ).ok();
        });
    }

    btn_box.append(&discord_btn);
    btn_box.append(&github_btn);

    body.append(&title_lbl);
    body.append(&desc_lbl);
    body.append(&btn_box);

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    root.append(&header);
    root.append(&body);

    dialog.set_content(Some(&root));
    dialog.present();
}

// ── Settings dialog ───────────────────────────────────────────────────────────

fn show_settings_dialog(parent: &gtk::Window, on_ui_mode_change: Rc<dyn Fn(&str)>, on_language_change: Rc<dyn Fn()>, on_tone_mapping_change: Rc<dyn Fn(&str)>, scroll_to_video: bool) {
    let dialog = adw::Window::builder()
        .title(t("Settings"))
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(520)
        .resizable(false)
        .build();
    dialog.set_widget_name("settings-dialog");

    let header = adw::HeaderBar::new();

    // ── Appearance section ────────────────────────────────────────────────
    let appearance_lbl = gtk::Label::builder()
        .label(t("Appearance"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .margin_top(18)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    let saved_scheme = load_app_settings()
        .color_scheme
        .unwrap_or_else(|| "system".into());

    let theme_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();

    let theme_row = adw::ActionRow::builder()
        .title(t("Theme"))
        .build();

    let btn_system = gtk::ToggleButton::builder()
        .label(t("System"))
        .active(saved_scheme == "system")
        .valign(gtk::Align::Center)
        .build();
    let btn_light = gtk::ToggleButton::builder()
        .label(t("Light"))
        .active(saved_scheme == "light")
        .group(&btn_system)
        .valign(gtk::Align::Center)
        .build();
    let btn_dark = gtk::ToggleButton::builder()
        .label(t("Dark"))
        .active(saved_scheme == "dark")
        .group(&btn_system)
        .valign(gtk::Align::Center)
        .build();
    let btn_custom = gtk::ToggleButton::builder()
        .label(t("Custom"))
        .active(saved_scheme == "custom")
        .group(&btn_system)
        .valign(gtk::Align::Center)
        .build();

    let theme_btns = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    theme_btns.append(&btn_system);
    theme_btns.append(&btn_light);
    theme_btns.append(&btn_dark);
    theme_btns.append(&btn_custom);

    theme_row.add_suffix(&theme_btns);
    theme_list.append(&theme_row);

    // ── Custom colour pickers (revealed when Theme = Custom) ─────────────
    let saved_settings = load_app_settings();
    let accent_is_system = saved_settings.accent_color.is_none();
    let bg_is_system     = saved_settings.window_bg_color.is_none();

    // Helper: build one color row.
    // Returns (ActionRow, ColorDialogButton, reset Button) so callers can wire them.
    let make_color_row = |title: &str, dialog_title: &str, saved_hex: Option<&str>, fallback: &str| {
        let dialog = gtk::ColorDialog::builder()
            .with_alpha(false).title(dialog_title).build();
        let initial = saved_hex
            .and_then(|h| gdk4::RGBA::parse(h).ok())
            .unwrap_or_else(|| gdk4::RGBA::parse(fallback).unwrap_or(gdk4::RGBA::WHITE));
        let color_btn = gtk::ColorDialogButton::builder()
            .dialog(&dialog).rgba(&initial)
            .valign(gtk::Align::Center)
            .build();
        let reset_btn = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text(t("Reset to system default"))
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .visible(saved_hex.is_some())   // only shown when a custom color is set
            .build();
        let suffix = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4).valign(gtk::Align::Center).build();
        suffix.append(&reset_btn);
        suffix.append(&color_btn);
        let subtitle = if let Some(h) = saved_hex { h.to_string() }
                       else { t("System default").to_string() };
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(&subtitle)
            .build();
        row.add_suffix(&suffix);
        (row, color_btn, reset_btn)
    };

    let (accent_row, accent_btn, accent_reset) = make_color_row(
        t("Accent color"), t("Accent Color"),
        saved_settings.accent_color.as_deref(), "#3584e4",
    );
    let (bg_row, bg_btn, bg_reset) = make_color_row(
        t("Background color"), t("Background Color"),
        saved_settings.window_bg_color.as_deref(), "#1c1c1c",
    );
    let (text_row, text_btn, text_reset) = make_color_row(
        t("Text/Icon"), t("Text/Icon Color"),
        saved_settings.text_color.as_deref(), "#ffffff",
    );
    // aliases used in btn_custom handler
    let accent_sys_btn = accent_reset.clone();
    let bg_sys_btn     = bg_reset.clone();
    let text_sys_btn   = text_reset.clone();

    // Wire: accent picker picked a new color
    accent_btn.connect_rgba_notify({
        let row   = accent_row.clone();
        let reset = accent_reset.clone();
        move |btn| {
            let rgba = btn.rgba();
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.red()   * 255.0).round() as u8,
                (rgba.green() * 255.0).round() as u8,
                (rgba.blue()  * 255.0).round() as u8,
            );
            row.set_subtitle(&hex);
            reset.set_visible(true);
            let mut s = load_app_settings();
            s.accent_color = Some(hex);
            save_app_settings(&s);
            apply_custom_colors();
        }
    });

    // Wire: accent reset → back to system
    accent_reset.connect_clicked({
        let row = accent_row.clone();
        move |btn| {
            btn.set_visible(false);
            row.set_subtitle(t("System default"));
            let mut s = load_app_settings();
            s.accent_color = None;
            save_app_settings(&s);
            apply_custom_colors();
        }
    });

    // Wire: background picker picked a new color
    bg_btn.connect_rgba_notify({
        let row   = bg_row.clone();
        let reset = bg_reset.clone();
        move |btn| {
            let rgba = btn.rgba();
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.red()   * 255.0).round() as u8,
                (rgba.green() * 255.0).round() as u8,
                (rgba.blue()  * 255.0).round() as u8,
            );
            row.set_subtitle(&hex);
            reset.set_visible(true);
            let mut s = load_app_settings();
            s.window_bg_color = Some(hex);
            save_app_settings(&s);
            apply_custom_colors();
        }
    });

    // Wire: background reset → back to system
    bg_reset.connect_clicked({
        let row = bg_row.clone();
        move |btn| {
            btn.set_visible(false);
            row.set_subtitle(t("System default"));
            let mut s = load_app_settings();
            s.window_bg_color = None;
            save_app_settings(&s);
            apply_custom_colors();
        }
    });

    // Wire: text picker
    text_btn.connect_rgba_notify({
        let row   = text_row.clone();
        let reset = text_reset.clone();
        move |btn| {
            let rgba = btn.rgba();
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.red()   * 255.0).round() as u8,
                (rgba.green() * 255.0).round() as u8,
                (rgba.blue()  * 255.0).round() as u8,
            );
            row.set_subtitle(&hex);
            reset.set_visible(true);
            let mut s = load_app_settings();
            s.text_color = Some(hex);
            save_app_settings(&s);
            apply_custom_colors();
        }
    });

    // Wire: text reset → back to system
    text_reset.connect_clicked({
        let row = text_row.clone();
        move |btn| {
            btn.set_visible(false);
            row.set_subtitle(t("System default"));
            let mut s = load_app_settings();
            s.text_color = None;
            save_app_settings(&s);
            apply_custom_colors();
        }
    });

    let custom_colors_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12).margin_end(12).margin_top(8)
        .build();
    custom_colors_list.append(&accent_row);
    custom_colors_list.append(&bg_row);
    custom_colors_list.append(&text_row);

    let custom_revealer = gtk::Revealer::builder()
        .child(&custom_colors_list)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .reveal_child(saved_scheme == "custom")
        .build();

    // ── Keyboard shortcuts section ────────────────────────────────────────
    let shortcuts_lbl = gtk::Label::builder()
        .label(t("Keyboard Shortcuts"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .margin_top(24)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Helper: build a row with a ShortcutLabel suffix
    let mk_sc = |title: &str, accel: &str| -> adw::ActionRow {
        let row = adw::ActionRow::builder().title(title).build();
        let label = gtk::ShortcutLabel::builder()
            .accelerator(accel)
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&label);
        row
    };

    // ── Playback ──────────────────────────────────────────────────────────
    let playback_lbl = gtk::Label::builder()
        .label(t("Playback"))
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .margin_top(6)
        .margin_bottom(2)
        .margin_start(16)
        .build();
    let playback_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();
    playback_list.append(&mk_sc(t("Play / Pause"),   "space"));
    playback_list.append(&mk_sc(t("Next track"),     "n"));
    playback_list.append(&mk_sc(t("Previous track"), "b"));
    playback_list.append(&mk_sc(t("Mute"),           "m"));
    playback_list.append(&mk_sc(t("Screenshot"),     "s"));

    // ── Seek & Volume ─────────────────────────────────────────────────────
    let seek_lbl = gtk::Label::builder()
        .label(t("Seek & Volume"))
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .margin_top(12)
        .margin_bottom(2)
        .margin_start(16)
        .build();
    let seek_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();
    seek_list.append(&mk_sc(t("Seek −5 s"),   "Left"));
    seek_list.append(&mk_sc(t("Seek +5 s"),   "Right"));
    seek_list.append(&mk_sc(t("Seek −30 s"),  "<Shift>Left"));
    seek_list.append(&mk_sc(t("Seek +30 s"),  "<Shift>Right"));
    seek_list.append(&mk_sc(t("Volume up"),   "Up"));
    seek_list.append(&mk_sc(t("Volume down"), "Down"));

    // ── Speed & Video ─────────────────────────────────────────────────────
    let video_lbl = gtk::Label::builder()
        .label(t("Speed & Video"))
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .margin_top(12)
        .margin_bottom(2)
        .margin_start(16)
        .build();
    let video_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();
    video_list.append(&mk_sc(t("Speed up"),        "bracketright"));
    video_list.append(&mk_sc(t("Speed down"),      "bracketleft"));
    video_list.append(&mk_sc(t("Reset speed"),     "BackSpace"));
    video_list.append(&mk_sc(t("Fullscreen"),      "f"));
    video_list.append(&mk_sc(t("Exit fullscreen"), "Escape"));

    // ── App ───────────────────────────────────────────────────────────────
    let app_lbl = gtk::Label::builder()
        .label(t("App"))
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .margin_top(12)
        .margin_bottom(2)
        .margin_start(16)
        .build();
    let app_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();
    app_list.append(&mk_sc(t("Open file"),       "<Primary>o"));
    app_list.append(&mk_sc(t("Open URL"),        "<Primary>u"));
    app_list.append(&mk_sc(t("Load subtitle"),   "<Primary>t"));
    app_list.append(&mk_sc(t("Toggle playlist"), "<Primary>p"));
    app_list.append(&mk_sc(t("Settings"),        "<Primary>comma"));

    // ── Video / HDR section ───────────────────────────────────────────────
    let video_section_lbl = gtk::Label::builder()
        .label(t("Video"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .margin_top(24)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    let saved_tone_mapping = load_app_settings()
        .tone_mapping
        .unwrap_or_else(|| "bt.2446a".into());

    let hdr_info_lbl = gtk::Label::builder()
        .label(t("If your display supports HDR, the player will show HDR content natively with no processing. If your display does not support HDR, you can choose how HDR content is converted for display:"))
        .halign(gtk::Align::Start)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .margin_start(16)
        .margin_end(16)
        .margin_bottom(8)
        .build();

    let tm_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();

    let tm_row = adw::ActionRow::builder()
        .title(t("Tone mapping"))
        .build();

    let btn_bt2446a = gtk::ToggleButton::builder()
        .label("BT.2446a")
        .active(saved_tone_mapping == "bt.2446a")
        .tooltip_text(t("ITU standard — recommended"))
        .valign(gtk::Align::Center)
        .build();
    let btn_reinhard = gtk::ToggleButton::builder()
        .label("Reinhard")
        .active(saved_tone_mapping == "reinhard")
        .group(&btn_bt2446a)
        .tooltip_text(t("Softer, less contrast"))
        .valign(gtk::Align::Center)
        .build();
    let btn_hable = gtk::ToggleButton::builder()
        .label("Hable")
        .active(saved_tone_mapping == "hable")
        .group(&btn_bt2446a)
        .tooltip_text(t("Cinematic S-curve"))
        .valign(gtk::Align::Center)
        .build();
    let btn_clip = gtk::ToggleButton::builder()
        .label("Clip")
        .active(saved_tone_mapping == "clip")
        .group(&btn_bt2446a)
        .tooltip_text(t("No tone mapping — for HDR displays"))
        .valign(gtk::Align::Center)
        .build();

    let tm_btns = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    tm_btns.append(&btn_bt2446a);
    tm_btns.append(&btn_reinhard);
    tm_btns.append(&btn_hable);
    tm_btns.append(&btn_clip);

    tm_row.add_suffix(&tm_btns);
    tm_list.append(&tm_row);

    // Wire tone mapping buttons
    for (btn, mode) in [
        (&btn_bt2446a, "bt.2446a"),
        (&btn_reinhard, "reinhard"),
        (&btn_hable, "hable"),
        (&btn_clip, "clip"),
    ] {
        btn.connect_toggled({
            let cb = on_tone_mapping_change.clone();
            let mode = mode.to_string();
            move |btn| {
                if btn.is_active() {
                    let mut s = load_app_settings();
                    s.tone_mapping = Some(mode.clone());
                    save_app_settings(&s);
                    cb(&mode);
                }
            }
        });
    }

    // ── Footer ────────────────────────────────────────────────────────────
    let footer = gtk::Label::builder()
        .use_markup(true)
        .wrap(true)
        .max_width_chars(36)
        .justify(gtk::Justification::Center)
        .css_classes(["caption", "dim-label"])
        .halign(gtk::Align::Center)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(12)
        .margin_end(12)
        .build();
    footer.set_markup(&format!(
        "{}\n<a href=\"https://github.com/daniacosta-dev/aurora-media-player\">{}</a>",
        t("If you like Aurora Media Player, consider"),
        t("⭐ starring it on GitHub"),
    ));

    // ── Control bar section ─────────────────────────────────────────────
    let layout_lbl = gtk::Label::builder()
        .label(t("Control bar"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .margin_top(24)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    let saved_ui_mode = load_app_settings()
        .ui_mode
        .unwrap_or_else(|| "modern".into());

    let layout_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();

    let layout_row = adw::ActionRow::builder()
        .title(t("Control bar style"))
        .build();

    let btn_modern = gtk::ToggleButton::builder()
        .label(t("Modern"))
        .active(saved_ui_mode == "modern")
        .valign(gtk::Align::Center)
        .build();
    let btn_floating = gtk::ToggleButton::builder()
        .label(t("Floating"))
        .active(saved_ui_mode == "floating")
        .group(&btn_modern)
        .valign(gtk::Align::Center)
        .build();
    let btn_fixed = gtk::ToggleButton::builder()
        .label(t("Fixed"))
        .active(saved_ui_mode == "fixed")
        .group(&btn_modern)
        .valign(gtk::Align::Center)
        .build();

    let layout_btns = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    layout_btns.append(&btn_modern);
    layout_btns.append(&btn_floating);
    layout_btns.append(&btn_fixed);

    layout_row.add_suffix(&layout_btns);
    layout_list.append(&layout_row);

    // ── Language section ─────────────────────────────────────────────────
    let lang_section_lbl = gtk::Label::builder()
        .label(t("Language"))
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .margin_top(24)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    let saved_lang = load_app_settings()
        .language
        .unwrap_or_else(|| "en".into());

    let lang_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .build();

    let lang_row = adw::ActionRow::builder()
        .title(t("Interface language"))
        .build();

    let btn_en = gtk::ToggleButton::builder()
        .label(t("English"))
        .active(saved_lang == "en")
        .valign(gtk::Align::Center)
        .build();
    let btn_es = gtk::ToggleButton::builder()
        .label(t("Spanish"))
        .active(saved_lang == "es")
        .group(&btn_en)
        .valign(gtk::Align::Center)
        .build();

    let lang_btns = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    lang_btns.append(&btn_en);
    lang_btns.append(&btn_es);

    lang_row.add_suffix(&lang_btns);
    lang_list.append(&lang_row);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_bottom(32)
        .build();
    content.append(&appearance_lbl);
    content.append(&theme_list);
    content.append(&custom_revealer);
    content.append(&layout_lbl);
    content.append(&layout_list);
    content.append(&lang_section_lbl);
    content.append(&lang_list);
    content.append(&video_section_lbl);
    content.append(&hdr_info_lbl);
    content.append(&tm_list);
    content.append(&shortcuts_lbl);
    content.append(&playback_lbl);
    content.append(&playback_list);
    content.append(&seek_lbl);
    content.append(&seek_list);
    content.append(&video_lbl);
    content.append(&video_list);
    content.append(&app_lbl);
    content.append(&app_list);
    content.append(&footer);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .margin_bottom(12)
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroll));
    dialog.set_content(Some(&toolbar_view));

    // Wire theme buttons → StyleManager + persistence
    btn_system.connect_toggled({
        let revealer = custom_revealer.clone();
        move |btn| {
            if btn.is_active() {
                adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
                let mut s = load_app_settings();
                s.color_scheme = Some("system".into());
                s.accent_color = None;
                s.window_bg_color = None;
                s.text_color = None;
                save_app_settings(&s);
                apply_custom_colors();
                revealer.set_reveal_child(false);
            }
        }
    });
    btn_light.connect_toggled({
        let revealer = custom_revealer.clone();
        move |btn| {
            if btn.is_active() {
                adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
                let mut s = load_app_settings();
                s.color_scheme = Some("light".into());
                s.accent_color = None;
                s.window_bg_color = None;
                s.text_color = None;
                save_app_settings(&s);
                apply_custom_colors();
                revealer.set_reveal_child(false);
            }
        }
    });
    btn_dark.connect_toggled({
        let revealer = custom_revealer.clone();
        move |btn| {
            if btn.is_active() {
                adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
                let mut s = load_app_settings();
                s.color_scheme = Some("dark".into());
                s.accent_color = None;
                s.window_bg_color = None;
                s.text_color = None;
                save_app_settings(&s);
                apply_custom_colors();
                revealer.set_reveal_child(false);
            }
        }
    });
    btn_custom.connect_toggled({
        let revealer = custom_revealer.clone();
        let accent_btn = accent_btn.clone();
        let accent_reset = accent_reset.clone();
        let bg_btn = bg_btn.clone();
        let bg_reset = bg_reset.clone();
        let text_btn = text_btn.clone();
        let text_reset = text_reset.clone();
        move |btn| {
            if btn.is_active() {
                adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
                let mut s = load_app_settings();
                s.color_scheme = Some("custom".into());
                // Always snapshot every button's current RGBA so that
                // clicking "Select" without moving the picker still applies
                // the color (notify::rgba won't fire if the value didn't change).
                let rgba_hex = |rgba: gdk4::RGBA| format!(
                    "#{:02x}{:02x}{:02x}",
                    (rgba.red()   * 255.0).round() as u8,
                    (rgba.green() * 255.0).round() as u8,
                    (rgba.blue()  * 255.0).round() as u8,
                );
                s.accent_color     = Some(rgba_hex(accent_btn.rgba()));
                s.window_bg_color  = Some(rgba_hex(bg_btn.rgba()));
                s.text_color       = Some(rgba_hex(text_btn.rgba()));
                accent_reset.set_visible(true);
                bg_reset.set_visible(true);
                text_reset.set_visible(true);
                save_app_settings(&s);
                apply_custom_colors();
                revealer.set_reveal_child(true);
            }
        }
    });

    btn_floating.connect_toggled({
        let cb = on_ui_mode_change.clone();
        move |btn| {
            if btn.is_active() {
                let mut s = load_app_settings(); s.ui_mode = Some("floating".into()); save_app_settings(&s);
                cb("floating");
            }
        }
    });
    btn_fixed.connect_toggled({
        let cb = on_ui_mode_change.clone();
        move |btn| {
            if btn.is_active() {
                let mut s = load_app_settings(); s.ui_mode = Some("fixed".into()); save_app_settings(&s);
                cb("fixed");
            }
        }
    });
    btn_modern.connect_toggled({
        let cb = on_ui_mode_change.clone();
        move |btn| {
            if btn.is_active() {
                let mut s = load_app_settings(); s.ui_mode = Some("modern".into()); save_app_settings(&s);
                cb("modern");
            }
        }
    });

    btn_en.connect_toggled({
        let lang_cb = on_language_change.clone();
        move |btn| {
            if btn.is_active() {
                crate::i18n::set(crate::i18n::Lang::En);
                let mut s = load_app_settings(); s.language = Some("en".into()); save_app_settings(&s);
                lang_cb();
            }
        }
    });
    btn_es.connect_toggled({
        let lang_cb = on_language_change.clone();
        move |btn| {
            if btn.is_active() {
                crate::i18n::set(crate::i18n::Lang::Es);
                let mut s = load_app_settings(); s.language = Some("es".into()); save_app_settings(&s);
                lang_cb();
            }
        }
    });

    dialog.connect_close_request(|_| {
        apply_custom_colors();
        glib::Propagation::Proceed
    });
    dialog.present();

    // Scroll to the Video section after the dialog is rendered.
    if scroll_to_video {
        let scroll_c = scroll.clone();
        let tm_list_c = tm_list.clone();
        let content_c = content.clone();
        glib::idle_add_local_once(move || {
            if let Some((_, y)) = tm_list_c.translate_coordinates(&content_c, 0.0, 0.0) {
                scroll_c.vadjustment().set_value((y - 12.0).max(0.0));
            }
        });
    }
}

// ── URL playlist dialog ───────────────────────────────────────────────────────

/// True if the URL path (before `?`) ends with `.m3u` or `.m3u8`.
fn looks_like_m3u_url(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url).to_lowercase();
    path.ends_with(".m3u") || path.ends_with(".m3u8")
}

/// Derive a hostname-based fallback title from a URL.
fn title_for_url(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .and_then(|host| host.split('?').next())
        .unwrap_or("URL")
        .to_string()
}

/// Fetch an M3U URL with curl and return `(title, stream_url)` pairs.
/// Titles come from `#EXTINF` lines; falls back to hostname when absent.
/// Called from a background thread — must not touch GTK objects.
fn fetch_and_parse_m3u(url: &str) -> Vec<(String, String)> {
    let output = std::process::Command::new("curl")
        .args(["-s", "-L", "--max-time", "15", url])
        .output()
        .ok();
    let Some(output) = output else { return vec![] };
    let Ok(content) = String::from_utf8(output.stdout) else { return vec![] };

    let mut result = Vec::new();
    let mut pending_title: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // #EXTINF:duration[,...],Title
            if let Some((_, title)) = rest.split_once(',') {
                let title = title.trim();
                if !title.is_empty() {
                    pending_title = Some(title.to_string());
                }
            }
        } else if line.starts_with('#') {
            continue; // skip other directives
        } else if line.starts_with("http://") || line.starts_with("https://") {
            let title = pending_title.take()
                .unwrap_or_else(|| title_for_url(line));
            result.push((title, line.to_string()));
        }
    }
    result
}

/// Expand any M3U URLs in `urls` into `(title, stream_url)` pairs.
/// Non-M3U URLs are kept as-is with a hostname-based title.
/// Called from a background thread.
fn expand_m3u_urls(urls: Vec<String>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for url in urls {
        if looks_like_m3u_url(&url) {
            let fetched = fetch_and_parse_m3u(&url);
            if fetched.is_empty() {
                result.push((title_for_url(&url), url)); // fallback: keep original
            } else {
                result.extend(fetched);
            }
        } else {
            result.push((title_for_url(&url), url));
        }
    }
    result
}

/// Returns `Some(error_message)` if the URL is not a valid http/https URL,
/// or `None` if it looks fine.  Only checks syntax — not reachability.
fn validate_url(url: &str) -> Option<String> {
    let url = url.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Some("Must start with http:// or https://".into());
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or("");
    let host = rest.split('/').next().unwrap_or("").split('?').next().unwrap_or("");
    if host.is_empty() {
        return Some("Missing hostname".into());
    }
    None
}

fn show_url_playlist_dialog(
    parent: &gtk::Window,
    on_load: Rc<impl Fn(Option<String>, Vec<(String, String)>) + 'static>,
) {
    let dialog = adw::Window::builder()
        .title(t("URL Playlist"))
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(520)
        .build();

    // ── Header ────────────────────────────────────────────────────────────
    let header = adw::HeaderBar::new();
    let play_btn = Button::builder()
        .label(t("Play"))
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    header.pack_end(&play_btn);

    // ── URL entry list ────────────────────────────────────────────────────
    let list_box = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(12)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Add URL sentinel row (always at the bottom of the entry list)
    let add_row = gtk::ListBoxRow::builder().activatable(true).build();
    let add_label = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    add_label.append(&gtk::Image::from_icon_name("list-add-symbolic"));
    add_label.append(&gtk::Label::new(Some(t("Add URL"))));
    add_row.set_child(Some(&add_label));
    list_box.append(&add_row);

    // ── Save-as row ───────────────────────────────────────────────────────
    let save_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let name_row = adw::EntryRow::builder().title(t("Save as playlist…")).build();
    let save_btn = Button::builder()
        .label(t("Save"))
        .css_classes(["flat"])
        .tooltip_text(t("Save playlist"))
        .sensitive(false)
        .build();
    name_row.add_suffix(&save_btn);
    save_list.append(&name_row);

    // ── Saved playlists list (hidden when empty) ──────────────────────────
    let saved_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(0)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .visible(false)
        .build();

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    vbox.append(&list_box);
    vbox.append(&save_list);
    vbox.append(&saved_list);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&vbox)
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroll));
    dialog.set_content(Some(&toolbar_view));

    // ── Entry row tracking ────────────────────────────────────────────────
    let entries: Rc<RefCell<Vec<adw::EntryRow>>> = Rc::new(RefCell::new(Vec::new()));

    // Late-bound: loads a saved playlist's URLs into the editor rows.
    let load_into_editor: Rc<RefCell<Option<Box<dyn Fn(Vec<String>, String)>>>> =
        Rc::new(RefCell::new(None));

    // ── Saved-playlist section rebuild (late-bound to break circular ref) ─
    let rebuild_saved: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    {
        let saved_list_c = saved_list.clone();
        let on_load_c = on_load.clone();
        let dialog_c = dialog.clone();
        let rebuild_ref = rebuild_saved.clone();
        let lie_ref = load_into_editor.clone();
        let name_row_rs = name_row.clone();
        *rebuild_saved.borrow_mut() = Some(Box::new(move || {
            while let Some(child) = saved_list_c.first_child() {
                saved_list_c.remove(&child);
            }
            let saved = load_saved_playlists();
            // Hide the playlist currently loaded in the editor to avoid redundancy.
            let editing = name_row_rs.text();
            let editing = editing.trim().to_string();
            let visible: Vec<_> = saved.playlists.iter()
                .filter(|p| p.name != editing)
                .collect();
            saved_list_c.set_visible(!visible.is_empty());
            for playlist in &visible {
                let row = gtk::ListBoxRow::builder().activatable(false).build();
                let hbox = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(8)
                    .margin_top(8)
                    .margin_bottom(8)
                    .margin_start(12)
                    .margin_end(12)
                    .build();
                let label = gtk::Label::builder()
                    .label(&playlist.name)
                    .hexpand(true)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let edit_btn = Button::builder()
                    .icon_name("document-edit-symbolic")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .tooltip_text(t("Edit"))
                    .build();
                let load_btn = Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .tooltip_text(t("Play"))
                    .build();
                let delete_btn = Button::builder()
                    .icon_name("edit-delete-symbolic")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .tooltip_text(t("Delete"))
                    .build();
                hbox.append(&label);
                hbox.append(&edit_btn);
                hbox.append(&load_btn);
                hbox.append(&delete_btn);
                row.set_child(Some(&hbox));
                saved_list_c.append(&row);

                let urls_e = playlist.urls.clone();
                let name_e = playlist.name.clone();
                let lie_e = lie_ref.clone();
                edit_btn.connect_clicked(move |_| {
                    if let Some(f) = &*lie_e.borrow() {
                        f(urls_e.clone(), name_e.clone());
                    }
                });

                let urls = playlist.urls.clone();
                let saved_name = playlist.name.clone();
                let on_load_l = on_load_c.clone();
                let dialog_l = dialog_c.clone();
                load_btn.connect_clicked(move |btn| {
                    // Expand any M3U URLs before loading, same as the editor's Play button.
                    if urls.iter().any(|u| looks_like_m3u_url(u)) {
                        btn.set_sensitive(false);
                        let urls_c = urls.clone();
                        let (tx, rx) = std::sync::mpsc::channel::<Vec<(String, String)>>();
                        std::thread::spawn(move || { tx.send(expand_m3u_urls(urls_c)).ok(); });
                        let on_load_t = on_load_l.clone();
                        let dialog_t = dialog_l.clone();
                        let btn_w = btn.downgrade();
                        let name_t = saved_name.clone();
                        glib::timeout_add_local(Duration::from_millis(100), move || {
                            match rx.try_recv() {
                                Ok(expanded) => {
                                    if !expanded.is_empty() { on_load_t(Some(name_t.clone()), expanded); }
                                    dialog_t.close();
                                    glib::ControlFlow::Break
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                                Err(_) => {
                                    if let Some(b) = btn_w.upgrade() { b.set_sensitive(true); }
                                    glib::ControlFlow::Break
                                }
                            }
                        });
                    } else {
                        let items = urls.iter().map(|u| (title_for_url(u), u.clone())).collect();
                        on_load_l(Some(saved_name.clone()), items);
                        dialog_l.close();
                    }
                });

                let name_d = playlist.name.clone();
                let rebuild_d = rebuild_ref.clone();
                delete_btn.connect_clicked(move |_| {
                    let mut saved = load_saved_playlists();
                    saved.playlists.retain(|p| p.name != name_d);
                    write_saved_playlists(&saved);
                    if let Some(f) = &*rebuild_d.borrow() { f(); }
                });
            }
        }));
    }
    if let Some(f) = &*rebuild_saved.borrow() { f(); }

    // ── add_row sensitivity: disabled when last entry is empty ────────────
    let update_add_row = {
        let entries_c = entries.clone();
        let add_row_c = add_row.clone();
        Rc::new(move || {
            let sensitive = entries_c
                .borrow()
                .last()
                .map(|e| !e.text().trim().is_empty() && !e.has_css_class("error"))
                .unwrap_or(true); // no entries yet → allow adding the first
            add_row_c.set_sensitive(sensitive);
        })
    };

    // ── save_btn sensitivity: name non-empty, at least one valid URL,
    //    and no outstanding invalid (non-empty) entries ─────────────────────
    let update_save_btn = {
        let entries_c = entries.clone();
        let name_row_c = name_row.clone();
        let save_btn_c = save_btn.clone();
        Rc::new(move || {
            let has_name = !name_row_c.text().trim().is_empty();
            let ents = entries_c.borrow();
            let has_valid = ents.iter().any(|e| {
                let t = e.text(); !t.trim().is_empty() && !e.has_css_class("error")
            });
            let has_invalid = ents.iter().any(|e| {
                let t = e.text(); !t.trim().is_empty() && e.has_css_class("error")
            });
            save_btn_c.set_sensitive(has_name && has_valid && !has_invalid);
        })
    };

    // ── Append entry row before the sentinel ─────────────────────────────
    let append_entry = {
        let list_box_c = list_box.clone();
        let entries_c = entries.clone();
        let play_btn_c = play_btn.clone();
        let update_add_c = update_add_row.clone();
        let update_save_c = update_save_btn.clone();

        Rc::new(move || {
            let entry_row = adw::EntryRow::builder()
                .title("URL")
                .show_apply_button(false)
                .build();

            // Warning icon shown when the URL format is invalid.
            let warn_icon = gtk::Image::builder()
                .icon_name("dialog-warning-symbolic")
                .css_classes(["warning"])
                .visible(false)
                .build();
            let remove_btn = Button::builder()
                .icon_name("list-remove-symbolic")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .tooltip_text(t("Remove"))
                .build();
            entry_row.add_suffix(&warn_icon);
            entry_row.add_suffix(&remove_btn);

            let pos = list_box_c.observe_children().n_items().saturating_sub(1) as i32;
            list_box_c.insert(&entry_row, pos);
            entries_c.borrow_mut().push(entry_row.clone());

            {
                let entries_s = entries_c.clone();
                let play_btn_s = play_btn_c.clone();
                let update_add_s = update_add_c.clone();
                let update_save_s = update_save_c.clone();
                let row_s = entry_row.clone();
                let warn_s = warn_icon.clone();
                entry_row.connect_changed(move |row| {
                    let text = row.text();
                    let text = text.trim();
                    if text.is_empty() {
                        row_s.remove_css_class("error");
                        warn_s.set_visible(false);
                        warn_s.set_tooltip_text(None);
                    } else if let Some(err) = validate_url(text) {
                        row_s.add_css_class("error");
                        warn_s.set_visible(true);
                        warn_s.set_tooltip_text(Some(&err));
                    } else {
                        row_s.remove_css_class("error");
                        warn_s.set_visible(false);
                        warn_s.set_tooltip_text(None);
                    }
                    let has_valid = entries_s.borrow().iter().any(|e| {
                        let t = e.text(); !t.trim().is_empty() && !e.has_css_class("error")
                    });
                    play_btn_s.set_sensitive(has_valid);
                    update_add_s();
                    update_save_s();
                });
            }

            {
                let list_box_r = list_box_c.clone();
                let entries_r = entries_c.clone();
                let play_btn_r = play_btn_c.clone();
                let update_add_r = update_add_c.clone();
                let update_save_r = update_save_c.clone();
                let entry_row_r = entry_row.clone();
                remove_btn.connect_clicked(move |_| {
                    list_box_r.remove(&entry_row_r);
                    entries_r.borrow_mut().retain(|e| e != &entry_row_r);
                    let has_valid = entries_r.borrow().iter().any(|e| {
                        let t = e.text(); !t.trim().is_empty() && !e.has_css_class("error")
                    });
                    play_btn_r.set_sensitive(has_valid);
                    update_add_r();
                    update_save_r();
                });
            }

            update_add_c();
        })
    };

    // ── Wire load_into_editor (needs append_entry, so set here) ──────────
    {
        let entries_l = entries.clone();
        let list_box_l = list_box.clone();
        let append_l = append_entry.clone();
        let name_row_l = name_row.clone();
        let play_btn_l = play_btn.clone();
        let update_add_l = update_add_row.clone();
        let update_save_l = update_save_btn.clone();
        let rebuild_l = rebuild_saved.clone();
        *load_into_editor.borrow_mut() = Some(Box::new(move |urls: Vec<String>, name: String| {
            // Remove all current entry rows from the list.
            {
                let mut ents = entries_l.borrow_mut();
                for entry in ents.drain(..) {
                    list_box_l.remove(&entry);
                }
            }
            // Re-add one row per saved URL, pre-filled.
            // Clone the entry out before calling set_text to avoid a double-borrow
            // (set_text fires connect_changed which also borrows entries_l).
            for url in urls {
                append_l();
                let entry = entries_l.borrow().last().cloned();
                if let Some(entry) = entry {
                    entry.set_text(&url);
                }
            }
            // Pre-fill playlist name so saving will update the existing entry.
            name_row_l.set_text(&name);
            // Refresh sensitivities.
            let has_valid = entries_l.borrow().iter().any(|e| {
                let t = e.text(); !t.trim().is_empty() && !e.has_css_class("error")
            });
            play_btn_l.set_sensitive(has_valid);
            update_add_l();
            update_save_l();
            // Rebuild the saved list so the edited playlist disappears from it.
            if let Some(f) = &*rebuild_l.borrow() { f(); }
        }));
    }

    // Start with one empty entry (add_row will be insensitive initially).
    append_entry();

    // "Add URL" sentinel click → append another entry.
    {
        let append_c = append_entry.clone();
        list_box.connect_row_activated(move |_, row| {
            if row.child().and_downcast_ref::<adw::EntryRow>().is_none() {
                append_c();
            }
        });
    }

    // name_row changes → update save sensitivity + refresh saved list
    // (the saved list hides whichever playlist matches the current name).
    {
        let update_save_c = update_save_btn.clone();
        let rebuild_c2 = rebuild_saved.clone();
        name_row.connect_changed(move |_| {
            update_save_c();
            if let Some(f) = &*rebuild_c2.borrow() { f(); }
        });
    }

    // ── Save button ───────────────────────────────────────────────────────
    {
        let entries_c = entries.clone();
        let name_row_c = name_row.clone();
        let rebuild_c = rebuild_saved.clone();
        save_btn.connect_clicked(move |_| {
            let name = name_row_c.text().trim().to_string();
            let urls: Vec<String> = entries_c
                .borrow()
                .iter()
                .map(|e| e.text().trim().to_string())
                .filter(|u| !u.is_empty())
                .collect();
            if name.is_empty() || urls.is_empty() { return; }
            let mut saved = load_saved_playlists();
            if let Some(existing) = saved.playlists.iter_mut().find(|p| p.name == name) {
                existing.urls = urls;
            } else {
                saved.playlists.push(SavedPlaylist { name, urls });
            }
            write_saved_playlists(&saved);
            name_row_c.set_text("");
            if let Some(f) = &*rebuild_c.borrow() { f(); }
        });
    }

    // ── Play All ──────────────────────────────────────────────────────────
    {
        let entries_c = entries.clone();
        let dialog_c = dialog.clone();
        let play_btn_w = play_btn.downgrade();
        play_btn.connect_clicked(move |btn| {
            let urls: Vec<String> = entries_c
                .borrow()
                .iter()
                .filter(|e| !e.has_css_class("error"))
                .map(|e| e.text().trim().to_string())
                .filter(|u| !u.is_empty())
                .collect();
            if urls.is_empty() { return; }

            // If any URL looks like an M3U playlist, fetch and expand it
            // in a background thread so the UI stays responsive.
            if urls.iter().any(|u| looks_like_m3u_url(u)) {
                btn.set_sensitive(false);
                btn.set_label("Loading…");

                // Derive a playlist name from the first M3U URL's hostname.
                let playlist_name: Option<String> = urls.iter()
                    .find(|u| looks_like_m3u_url(u))
                    .map(|u| title_for_url(u));

                let (tx, rx) = std::sync::mpsc::channel::<Vec<(String, String)>>();
                std::thread::spawn(move || { tx.send(expand_m3u_urls(urls)).ok(); });

                let on_load_c = on_load.clone();
                let dialog_c2 = dialog_c.clone();
                let play_btn_w2 = play_btn_w.clone();
                glib::timeout_add_local(Duration::from_millis(100), move || {
                    match rx.try_recv() {
                        Ok(expanded) => {
                            if !expanded.is_empty() { on_load_c(playlist_name.clone(), expanded); }
                            dialog_c2.close();
                            glib::ControlFlow::Break
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                        Err(_) => {
                            if let Some(btn) = play_btn_w2.upgrade() {
                                btn.set_sensitive(true);
                                btn.set_label("Play");
                            }
                            glib::ControlFlow::Break
                        }
                    }
                });
            } else {
                // Direct URLs (not M3U) → go to "Recent URLs" in library (None).
                let items = urls.into_iter().map(|u| (title_for_url(&u), u)).collect();
                on_load(None, items);
                dialog_c.close();
            }
        });
    }

    dialog.present();
}
