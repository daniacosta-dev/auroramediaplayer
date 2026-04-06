use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::NavigationPage;
use gtk4::{self as gtk, Label, ListBox, ListBoxRow, ScrolledWindow,
           Separator, Orientation, SelectionMode, Align};
use gtk4::prelude::*;

use crate::i18n::t;

// ── LibrarySidebar ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct LibrarySidebar {
    inner: Rc<SidebarInner>,
}

struct SidebarInner {
    page: NavigationPage,
    list: ListBox,
    folder_rows: RefCell<Vec<(PathBuf, ListBoxRow)>>,
    on_filter: RefCell<Option<std::boxed::Box<dyn Fn(String)>>>,
    on_remove_folder: RefCell<Option<std::boxed::Box<dyn Fn(PathBuf)>>>,
}

impl LibrarySidebar {
    pub fn new() -> Self {
        let list = ListBox::builder()
            .selection_mode(SelectionMode::Single)
            .css_classes(vec!["navigation-sidebar"])
            .build();

        let row_all   = make_category_row("video-display-symbolic",  t("All Media"), "all");
        let row_video = make_category_row("video-x-generic-symbolic", t("Videos"),   "video");
        let row_audio = make_category_row("audio-x-generic-symbolic", t("Music"),    "audio");

        list.append(&row_all);
        list.append(&row_video);
        list.append(&row_audio);

        let sep = Separator::new(Orientation::Horizontal);
        sep.set_margin_top(6);
        sep.set_margin_bottom(6);
        list.append(&sep);

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
            on_filter: RefCell::new(None),
            on_remove_folder: RefCell::new(None),
        });

        inner.list.connect_row_selected({
            let inner_c = inner.clone();
            move |_, row| {
                if let Some(row) = row {
                    let filter = row.widget_name().to_string();
                    if let Some(cb) = &*inner_c.on_filter.borrow() {
                        cb(filter);
                    }
                }
            }
        });

        Self { inner }
    }

    pub fn page(&self) -> &NavigationPage {
        &self.inner.page
    }

    pub fn clone_ref(&self) -> Self {
        Self { inner: self.inner.clone() }
    }

    pub fn connect_filter_changed<F: Fn(String) + 'static>(&self, f: F) {
        *self.inner.on_filter.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Callback invoked when the user removes a folder via right-click menu.
    pub fn connect_remove_folder<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.inner.on_remove_folder.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Rebuild the watched-folder rows to match the given list.
    pub fn update_folders(&self, folders: Vec<PathBuf>) {
        for (_, row) in self.inner.folder_rows.borrow().iter() {
            self.inner.list.remove(row);
        }

        let mut new_rows = Vec::new();
        for folder in folders {
            let name = folder.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Folder")
                .to_string();

            let row = make_folder_row(&name, &folder.to_string_lossy());

            // ── Right-click context menu ──────────────────────────────────
            attach_folder_context_menu(&row, folder.clone(), self.inner.clone());

            self.inner.list.append(&row);
            new_rows.push((folder, row));
        }
        *self.inner.folder_rows.borrow_mut() = new_rows;
    }
}

// ── Context menu ──────────────────────────────────────────────────────────────

fn attach_folder_context_menu(row: &ListBoxRow, folder: PathBuf, inner: Rc<SidebarInner>) {
    // Build a small popover with a single "Remove" button.
    let remove_btn = gtk::Button::builder()
        .label(t("Remove from library"))
        .css_classes(vec!["flat", "destructive-action"])
        .halign(Align::Fill)
        .build();

    let popover_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    popover_box.append(&remove_btn);

    let popover = gtk::Popover::builder()
        .child(&popover_box)
        .has_arrow(false)
        .build();
    popover.set_parent(row);

    // Wire the remove button.
    {
        let popover_c = popover.clone();
        let folder_c  = folder.clone();
        remove_btn.connect_clicked(move |_| {
            popover_c.popdown();
            if let Some(cb) = &*inner.on_remove_folder.borrow() {
                cb(folder_c.clone());
            }
        });
    }

    // Right-click (button 3) on the row → show popover at pointer position.
    let gesture = gtk::GestureClick::builder()
        .button(3) // right mouse button
        .build();

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

fn make_category_row(icon: &str, label: &str, name: &str) -> ListBoxRow {
    let row_box = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(10)
        .margin_start(6)
        .margin_end(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);

    let lbl = Label::builder()
        .label(label)
        .halign(Align::Start)
        .hexpand(true)
        .build();

    row_box.append(&image);
    row_box.append(&lbl);

    let row = ListBoxRow::builder().child(&row_box).build();
    row.set_widget_name(name);
    row
}

fn make_folder_row(label: &str, name: &str) -> ListBoxRow {
    let row_box = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(10)
        .margin_start(6)
        .margin_end(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let image = gtk::Image::from_icon_name("folder-symbolic");
    image.set_pixel_size(16);

    let lbl = Label::builder()
        .label(label)
        .halign(Align::Start)
        .hexpand(true)
        .build();

    row_box.append(&image);
    row_box.append(&lbl);

    let row = ListBoxRow::builder().child(&row_box).build();
    row.set_widget_name(name);
    row
}
