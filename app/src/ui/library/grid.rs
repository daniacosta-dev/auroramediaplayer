use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::NavigationPage;
use gtk4::{self as gtk, FlowBox, FlowBoxChild, Label, ScrolledWindow,
           Orientation, Align, SelectionMode};
use gtk4::prelude::*;

use crate::i18n::t;
use crate::library::{MediaItem, MediaKind, Playlist};

// ── Filter kind ───────────────────────────────────────────────────────────────

const FILTER_ALL: u8   = 0;
const FILTER_VIDEO: u8 = 1;
const FILTER_AUDIO: u8 = 2;

// ── MediaGrid ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MediaGrid {
    inner: Rc<GridInner>,
}

struct GridInner {
    page: NavigationPage,
    flow: FlowBox,
    filter_kind: Rc<Cell<u8>>,
    /// When Some, only show cards whose item path is in this set.
    filter_playlist: Rc<RefCell<Option<HashSet<PathBuf>>>>,
    /// Full ordered item list — matches FlowBox child order at all times.
    items: Rc<RefCell<Vec<MediaItem>>>,
    playlists: RefCell<Vec<Playlist>>,
    on_activated: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    on_add_to_playlist: RefCell<Option<Box<dyn Fn(PathBuf, String)>>>,
    on_new_playlist: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
}

impl MediaGrid {
    pub fn new() -> Self {
        let filter_kind: Rc<Cell<u8>> = Rc::new(Cell::new(FILTER_ALL));
        let filter_playlist: Rc<RefCell<Option<HashSet<PathBuf>>>> =
            Rc::new(RefCell::new(None));
        let items: Rc<RefCell<Vec<MediaItem>>> = Rc::new(RefCell::new(Vec::new()));

        let flow = FlowBox::builder()
            .valign(Align::Start)
            .halign(Align::Fill)
            .selection_mode(SelectionMode::None)
            .homogeneous(true)
            .column_spacing(12)
            .row_spacing(12)
            .margin_start(16)
            .margin_end(16)
            .margin_top(16)
            .margin_bottom(16)
            .build();

        // Filter function installed once — reads both filters without rebuilding.
        {
            let fk = filter_kind.clone();
            let fp = filter_playlist.clone();
            let items_f = items.clone();
            flow.set_filter_func(move |child| {
                let idx = child.index() as usize;
                // Playlist filter: hide items not in the active playlist.
                if let Some(paths) = fp.borrow().as_ref() {
                    match items_f.borrow().get(idx) {
                        Some(item) if paths.contains(&item.path) => {}
                        _ => return false,
                    }
                }
                // Kind filter.
                match fk.get() {
                    FILTER_VIDEO => child.widget_name() == "video",
                    FILTER_AUDIO => child.widget_name() == "audio",
                    _            => true,
                }
            });
        }

        let scroll = ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&flow)
            .build();

        let page = NavigationPage::builder()
            .title(t("All Media"))
            .tag("library-grid")
            .child(&scroll)
            .build();

        let inner = Rc::new(GridInner {
            page,
            flow,
            filter_kind,
            filter_playlist,
            items,
            playlists: RefCell::new(Vec::new()),
            on_activated: RefCell::new(None),
            on_add_to_playlist: RefCell::new(None),
            on_new_playlist: RefCell::new(None),
        });

        inner.flow.connect_child_activated({
            let inner_c = inner.clone();
            move |_, child| {
                let idx = child.index() as usize;
                let path = inner_c.items.borrow().get(idx).map(|i| i.path.clone());
                if let Some(path) = path {
                    if let Some(cb) = &*inner_c.on_activated.borrow() {
                        cb(path);
                    }
                }
            }
        });

