use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::NavigationPage;
use gtk4::{self as gtk, Label, ListBox, ListBoxRow, ScrolledWindow,
           Orientation, SelectionMode, Align};
use gtk4::prelude::*;

use crate::i18n::t;
use crate::library::Playlist;

// ── LibrarySidebar ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct LibrarySidebar {
    inner: Rc<SidebarInner>,
}

struct SidebarInner {
    page:             NavigationPage,
    list:             ListBox,
    // Count labels for fixed category rows.
    count_all:        Label,
    count_video:      Label,
    count_audio:      Label,
    count_recent:     Label,
    folder_rows:      RefCell<Vec<(PathBuf, ListBoxRow)>>,
    playlist_rows:    RefCell<Vec<(String, ListBoxRow)>>,
    playlist_header:  ListBoxRow,
    on_filter:        RefCell<Option<Box<dyn Fn(String)>>>,
    on_remove_folder: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    on_delete_playlist: RefCell<Option<Box<dyn Fn(String)>>>,
    on_rename_playlist: RefCell<Option<Box<dyn Fn(String, String)>>>,
    on_new_playlist:  RefCell<Option<Box<dyn Fn()>>>,
}

impl LibrarySidebar {
    pub fn new() -> Self {
        let list = ListBox::builder()
            .selection_mode(SelectionMode::Single)
            .css_classes(vec!["navigation-sidebar"])
            .build();

        // ── Fixed category rows ───────────────────────────────────────────
        let (row_all,    count_all)    = make_nav_row_with_count("video-display-symbolic",   t("All Media"), "all");
        let (row_video,  count_video)  = make_nav_row_with_count("video-x-generic-symbolic", t("Videos"),    "video");
        let (row_audio,  count_audio)  = make_nav_row_with_count("audio-x-generic-symbolic", t("Music"),     "audio");
        let (row_recent, count_recent) = make_nav_row_with_count("media-playback-start-symbolic", t("Recently Played"), "recent");

        list.append(&row_all);
        list.append(&row_video);
        list.append(&row_audio);
        list.append(&row_recent);

        // ── "Folders" section header ──────────────────────────────────────
        let folder_header = make_section_header(t("Folders"), None::<&gtk::Button>);
        list.append(&folder_header);

        // ── "Playlists [+]" section header ───────────────────────────────
        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(vec!["flat", "circular"])
            .tooltip_text(t("New playlist"))
            .build();
        add_btn.set_size_request(22, 22);
        add_btn.set_cursor_from_name(Some("pointer"));
        let playlist_header = make_section_header(t("Playlists"), Some(&add_btn));

        let scroll = ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&list)
            .build();

        let page = NavigationPage::builder()
            .title(t("Library"))
            .tag("library-sidebar")
            .child(&scroll)
            .build();

        list.select_row(Some(&row_all));

        let inner = Rc::new(SidebarInner {
            page,
            list,
            count_all,
            count_video,
            count_audio,
            count_recent,
            folder_rows:      RefCell::new(Vec::new()),
            playlist_rows:    RefCell::new(Vec::new()),
            playlist_header,
            on_filter:          RefCell::new(None),
            on_remove_folder:   RefCell::new(None),
            on_delete_playlist: RefCell::new(None),
            on_rename_playlist: RefCell::new(None),
            on_new_playlist:    RefCell::new(None),
        });

        // Wire row selection.
        inner.list.connect_row_selected({
            let inner_c = inner.clone();
            move |_, row| {
                if let Some(row) = row {
                    let name = row.widget_name().to_string();
                    if let Some(cb) = &*inner_c.on_filter.borrow() {
                        cb(name);
                    }
                }
            }
        });

        // Wire "+" button.
        add_btn.connect_clicked({
            let inner_c = inner.clone();
            move |_| {
                if let Some(cb) = &*inner_c.on_new_playlist.borrow() {
                    cb();
                }
            }
        });

