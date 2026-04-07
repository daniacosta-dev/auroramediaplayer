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
    page: NavigationPage,
    list: ListBox,
    folder_rows: RefCell<Vec<(PathBuf, ListBoxRow)>>,
    playlist_rows: RefCell<Vec<(String, ListBoxRow)>>,
    /// Non-selectable "Playlists [+]" section header — repositioned on update.
    playlist_header: ListBoxRow,
    on_filter: RefCell<Option<Box<dyn Fn(String)>>>,
    on_remove_folder: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    on_delete_playlist: RefCell<Option<Box<dyn Fn(String)>>>,
    on_new_playlist: RefCell<Option<Box<dyn Fn()>>>,
}

impl LibrarySidebar {
    pub fn new() -> Self {
        let list = ListBox::builder()
            .selection_mode(SelectionMode::Single)
            .css_classes(vec!["navigation-sidebar"])
            .build();

        // ── Fixed category rows ───────────────────────────────────────────
        let row_all   = make_nav_row("video-display-symbolic",   t("All Media"), "all");
        let row_video = make_nav_row("video-x-generic-symbolic", t("Videos"),    "video");
        let row_audio = make_nav_row("audio-x-generic-symbolic", t("Music"),     "audio");

        list.append(&row_all);
        list.append(&row_video);
        list.append(&row_audio);

        // ── "Folders" section header (always visible) ─────────────────────
        let folder_header = make_section_header(t("Folders"), None::<&gtk::Button>);
        list.append(&folder_header);

        // ── "Playlists [+]" section header ───────────────────────────────
        // The "+" button is wired after `inner` is created (below).
        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .css_classes(vec!["flat", "circular"])
            .tooltip_text(t("New playlist"))
            .build();
        add_btn.set_size_request(22, 22);
        add_btn.set_cursor_from_name(Some("pointer"));

        let playlist_header = make_section_header(t("Playlists"), Some(&add_btn));
        // Playlist header is appended by the first update_folders call; don't
        // append it here to avoid double-insertion.

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
            folder_rows: RefCell::new(Vec::new()),
            playlist_rows: RefCell::new(Vec::new()),
            playlist_header,
            on_filter: RefCell::new(None),
            on_remove_folder: RefCell::new(None),
            on_delete_playlist: RefCell::new(None),
            on_new_playlist: RefCell::new(None),
        });

        // Wire row selection — emits the row's widget_name via on_filter.
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

        // Wire "+" button after inner is constructed.
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

    pub fn connect_new_playlist<F: Fn() + 'static>(&self, f: F) {
        *self.inner.on_new_playlist.borrow_mut() = Some(Box::new(f));
    }

    /// Rebuild watched-folder rows.  Also repositions the playlist section.
    pub fn update_folders(&self, folders: Vec<PathBuf>) {
        // Detach the entire dynamic section so we can rebuild in order.
        for (_, row) in self.inner.folder_rows.borrow().iter() {
            self.inner.list.remove(row);
        }
        self.inner.list.remove(&self.inner.playlist_header);
        for (_, row) in self.inner.playlist_rows.borrow().iter() {
            self.inner.list.remove(row);
        }

        // Append new folder rows.
        let mut new_rows = Vec::new();
        for folder in folders {
            let label = folder.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Folder")
                .to_string();
            let row = make_nav_row("folder-symbolic", &label, &folder.to_string_lossy());
            attach_folder_context_menu(&row, folder.clone(), self.inner.clone());
            self.inner.list.append(&row);
            new_rows.push((folder, row));
        }
        *self.inner.folder_rows.borrow_mut() = new_rows;

        // Re-append playlist section in correct order.
        self.inner.list.append(&self.inner.playlist_header);
        for (_, row) in self.inner.playlist_rows.borrow().iter() {
            self.inner.list.append(row);
        }
    }

    /// Rebuild playlist rows (keeps playlist_header position stable).
    pub fn update_playlists(&self, playlists: Vec<Playlist>) {
        // Detach playlist section temporarily.
        self.inner.list.remove(&self.inner.playlist_header);
        for (_, row) in self.inner.playlist_rows.borrow().iter() {
            self.inner.list.remove(row);
        }

        // Re-append playlist section with new rows.
        self.inner.list.append(&self.inner.playlist_header);
        let mut new_rows = Vec::new();
        for pl in &playlists {
            let widget_name = format!("playlist:{}", pl.id);
            let row = make_nav_row("view-list-symbolic", &pl.name, &widget_name);
            attach_playlist_context_menu(&row, pl.id.clone(), self.inner.clone());
            self.inner.list.append(&row);
            new_rows.push((pl.id.clone(), row));
        }
        *self.inner.playlist_rows.borrow_mut() = new_rows;
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

fn attach_playlist_context_menu(row: &ListBoxRow, playlist_id: String, inner: Rc<SidebarInner>) {
    let delete_btn = gtk::Button::builder()
        .label(t("Delete playlist"))
        .css_classes(vec!["flat", "destructive-action"])
        .halign(Align::Fill)
        .build();

    let popover_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4)
        .margin_start(4).margin_end(4)
        .build();
    popover_box.append(&delete_btn);

    let popover = gtk::Popover::builder()
        .child(&popover_box)
        .has_arrow(false)
        .build();
    popover.set_parent(row);

    let popover_c = popover.clone();
    delete_btn.connect_clicked(move |_| {
        popover_c.popdown();
        if let Some(cb) = &*inner.on_delete_playlist.borrow() {
            cb(playlist_id.clone());
        }
    });

    attach_right_click_gesture(row, &popover);
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

/// Standard navigation row: icon + label, selectable.
fn make_nav_row(icon: &str, label: &str, name: &str) -> ListBoxRow {
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

    row_box.append(&image);
    row_box.append(&lbl);

    let row = ListBoxRow::builder().child(&row_box).build();
    row.set_widget_name(name);
    row.set_cursor_from_name(Some("pointer"));
    row
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