        Self { inner }
    }

    pub fn page(&self) -> &NavigationPage { &self.inner.page }

    pub fn clone_ref(&self) -> Self { Self { inner: self.inner.clone() } }

    pub fn connect_item_activated<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.inner.on_activated.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_add_to_playlist<F: Fn(PathBuf, String) + 'static>(&self, f: F) {
        *self.inner.on_add_to_playlist.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_new_playlist<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.inner.on_new_playlist.borrow_mut() = Some(Box::new(f));
    }

    /// Update the playlist list used to build the right-click popover.
    pub fn set_playlists(&self, playlists: Vec<Playlist>) {
        *self.inner.playlists.borrow_mut() = playlists;
    }

    /// Populate the grid with a new item list (library load / rescan).
    /// Clears all active filters.
    pub fn show_items(&self, items_vec: Vec<MediaItem>) {
        while let Some(child) = self.inner.flow.first_child() {
            self.inner.flow.remove(&child);
        }
        for item in &items_vec {
            let card = make_card(item);
            attach_context_menu(&card, item.path.clone(), self.inner.clone());
            self.inner.flow.insert(&card, -1);
        }
        *self.inner.items.borrow_mut() = items_vec;
        self.inner.filter_kind.set(FILTER_ALL);
        *self.inner.filter_playlist.borrow_mut() = None;
        self.inner.flow.invalidate_filter();
    }

    /// Update just one card's thumbnail after async probe completes.
    pub fn update_item_thumbnail(&self, path: &std::path::Path, thumb_path: PathBuf) {
        let idx = self.inner.items.borrow().iter().position(|i| i.path == path);
        let Some(idx) = idx else { return };

        self.inner.items.borrow_mut()[idx].thumbnail_path = Some(thumb_path);

        if let Some(old_child) = self.inner.flow.child_at_index(idx as i32) {
            let new_card = {
                let items = self.inner.items.borrow();
                make_card(&items[idx])
            };
            let new_path = self.inner.items.borrow()[idx].path.clone();
            self.inner.flow.remove(&old_child);
            self.inner.flow.insert(&new_card, idx as i32);
            attach_context_menu(&new_card, new_path, self.inner.clone());
            self.inner.flow.invalidate_filter();
        }
    }

    /// Switch the kind filter (all / video / audio) — O(1), no rebuild.
    pub fn apply_filter(&self, filter: &str) {
        let kind = match filter {
            "video" => FILTER_VIDEO,
            "audio" => FILTER_AUDIO,
            _       => FILTER_ALL,
        };
        self.inner.filter_kind.set(kind);
        *self.inner.filter_playlist.borrow_mut() = None;
        self.inner.flow.invalidate_filter();
    }

    /// Show only items whose path is in `paths` — O(1), no rebuild.
    pub fn apply_playlist_filter(&self, paths: Vec<PathBuf>) {
        let set: HashSet<PathBuf> = paths.into_iter().collect();
        *self.inner.filter_playlist.borrow_mut() = Some(set);
        self.inner.filter_kind.set(FILTER_ALL);
        self.inner.flow.invalidate_filter();
    }
}

// ── Right-click context menu ──────────────────────────────────────────────────

fn attach_context_menu(child: &FlowBoxChild, path: PathBuf, inner: Rc<GridInner>) {
    // Content is rebuilt on each show so the playlist list is always fresh.
    let content_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    content_box.set_size_request(190, -1);

    let popover = gtk::Popover::builder()
        .child(&content_box)
        .has_arrow(false)
        .build();
    popover.set_parent(child);

    popover.connect_show({
        let inner_c = inner.clone();
        let path_c = path.clone();
        let content_ref = content_box.clone();
        let popover_weak = popover.downgrade();
        move |_| {
            // Clear previous content.
            while let Some(w) = content_ref.first_child() {
                content_ref.remove(&w);
            }

            let playlists = inner_c.playlists.borrow().clone();

            if !playlists.is_empty() {
                // Section header
                let header = Label::builder()
                    .label(t("Add to playlist"))
                    .halign(Align::Start)
                    .css_classes(vec!["caption", "dim-label"])
                    .margin_start(8)
                    .margin_bottom(2)
                    .build();
                content_ref.append(&header);

                for pl in &playlists {
                    let lbl = Label::builder()
                        .label(&pl.name)
                        .halign(Align::Start)
                        .hexpand(true)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .max_width_chars(24)
                        .build();
                    let btn = gtk::Button::builder()
                        .child(&lbl)
                        .css_classes(vec!["flat"])
                        .build();

                    let inner_btn = inner_c.clone();
                    let path_btn = path_c.clone();
                    let pl_id = pl.id.clone();
                    let pw = popover_weak.clone();
                    btn.connect_clicked(move |_| {
                        if let Some(p) = pw.upgrade() { p.popdown(); }
                        if let Some(cb) = &*inner_btn.on_add_to_playlist.borrow() {
                            cb(path_btn.clone(), pl_id.clone());
                        }
                    });
                    content_ref.append(&btn);
                }

                let sep = gtk::Separator::new(Orientation::Horizontal);
                sep.set_margin_top(4);
                sep.set_margin_bottom(4);
                content_ref.append(&sep);
            }

            // "New playlist…" button with icon
            let new_icon = gtk::Image::from_icon_name("list-add-symbolic");
            let new_lbl = Label::builder()
                .label(t("New playlist…"))
                .halign(Align::Start)
                .hexpand(true)
                .build();
            let new_row = gtk::Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .build();
            new_row.append(&new_icon);
            new_row.append(&new_lbl);

            let new_btn = gtk::Button::builder()
                .child(&new_row)
                .css_classes(vec!["flat"])
                .build();

            let inner_new = inner_c.clone();
            let path_new = path_c.clone();
            let pw2 = popover_weak.clone();
            new_btn.connect_clicked(move |_| {
                if let Some(p) = pw2.upgrade() { p.popdown(); }
                if let Some(cb) = &*inner_new.on_new_playlist.borrow() {
                    cb(path_new.clone());
                }
            });
            content_ref.append(&new_btn);
        }
    });

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
    child.add_controller(gesture);
}