        Self { inner }
    }

    pub fn page(&self) -> &NavigationPage { &self.inner.page }

    pub fn clone_ref(&self) -> Self { Self { inner: self.inner.clone() } }

    pub fn connect_filter_changed<F: Fn(String) + 'static>(&self, f: F) {
        *self.inner.on_filter.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_remove_folder<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.inner.on_remove_folder.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_delete_playlist<F: Fn(String) + 'static>(&self, f: F) {
        *self.inner.on_delete_playlist.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_rename_playlist<F: Fn(String, String) + 'static>(&self, f: F) {
        *self.inner.on_rename_playlist.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_new_playlist<F: Fn() + 'static>(&self, f: F) {
        *self.inner.on_new_playlist.borrow_mut() = Some(Box::new(f));
    }

    /// Update the counts shown next to the fixed category rows.
    pub fn update_category_counts(&self, all: usize, video: usize, audio: usize, recent: usize) {
        set_count_label(&self.inner.count_all,    all);
        set_count_label(&self.inner.count_video,  video);
        set_count_label(&self.inner.count_audio,  audio);
        set_count_label(&self.inner.count_recent, recent);
    }

    /// Rebuild watched-folder rows with item counts.
    /// `folders` is `(path, item_count)`.
    pub fn update_folders(&self, folders: Vec<(PathBuf, usize)>) {
        for (_, row) in self.inner.folder_rows.borrow().iter() {
            self.inner.list.remove(row);
        }
        self.inner.list.remove(&self.inner.playlist_header);
        for (_, row) in self.inner.playlist_rows.borrow().iter() {
            self.inner.list.remove(row);
        }

        let mut new_rows = Vec::new();
        for (folder, count) in folders {
            let label = folder.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Folder")
                .to_string();
            let (row, count_lbl) =
                make_nav_row_with_count("folder-symbolic", &label, &folder.to_string_lossy());
            set_count_label(&count_lbl, count);
            attach_folder_context_menu(&row, folder.clone(), self.inner.clone());
            self.inner.list.append(&row);
            new_rows.push((folder, row));
        }
        *self.inner.folder_rows.borrow_mut() = new_rows;

        self.inner.list.append(&self.inner.playlist_header);
        for (_, row) in self.inner.playlist_rows.borrow().iter() {
            self.inner.list.append(row);
        }
    }

    /// Rebuild playlist rows, showing item counts.
    pub fn update_playlists(&self, playlists: Vec<Playlist>) {
        self.inner.list.remove(&self.inner.playlist_header);
        for (_, row) in self.inner.playlist_rows.borrow().iter() {
            self.inner.list.remove(row);
        }

        self.inner.list.append(&self.inner.playlist_header);
        let mut new_rows = Vec::new();
        for pl in &playlists {
            let widget_name = format!("playlist:{}", pl.id);
            // Detect URL playlists by checking if paths are http(s) URLs.
            let is_url_playlist = pl.paths.first().map(|p| {
                let s = p.to_string_lossy();
                s.starts_with("http://") || s.starts_with("https://")
            }).unwrap_or(false);
            let icon = if is_url_playlist { "applications-internet-symbolic" } else { "view-list-symbolic" };
            let (row, count_lbl) =
                make_nav_row_with_count(icon, &pl.name, &widget_name);
            let live = if is_url_playlist {
                pl.paths.len() // URL items don't live on disk
            } else {
                pl.paths.iter().filter(|p| p.is_file()).count()
            };
            set_count_label(&count_lbl, live);
            attach_playlist_context_menu(&row, pl.id.clone(), pl.name.clone(), self.inner.clone());
            self.inner.list.append(&row);
            new_rows.push((pl.id.clone(), row));
        }
        *self.inner.playlist_rows.borrow_mut() = new_rows;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn set_count_label(lbl: &Label, count: usize) {
    if count > 0 {
        lbl.set_label(&count.to_string());
        lbl.set_visible(true);
    } else {
        lbl.set_visible(false);
    }
}

// ── Context menus ─────────────────────────────────────────────────────────────

fn attach_folder_context_menu(row: &ListBoxRow, folder: PathBuf, inner: Rc<SidebarInner>) {
    let remove_btn = gtk::Button::builder()
        .label(t("Remove from library"))
        .css_classes(vec!["flat", "destructive-action"])
        .halign(Align::Fill)
        .build();

    let popover_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4)
        .margin_start(4).margin_end(4)
        .build();
    popover_box.append(&remove_btn);

    let popover = gtk::Popover::builder()
        .child(&popover_box)
        .has_arrow(false)
        .build();
    popover.set_parent(row);

    let popover_c = popover.clone();
    remove_btn.connect_clicked(move |_| {
        popover_c.popdown();
        if let Some(cb) = &*inner.on_remove_folder.borrow() {
            cb(folder.clone());
        }
    });

    attach_right_click_gesture(row, &popover);
}

