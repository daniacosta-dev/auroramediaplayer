use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::NavigationPage;
use gtk4::{self as gtk, FlowBox, FlowBoxChild, Label, ScrolledWindow,
           Orientation, Align, SelectionMode};
use gtk4::prelude::*;

use crate::i18n::t;
use crate::library::{MediaItem, MediaKind};

// ── Filter kind ───────────────────────────────────────────────────────────────

// Stored in a Cell so the filter_func closure (which must be 'static) can read it.
// 0 = all, 1 = video, 2 = audio
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
    items: RefCell<Vec<MediaItem>>,
    on_activated: RefCell<Option<std::boxed::Box<dyn Fn(PathBuf)>>>,
}

impl MediaGrid {
    pub fn new() -> Self {
        let filter_kind: Rc<Cell<u8>> = Rc::new(Cell::new(FILTER_ALL));

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

        // Install the filter function once — just reads filter_kind, never rebuilds.
        let fk = filter_kind.clone();
        flow.set_filter_func(move |child| {
            match fk.get() {
                FILTER_VIDEO => child.widget_name() == "video",
                FILTER_AUDIO => child.widget_name() == "audio",
                _            => true,
            }
        });

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
            items: RefCell::new(Vec::new()),
            on_activated: RefCell::new(None),
        });

        // Activation: index into the full items vec (filter hides, doesn't remove).
        inner.flow.connect_child_activated({
            let inner_c = inner.clone();
            move |_, child| {
                let idx = child.index() as usize;
                let items = inner_c.items.borrow();
                if let Some(item) = items.get(idx) {
                    let path = item.path.clone();
                    drop(items);
                    if let Some(cb) = &*inner_c.on_activated.borrow() {
                        cb(path);
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

    pub fn connect_item_activated<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.inner.on_activated.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Populate the grid with a new item list (called on load / rescan only).
    /// Preserves the current filter.
    pub fn show_items(&self, items: Vec<MediaItem>) {
        while let Some(child) = self.inner.flow.first_child() {
            self.inner.flow.remove(&child);
        }
        for item in &items {
            let card = make_card(item);
            self.inner.flow.insert(&card, -1);
        }
        *self.inner.items.borrow_mut() = items;
        // Re-apply current filter to newly inserted cards.
        self.inner.flow.invalidate_filter();
    }

    /// Update just one card's thumbnail after async probe completes.
    /// Replaces the single FlowBoxChild at the item's index — no full rebuild.
    pub fn update_item_thumbnail(&self, path: &std::path::Path, thumb_path: std::path::PathBuf) {
        let idx = self.inner.items.borrow().iter().position(|i| i.path == path);
        let Some(idx) = idx else { return };

        self.inner.items.borrow_mut()[idx].thumbnail_path = Some(thumb_path);

        if let Some(old_child) = self.inner.flow.child_at_index(idx as i32) {
            let new_card = {
                let items = self.inner.items.borrow();
                make_card(&items[idx])
            };
            self.inner.flow.remove(&old_child);
            self.inner.flow.insert(&new_card, idx as i32);
            self.inner.flow.invalidate_filter();
        }
    }

    /// Switch the active filter — O(1), no widget rebuild.
    pub fn apply_filter(&self, filter: &str) {
        let kind = match filter {
            "video" => FILTER_VIDEO,
            "audio" => FILTER_AUDIO,
            _       => FILTER_ALL,
        };
        self.inner.filter_kind.set(kind);
        self.inner.flow.invalidate_filter();
    }
}

// ── Card factory ─────────────────────────────────────────────────────────────

fn make_card(item: &MediaItem) -> FlowBoxChild {
    let card_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .width_request(160)
        .css_classes(vec!["library-card"])
        .build();

    // ── Thumbnail area ────────────────────────────────────────────────────
    // The Overlay is fixed at 160×100.  Thumbnails are extracted at that exact
    // size by ffmpeg, so Picture's natural size == 160×100 and the FlowBox can
    // pack multiple cards per row correctly.
    let thumb_overlay = gtk::Overlay::new();
    thumb_overlay.set_size_request(160, 100);

    let thumb_widget: gtk::Widget = if let Some(thumb_path) = &item.thumbnail_path {
        let pic = gtk::Picture::for_filename(thumb_path);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_can_shrink(true);
        pic.set_size_request(160, 100);
        pic.upcast()
    } else {
        make_placeholder(item).upcast()
    };
    thumb_overlay.set_child(Some(&thumb_widget));

    // Duration badge (bottom-right)
    if let Some(dur) = item.duration_secs {
        let dur_lbl = Label::builder()
            .label(&fmt_duration(dur as u64))
            .css_classes(vec!["library-badge", "library-badge-duration"])
            .halign(Align::End)
            .valign(Align::End)
            .margin_end(6)
            .margin_bottom(6)
            .build();
        thumb_overlay.add_overlay(&dur_lbl);
    }

    card_box.append(&thumb_overlay);

    // ── Metadata area ─────────────────────────────────────────────────────
    let meta_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .margin_start(8)
        .margin_end(8)
        .margin_top(6)
        .margin_bottom(8)
        .build();

    let title_lbl = Label::builder()
        .label(&item.title)
        .halign(Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(20)
        .css_classes(vec!["library-card-title"])
        .build();
    meta_box.append(&title_lbl);

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
            .max_width_chars(20)
            .css_classes(vec!["library-card-subtitle", "dim-label"])
            .build();
        meta_box.append(&sub_lbl);
    }

    card_box.append(&meta_box);

    // Tag the child with its kind so the filter_func can read it in O(1).
    let kind_name = match item.kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    };
    FlowBoxChild::builder()
        .child(&card_box)
        .css_classes(vec!["library-card-child"])
        .build()
        .tap(|c| c.set_widget_name(kind_name))
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
    icon.set_pixel_size(40);
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
    placeholder.set_size_request(160, 100);
    placeholder.append(&icon);
    placeholder
}

fn fmt_duration(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