// ── Card factory ──────────────────────────────────────────────────────────────

fn make_card(item: &MediaItem) -> FlowBoxChild {
    let card_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .width_request(200)
        .css_classes(vec!["library-card"])
        .build();

    // Thumbnail area — fixed at 200×120.
    let thumb_overlay = gtk::Overlay::new();
    thumb_overlay.set_size_request(200, 120);

    let thumb_widget: gtk::Widget = if let Some(thumb_path) = &item.thumbnail_path {
        let pic = gtk::Picture::for_filename(thumb_path);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_can_shrink(true);
        pic.set_size_request(200, 120);
        pic.add_css_class("library-card-picture");
        pic.upcast()
    } else {
        make_placeholder(item).upcast()
    };
    thumb_overlay.set_child(Some(&thumb_widget));

    // Bottom gradient overlay with title + subtitle.
    let overlay_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .valign(Align::End)
        .halign(Align::Fill)
        .css_classes(vec!["library-card-overlay"])
        .build();

    let title_lbl = Label::builder()
        .label(&item.title)
        .halign(Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(22)
        .css_classes(vec!["library-card-title"])
        .build();
    overlay_box.append(&title_lbl);

    let subtitle = match item.kind {
        MediaKind::Audio => item.artist.clone()
            .or_else(|| item.album.clone())
            .unwrap_or_default(),
        MediaKind::Video => item.path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
    };
    if !subtitle.is_empty() {
        let sub_lbl = Label::builder()
            .label(&subtitle)
            .halign(Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(22)
            .css_classes(vec!["library-card-subtitle"])
            .build();
        overlay_box.append(&sub_lbl);
    }

    thumb_overlay.add_overlay(&overlay_box);

    // Duration badge (top-right corner).
    if let Some(dur) = item.duration_secs {
        let dur_lbl = Label::builder()
            .label(&fmt_duration(dur as u64))
            .css_classes(vec!["library-badge", "library-badge-duration"])
            .halign(Align::End)
            .valign(Align::Start)
            .margin_end(6)
            .margin_top(6)
            .build();
        thumb_overlay.add_overlay(&dur_lbl);
    }

    card_box.append(&thumb_overlay);

    let kind_name = match item.kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    };
    FlowBoxChild::builder()
        .child(&card_box)
        .css_classes(vec!["library-card-child"])
        .build()
        .tap(|c| {
            c.set_widget_name(kind_name);
            c.set_cursor_from_name(Some("pointer"));
        })
}

trait Tap: Sized {
    fn tap<F: FnOnce(&Self)>(self, f: F) -> Self { f(&self); self }
}
impl<T> Tap for T {}

fn make_placeholder(item: &MediaItem) -> gtk::Box {
    let icon_name = match item.kind {
        MediaKind::Video => "video-x-generic-symbolic",
        MediaKind::Audio => "audio-x-generic-symbolic",
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(48);
    icon.set_halign(Align::Center);
    icon.set_valign(Align::Center);
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    icon.add_css_class("dim-label");

    let placeholder = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .halign(Align::Fill)
        .valign(Align::Fill)
        .css_classes(vec!["library-card-placeholder"])
        .build();
    placeholder.set_size_request(200, 120);
    placeholder.append(&icon);
    placeholder
}

fn fmt_duration(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