fn attach_playlist_context_menu(
    row: &ListBoxRow,
    playlist_id: String,
    playlist_name: String,
    inner: Rc<SidebarInner>,
) {
    // Rename button
    let rename_btn = gtk::Button::builder()
        .label(t("Rename…"))
        .css_classes(vec!["flat"])
        .halign(Align::Fill)
        .build();

    // Delete button
    let delete_btn = gtk::Button::builder()
        .label(t("Delete playlist"))
        .css_classes(vec!["flat", "destructive-action"])
        .halign(Align::Fill)
        .build();

    let popover_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4)
        .margin_start(4).margin_end(4)
        .spacing(2)
        .build();
    popover_box.set_size_request(160, -1);
    popover_box.append(&rename_btn);
    popover_box.append(&gtk::Separator::new(Orientation::Horizontal));
    popover_box.append(&delete_btn);

    let popover = gtk::Popover::builder()
        .child(&popover_box)
        .has_arrow(false)
        .build();
    popover.set_parent(row);

    // Rename action — show an AlertDialog to get the new name.
    {
        let inner_c = inner.clone();
        let id_c    = playlist_id.clone();
        let name_c  = playlist_name.clone();
        let popover_c = popover.clone();
        let row_c   = row.clone();
        rename_btn.connect_clicked(move |_| {
            popover_c.popdown();
            show_rename_dialog(&row_c, id_c.clone(), name_c.clone(), inner_c.clone());
        });
    }

    // Delete action.
    {
        let inner_c   = inner.clone();
        let id_c      = playlist_id.clone();
        let popover_c = popover.clone();
        delete_btn.connect_clicked(move |_| {
            popover_c.popdown();
            if let Some(cb) = &*inner_c.on_delete_playlist.borrow() {
                cb(id_c.clone());
            }
        });
    }

    attach_right_click_gesture(row, &popover);
}

fn show_rename_dialog(
    parent: &impl gtk4::prelude::IsA<gtk4::Widget>,
    playlist_id: String,
    current_name: String,
    inner: Rc<SidebarInner>,
) {
    let dialog = adw::AlertDialog::new(Some(&t("Rename Playlist")), None::<&str>);
    dialog.add_response("cancel", &t("Cancel"));
    dialog.add_response("rename", &t("Rename"));
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");

    let entry = gtk::Entry::builder()
        .text(&current_name)
        .activates_default(true)
        .margin_top(12)
        .build();
    // Pre-select all text so the user can type the new name immediately.
    entry.select_region(0, -1);
    dialog.set_extra_child(Some(&entry));

    dialog.connect_response(None, move |_, response| {
        if response != "rename" { return; }
        let new_name = entry.text().trim().to_string();
        if new_name.is_empty() || new_name == current_name { return; }
        if let Some(cb) = &*inner.on_rename_playlist.borrow() {
            cb(playlist_id.clone(), new_name);
        }
    });

    dialog.present(parent);
}

fn attach_right_click_gesture(row: &ListBoxRow, popover: &gtk::Popover) {
    let gesture = gtk::GestureClick::builder().button(3).build();
    gesture.connect_pressed({
        let popover_c = popover.clone();
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let rect = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            popover_c.set_pointing_to(Some(&rect));
            popover_c.popup();
        }
    });
    row.add_controller(gesture);
}

// ── Row factories ─────────────────────────────────────────────────────────────

/// Navigation row with a count badge on the right.
/// Returns `(row, count_label)` — the count label starts hidden.
fn make_nav_row_with_count(icon: &str, label: &str, name: &str) -> (ListBoxRow, Label) {
    let row_box = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(10)
        .margin_start(6).margin_end(6)
        .margin_top(6).margin_bottom(6)
        .build();

    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);

    let lbl = Label::builder()
        .label(label)
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let count_lbl = Label::builder()
        .halign(Align::End)
        .css_classes(vec!["dim-label", "caption"])
        .visible(false)
        .build();

    row_box.append(&image);
    row_box.append(&lbl);
    row_box.append(&count_lbl);

    let row = ListBoxRow::builder().child(&row_box).build();
    row.set_widget_name(name);
    row.set_cursor_from_name(Some("pointer"));

    (row, count_lbl)
}

/// Non-selectable section header with an optional action button on the right.
fn make_section_header(label: &str, btn: Option<&gtk::Button>) -> ListBoxRow {
    let row_box = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .margin_start(12).margin_end(4)
        .margin_top(12).margin_bottom(2)
        .build();

    let lbl = Label::builder()
        .label(label)
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["caption", "dim-label"])
        .build();
    row_box.append(&lbl);

    if let Some(b) = btn {
        row_box.append(b);
    }

    ListBoxRow::builder()
        .child(&row_box)
        .activatable(false)
        .selectable(false)
        .build()
}
