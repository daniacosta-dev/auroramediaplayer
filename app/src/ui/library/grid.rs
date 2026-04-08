use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::NavigationPage;
use gtk4::{self as gtk, FlowBox, FlowBoxChild, Label, ScrolledWindow,
           Orientation, Align, SelectionMode};
use gtk4::glib;
use gtk4::prelude::*;

use crate::i18n::t;
use crate::library::{MediaItem, MediaKind, Playlist};

// ── Is-stream helper ──────────────────────────────────────────────────────────

fn is_stream_url(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("http://") || s.starts_with("https://")
}

fn url_hostname(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split('/').next())
        .and_then(|host| host.split('?').next())
        .unwrap_or(url)
        .to_string()
}

// ── Filter constants ──────────────────────────────────────────────────────────

const FILTER_ALL: u8   = 0;
const FILTER_VIDEO: u8 = 1;
const FILTER_AUDIO: u8 = 2;

// ── Sort constants ────────────────────────────────────────────────────────────

const SORT_TITLE: u8           = 0;
const SORT_DATE_ADDED: u8      = 1;
const SORT_RECENTLY_PLAYED: u8 = 2;
const SORT_DURATION: u8        = 3;

// ── View-stack page names ─────────────────────────────────────────────────────

const PAGE_ITEMS:          &str = "items";
const PAGE_EMPTY_LIBRARY:  &str = "empty-library";
const PAGE_EMPTY_RESULTS:  &str = "empty-results";

// ── Widget-name encoding ──────────────────────────────────────────────────────
//
// Each FlowBoxChild carries its ORIGINAL insertion index and kind encoded as
// "<kind>:<idx>" in its widget_name.  This decouples look-ups from
// `child.index()`, which changes whenever FlowBox re-sorts and therefore
// cannot be used to index into the `items` Vec.

fn encode_name(kind: &str, idx: usize) -> String {
    format!("{kind}:{idx}")
}

fn decode_name(name: &str) -> (&str, usize) {
    let mut parts = name.splitn(2, ':');
    let kind = parts.next().unwrap_or("");
    let idx  = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (kind, idx)
}

// ── MediaGrid ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MediaGrid {
    inner: Rc<GridInner>,
}

struct GridInner {
    page:                  NavigationPage,
    flow:                  FlowBox,
    view_stack:            gtk::Stack,
    search_entry:          gtk::SearchEntry,
    sort_btn:              gtk::MenuButton,
    sort_labels:           Vec<Label>,
    empty_library:         adw::StatusPage,
    empty_results:         adw::StatusPage,
    filter_kind:           Rc<Cell<u8>>,
    filter_playlist:       Rc<RefCell<Option<HashSet<PathBuf>>>>,
    filter_folder:         Rc<RefCell<Option<PathBuf>>>,
    filter_search:         Rc<RefCell<String>>,
    filter_recent_only:    Rc<Cell<bool>>,
    filter_include_streams: Rc<Cell<bool>>,
    sort_order:            Rc<Cell<u8>>,
    now_playing_path:      Rc<RefCell<Option<PathBuf>>>,
    items:                 Rc<RefCell<Vec<MediaItem>>>,
    playlists:             RefCell<Vec<Playlist>>,
    on_activated:          RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    on_add_to_playlist:    RefCell<Option<Box<dyn Fn(PathBuf, String)>>>,
    on_new_playlist:       RefCell<Option<Box<dyn Fn(PathBuf)>>>,
}

impl MediaGrid {
    pub fn new() -> Self {
        let filter_kind           = Rc::new(Cell::new(FILTER_ALL));
        let filter_playlist       = Rc::new(RefCell::new(None::<HashSet<PathBuf>>));
        let filter_folder         = Rc::new(RefCell::new(None::<PathBuf>));
        let filter_search         = Rc::new(RefCell::new(String::new()));
        let filter_recent_only    = Rc::new(Cell::new(false));
        let filter_include_streams = Rc::new(Cell::new(false));
        let sort_order            = Rc::new(Cell::new(SORT_TITLE));
        let now_playing_path      = Rc::new(RefCell::new(None::<PathBuf>));
        let items                 = Rc::new(RefCell::new(Vec::<MediaItem>::new()));

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

        // ── Filter: kind + playlist + folder + search + recent + streams ─
        // Uses the original insertion index stored in widget_name — NOT child.index(),
        // which reflects visual (sorted) position and maps to the wrong item.
        {
            let fk  = filter_kind.clone();
            let fp  = filter_playlist.clone();
            let ff  = filter_folder.clone();
            let fs  = filter_search.clone();
            let fr  = filter_recent_only.clone();
            let fis = filter_include_streams.clone();
            let items_f = items.clone();
            flow.set_filter_func(move |child| {
                let _wn = child.widget_name(); let (kind, idx) = decode_name(&_wn);
                let items = items_f.borrow();
                let Some(item) = items.get(idx) else { return false };

                // Stream items are hidden unless we're in a URL playlist view.
                if kind == "stream" && !fis.get() { return false; }

                // Recently-played filter.
                if fr.get() && item.last_played.is_none() { return false; }

                // Playlist filter (path-based).
                if let Some(paths) = fp.borrow().as_ref() {
                    if !paths.contains(&item.path) { return false; }
                }

                // Folder filter.
                if let Some(folder) = ff.borrow().as_ref() {
                    if !item.path.starts_with(folder.as_path()) { return false; }
                }

                // Kind filter. Streams are only allowed when fis flag is set
                // (i.e. a URL playlist is selected); the fis check above already
                // blocks them otherwise, so here we just need to not double-block.
                let kind_ok = match fk.get() {
                    FILTER_VIDEO => kind == "video",
                    FILTER_AUDIO => kind == "audio",
                    _ => kind != "stream" || fis.get(),
                };
                if !kind_ok { return false; }

                // Search filter (title / artist / album, case-insensitive).
                let query = fs.borrow();
                if !query.is_empty() {
                    let q = query.to_lowercase();
                    let hit = item.title.to_lowercase().contains(q.as_str())
                        || item.artist.as_deref()
                            .map(|a| a.to_lowercase().contains(q.as_str()))
                            .unwrap_or(false)
                        || item.album.as_deref()
                            .map(|a| a.to_lowercase().contains(q.as_str()))
                            .unwrap_or(false);
                    if !hit { return false; }
                }

                true
            });
        }

        // ── Sort ─────────────────────────────────────────────────────────
        {
            let so = sort_order.clone();
            let items_s = items.clone();
            flow.set_sort_func(move |a, b| {
                let (_, ia) = decode_name(&a.widget_name());
                let (_, ib) = decode_name(&b.widget_name());
                let items = items_s.borrow();
                match (items.get(ia), items.get(ib)) {
                    (Some(a), Some(b)) => compare_items(a, b, so.get()),
                    _ => gtk4::Ordering::Equal,
                }
            });
        }

        // ── Toolbar: search + sort ────────────────────────────────────────
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(t("Search…"))
            .hexpand(true)
            .build();

        let sort_popover = gtk::Popover::new();
        let sort_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .margin_top(4).margin_bottom(4)
            .margin_start(4).margin_end(4)
            .spacing(2)
            .build();
        sort_box.set_size_request(180, -1);

        let sort_entries: &[(u8, &str, &'static str)] = &[
            (SORT_TITLE,           "view-sort-ascending-symbolic",    t("Title")),
            (SORT_DATE_ADDED,      "document-open-recent-symbolic",   t("Date Added")),
            (SORT_RECENTLY_PLAYED, "media-playback-start-symbolic",   t("Recently Played")),
            (SORT_DURATION,        "preferences-system-time-symbolic", t("Duration")),
        ];

        let mut sort_labels: Vec<Label> = Vec::new();
        for &(order, icon, label) in sort_entries {
            let row = gtk::Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .build();
            row.append(&gtk::Image::from_icon_name(icon));
            let lbl = Label::builder()
                .label(label)
                .halign(Align::Start)
                .hexpand(true)
                .build();
            row.append(&lbl);
            sort_labels.push(lbl);
            let btn = gtk::Button::builder()
                .child(&row)
                .css_classes(vec!["flat"])
                .build();
            let so = sort_order.clone();
            let flow_c = flow.clone();
            let pw = sort_popover.downgrade();
            btn.connect_clicked(move |_| {
                so.set(order);
                flow_c.invalidate_sort();
                if let Some(p) = pw.upgrade() { p.popdown(); }
            });
            sort_box.append(&btn);
        }
        sort_popover.set_child(Some(&sort_box));

        let sort_btn = gtk::MenuButton::builder()
            .icon_name("view-sort-ascending-symbolic")
            .tooltip_text(t("Sort by"))
            .popover(&sort_popover)
            .css_classes(vec!["flat"])
            .build();

        let toolbar_row = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .margin_start(12)
            .margin_end(6)
            .margin_top(8)
            .margin_bottom(0)
            .build();
        toolbar_row.append(&search_entry);
        toolbar_row.append(&sort_btn);

        let scroll = ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&flow)
            .build();

        let content_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .build();
        // ── View stack: grid / empty states ──────────────────────────────
        let view_stack = gtk::Stack::new();
        view_stack.set_vexpand(true);
        view_stack.add_named(&scroll, Some(PAGE_ITEMS));

        let empty_library = adw::StatusPage::builder()
            .icon_name("folder-music-symbolic")
            .title(t("No media yet"))
            .description(t("Add a folder using the button above to get started."))
            .build();
        view_stack.add_named(&empty_library, Some(PAGE_EMPTY_LIBRARY));

        let empty_results = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title(t("No results"))
            .description(t("Try a different search or filter."))
            .build();
        view_stack.add_named(&empty_results, Some(PAGE_EMPTY_RESULTS));

        content_box.append(&toolbar_row);
        content_box.append(&view_stack);

        let page = NavigationPage::builder()
            .title(t("All Media"))
            .tag("library-grid")
            .child(&content_box)
            .build();

        let inner = Rc::new(GridInner {
            page,
            flow,
            view_stack,
            search_entry,
            sort_btn,
            sort_labels,
            empty_library,
            empty_results,
            filter_kind,
            filter_playlist,
            filter_folder,
            filter_search,
            filter_recent_only,
            filter_include_streams,
            sort_order,
            now_playing_path,
            items,
            playlists:          RefCell::new(Vec::new()),
            on_activated:       RefCell::new(None),
            on_add_to_playlist: RefCell::new(None),
            on_new_playlist:    RefCell::new(None),
        });

        // Wire flow activation — decode original index from widget_name.
        inner.flow.connect_child_activated({
            let inner_c = inner.clone();
            move |_, child| {
                let (_, idx) = decode_name(&child.widget_name());
                let path = inner_c.items.borrow().get(idx).map(|i| i.path.clone());
                if let Some(path) = path {
                    if let Some(cb) = &*inner_c.on_activated.borrow() {
                        cb(path);
                    }
                }
            }
        });

        // Wire search entry.
        inner.search_entry.connect_search_changed({
            let inner_c = inner.clone();
            move |entry| {
                *inner_c.filter_search.borrow_mut() = entry.text().to_string();
                inner_c.flow.invalidate_filter();
                schedule_empty_state_update(inner_c.clone());
            }
        });

        Self { inner }
    }

    pub fn page(&self) -> &NavigationPage { &self.inner.page }

    pub fn relabel(&self) {
        use crate::i18n::t;
        self.inner.page.set_title(t("All Media"));
        self.inner.search_entry.set_placeholder_text(Some(t("Search…")));
        self.inner.sort_btn.set_tooltip_text(Some(t("Sort by")));
        self.inner.empty_library.set_title(t("No media yet"));
        self.inner.empty_library.set_description(Some(t("Add a folder using the button above to get started.")));
        self.inner.empty_results.set_title(t("No results"));
        self.inner.empty_results.set_description(Some(t("Try a different search or filter.")));
        // Sort popover labels: Title, Date Added, Recently Played, Duration
        let sort_keys = [t("Title"), t("Date Added"), t("Recently Played"), t("Duration")];
        for (lbl, text) in self.inner.sort_labels.iter().zip(sort_keys.iter()) {
            lbl.set_label(text);
        }
    }

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

    pub fn set_playlists(&self, playlists: Vec<Playlist>) {
        *self.inner.playlists.borrow_mut() = playlists;
    }

    /// Populate the grid with a new item list (library load / rescan).
    /// Resets kind and playlist filters; preserves search text and sort order.
    pub fn show_items(&self, items_vec: Vec<MediaItem>) {
        while let Some(child) = self.inner.flow.first_child() {
            self.inner.flow.remove(&child);
        }
        let playing = self.inner.now_playing_path.borrow().clone();
        for (idx, item) in items_vec.iter().enumerate() {
            let is_playing = playing.as_deref() == Some(item.path.as_path());
            let card = make_card(item, idx, is_playing);
            attach_context_menu(&card, item.path.clone(), self.inner.clone());
            self.inner.flow.insert(&card, -1);
        }
        *self.inner.items.borrow_mut() = items_vec;
        self.inner.filter_kind.set(FILTER_ALL);
        self.inner.filter_recent_only.set(false);
        self.inner.filter_include_streams.set(false);
        *self.inner.filter_playlist.borrow_mut() = None;
        *self.inner.filter_folder.borrow_mut() = None;
        self.inner.flow.invalidate_filter();
        self.inner.flow.invalidate_sort();
        schedule_empty_state_update(self.inner.clone());
    }

    /// Append stream items to the grid without resetting the current filter.
    /// Used when a URL playlist is added while the library view is open.
    pub fn append_stream_items(&self, new_items: Vec<MediaItem>) {
        let playing = self.inner.now_playing_path.borrow().clone();
        let start_idx = self.inner.items.borrow().len();
        for (i, item) in new_items.iter().enumerate() {
            let idx = start_idx + i;
            let is_playing = playing.as_deref() == Some(item.path.as_path());
            let card = make_card(item, idx, is_playing);
            let card_path = item.path.clone();
            attach_context_menu(&card, card_path, self.inner.clone());
            self.inner.flow.insert(&card, -1);
        }
        self.inner.items.borrow_mut().extend(new_items);
        self.inner.flow.invalidate_filter();
        schedule_empty_state_update(self.inner.clone());
    }

    /// Update one card's thumbnail after async probe completes.
    pub fn update_item_thumbnail(&self, path: &std::path::Path, thumb_path: PathBuf) {
        let orig_idx = self.inner.items.borrow().iter().position(|i| i.path == path);
        let Some(orig_idx) = orig_idx else { return };

        self.inner.items.borrow_mut()[orig_idx].thumbnail_path = Some(thumb_path);

        // Find the child whose widget_name encodes orig_idx.
        let target_name_prefix = format!(":{orig_idx}");
        let mut child_opt = self.inner.flow.first_child();
        while let Some(child) = child_opt {
            let next = child.next_sibling();
            if let Some(fbc) = child.downcast_ref::<FlowBoxChild>() {
                if fbc.widget_name().ends_with(&target_name_prefix) {
                    let is_playing = {
                        let np = self.inner.now_playing_path.borrow();
                        np.as_deref().map(|p| p == path).unwrap_or(false)
                    };
                    let new_card = {
                        let items = self.inner.items.borrow();
                        make_card(&items[orig_idx], orig_idx, is_playing)
                    };
                    let card_path = path.to_path_buf();
                    self.inner.flow.remove(fbc);
                    // Re-insert at the same logical position by appending and
                    // letting the sort func place it correctly.
                    self.inner.flow.insert(&new_card, orig_idx as i32);
                    attach_context_menu(&new_card, card_path, self.inner.clone());
                    self.inner.flow.invalidate_filter();
                    break;
                }
            }
            child_opt = next;
        }
    }

    /// Switch the kind filter (all / video / audio).
    pub fn apply_filter(&self, filter: &str) {
        let kind = match filter {
            "video" => FILTER_VIDEO,
            "audio" => FILTER_AUDIO,
            _       => FILTER_ALL,
        };
        self.inner.filter_kind.set(kind);
        self.inner.filter_recent_only.set(false);
        self.inner.filter_include_streams.set(false);
        *self.inner.filter_playlist.borrow_mut() = None;
        *self.inner.filter_folder.borrow_mut() = None;
        self.inner.flow.invalidate_filter();
        schedule_empty_state_update(self.inner.clone());
    }

    /// Show only items under the given folder path.
    pub fn apply_folder_filter(&self, folder: PathBuf) {
        *self.inner.filter_folder.borrow_mut() = Some(folder);
        self.inner.filter_kind.set(FILTER_ALL);
        self.inner.filter_recent_only.set(false);
        self.inner.filter_include_streams.set(false);
        *self.inner.filter_playlist.borrow_mut() = None;
        self.inner.flow.invalidate_filter();
        schedule_empty_state_update(self.inner.clone());
    }

    /// Show only items in the given playlist (including stream items for URL playlists).
    pub fn apply_playlist_filter(&self, paths: Vec<PathBuf>) {
        let has_streams = paths.iter().any(|p| is_stream_url(p.as_path()));
        let set: HashSet<PathBuf> = paths.into_iter().collect();
        *self.inner.filter_playlist.borrow_mut() = Some(set);
        self.inner.filter_kind.set(FILTER_ALL);
        self.inner.filter_recent_only.set(false);
        self.inner.filter_include_streams.set(has_streams);
        *self.inner.filter_folder.borrow_mut() = None;
        self.inner.flow.invalidate_filter();
        schedule_empty_state_update(self.inner.clone());
    }

    /// Show only items that have been played, sorted by most-recently-played.
    pub fn apply_recent_filter(&self) {
        self.inner.filter_recent_only.set(true);
        self.inner.filter_kind.set(FILTER_ALL);
        self.inner.filter_include_streams.set(false);
        *self.inner.filter_playlist.borrow_mut() = None;
        *self.inner.filter_folder.borrow_mut() = None;
        self.inner.sort_order.set(SORT_RECENTLY_PLAYED);
        self.inner.flow.invalidate_filter();
        self.inner.flow.invalidate_sort();
        schedule_empty_state_update(self.inner.clone());
    }

    /// Count of items that have ever been played (for the sidebar badge).
    pub fn recent_count(&self) -> usize {
        self.inner.items.borrow().iter().filter(|i| i.last_played.is_some()).count()
    }

    /// Sync play metadata for one item so the "Recently Played" filter sees it.
    /// Called after `record_play` updates the store.
    pub fn sync_play_data(&self, path: &std::path::Path, last_played: Option<u64>, play_count: u32) {
        let mut items = self.inner.items.borrow_mut();
        if let Some(item) = items.iter_mut().find(|i| i.path == path) {
            item.last_played = last_played;
            item.play_count  = play_count;
        }
    }

    /// Highlight the card for the given path; clear all others.
    /// Pass `None` to remove the highlight.
    pub fn set_now_playing_path(&self, path: Option<PathBuf>) {
        *self.inner.now_playing_path.borrow_mut() = path.clone();
        let items = self.inner.items.borrow();
        let mut child_opt = self.inner.flow.first_child();
        while let Some(child) = child_opt {
            if let Some(fbc) = child.downcast_ref::<FlowBoxChild>() {
                let (_, idx) = decode_name(&fbc.widget_name());
                let is_playing = path.as_ref()
                    .and_then(|p| items.get(idx).map(|i| i.path.as_path() == p.as_path()))
                    .unwrap_or(false);
                if is_playing {
                    fbc.add_css_class("library-card-playing");
                } else {
                    fbc.remove_css_class("library-card-playing");
                }
            }
            child_opt = child.next_sibling();
        }
    }
}

// ── Empty-state management ────────────────────────────────────────────────────

/// Schedule an idle callback that picks the right view-stack page.
/// Using idle avoids checking before GTK has applied the filter pass.
fn schedule_empty_state_update(inner: Rc<GridInner>) {
    glib::idle_add_local_once(move || {
        let items = inner.items.borrow();
        let non_stream_count = items.iter().filter(|i| i.kind != MediaKind::Stream).count();
        let page = if non_stream_count == 0 && !inner.filter_include_streams.get() {
            PAGE_EMPTY_LIBRARY
        } else {
            // Count items that pass the current filters.
            let fk  = inner.filter_kind.get();
            let fr  = inner.filter_recent_only.get();
            let fis = inner.filter_include_streams.get();
            let fs  = inner.filter_search.borrow().to_lowercase();
            let fp  = inner.filter_playlist.borrow();
            let ff  = inner.filter_folder.borrow();
            let visible = items.iter().filter(|item| {
                let is_stream = item.kind == MediaKind::Stream;
                if is_stream && !fis { return false; }
                if fr && item.last_played.is_none() { return false; }
                if let Some(paths) = fp.as_ref() {
                    if !paths.contains(&item.path) { return false; }
                }
                if let Some(folder) = ff.as_ref() {
                    if !item.path.starts_with(folder.as_path()) { return false; }
                }
                match fk {
                    FILTER_VIDEO => { if item.kind != MediaKind::Video { return false; } }
                    FILTER_AUDIO => { if item.kind != MediaKind::Audio { return false; } }
                    _ => { if is_stream && !fis { return false; } }
                }
                if !fs.is_empty() {
                    let hit = item.title.to_lowercase().contains(fs.as_str())
                        || item.artist.as_deref().map(|a| a.to_lowercase().contains(fs.as_str())).unwrap_or(false)
                        || item.album.as_deref().map(|a| a.to_lowercase().contains(fs.as_str())).unwrap_or(false);
                    if !hit { return false; }
                }
                true
            }).count();
            if visible == 0 { PAGE_EMPTY_RESULTS } else { PAGE_ITEMS }
        };
        inner.view_stack.set_visible_child_name(page);
    });
}

// ── Sort helper ───────────────────────────────────────────────────────────────

fn std_to_gtk(o: std::cmp::Ordering) -> gtk4::Ordering {
    match o {
        std::cmp::Ordering::Less    => gtk4::Ordering::Smaller,
        std::cmp::Ordering::Equal   => gtk4::Ordering::Equal,
        std::cmp::Ordering::Greater => gtk4::Ordering::Larger,
    }
}

fn compare_items(a: &MediaItem, b: &MediaItem, order: u8) -> gtk4::Ordering {
    let ord = match order {
        SORT_TITLE           => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        SORT_DATE_ADDED      => b.date_added.unwrap_or(0).cmp(&a.date_added.unwrap_or(0)),
        SORT_RECENTLY_PLAYED => b.last_played.unwrap_or(0).cmp(&a.last_played.unwrap_or(0)),
        SORT_DURATION        => {
            let da = a.duration_secs.unwrap_or(0.0) as u64;
            let db = b.duration_secs.unwrap_or(0.0) as u64;
            db.cmp(&da)
        }
        _ => std::cmp::Ordering::Equal,
    };
    std_to_gtk(ord)
}

// ── Right-click context menu ──────────────────────────────────────────────────

fn attach_context_menu(child: &FlowBoxChild, path: PathBuf, inner: Rc<GridInner>) {
    let content_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .margin_top(4).margin_bottom(4)
        .margin_start(4).margin_end(4)
        .build();
    content_box.set_size_request(190, -1);

    let popover = gtk::Popover::builder()
        .child(&content_box)
        .has_arrow(false)
        .build();
    popover.set_parent(child);

    popover.connect_show({
        let inner_c     = inner.clone();
        let path_c      = path.clone();
        let content_ref = content_box.clone();
        let pw          = popover.downgrade();
        move |_| {
            while let Some(w) = content_ref.first_child() { content_ref.remove(&w); }
            let playlists = inner_c.playlists.borrow().clone();

            if !playlists.is_empty() {
                content_ref.append(
                    &Label::builder()
                        .label(t("Add to playlist"))
                        .halign(Align::Start)
                        .css_classes(vec!["caption", "dim-label"])
                        .margin_start(8).margin_bottom(2)
                        .build(),
                );
                for pl in &playlists {
                    let btn = gtk::Button::builder()
                        .child(&Label::builder()
                            .label(&pl.name)
                            .halign(Align::Start)
                            .hexpand(true)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .max_width_chars(24)
                            .build())
                        .css_classes(vec!["flat"])
                        .build();
                    let inner_b = inner_c.clone();
                    let path_b  = path_c.clone();
                    let pl_id   = pl.id.clone();
                    let pw2     = pw.clone();
                    btn.connect_clicked(move |_| {
                        if let Some(p) = pw2.upgrade() { p.popdown(); }
                        if let Some(cb) = &*inner_b.on_add_to_playlist.borrow() {
                            cb(path_b.clone(), pl_id.clone());
                        }
                    });
                    content_ref.append(&btn);
                }
                let sep = gtk::Separator::new(Orientation::Horizontal);
                sep.set_margin_top(4); sep.set_margin_bottom(4);
                content_ref.append(&sep);
            }

            let new_row = gtk::Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .build();
            new_row.append(&gtk::Image::from_icon_name("list-add-symbolic"));
            new_row.append(&Label::builder()
                .label(t("New playlist…"))
                .halign(Align::Start)
                .hexpand(true)
                .build());
            let new_btn = gtk::Button::builder()
                .child(&new_row)
                .css_classes(vec!["flat"])
                .build();
            let inner_n = inner_c.clone();
            let path_n  = path_c.clone();
            let pw3     = pw.clone();
            new_btn.connect_clicked(move |_| {
                if let Some(p) = pw3.upgrade() { p.popdown(); }
                if let Some(cb) = &*inner_n.on_new_playlist.borrow() {
                    cb(path_n.clone());
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

fn make_card(item: &MediaItem, orig_idx: usize, is_playing: bool) -> FlowBoxChild {
    let card_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .width_request(200)
        .height_request(120)
        .css_classes(vec!["library-card"])
        .build();

    // Thumbnail — fixed 200×120.
    let thumb_overlay = gtk::Overlay::new();
    thumb_overlay.set_size_request(200, 120);
    thumb_overlay.set_halign(Align::Fill);
    thumb_overlay.set_valign(Align::Start);
    // Clip content that tries to exceed the 200×120 bounding box.
    thumb_overlay.set_overflow(gtk::Overflow::Hidden);

    let thumb_widget: gtk::Widget = if let Some(thumb_path) = &item.thumbnail_path {
        let pic = gtk::Picture::for_filename(thumb_path);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_can_shrink(true);
        pic.set_halign(Align::Fill);
        pic.set_valign(Align::Fill);
        pic.set_size_request(200, 120);
        pic.add_css_class("library-card-picture");
        pic.upcast()
    } else {
        make_placeholder(item).upcast()
    };
    thumb_overlay.set_child(Some(&thumb_widget));

    // Bottom gradient: title + subtitle.
    let overlay_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .valign(Align::End)
        .halign(Align::Fill)
        .css_classes(vec!["library-card-overlay"])
        .build();

    overlay_box.append(&Label::builder()
        .label(&item.title)
        .halign(Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(22)
        .css_classes(vec!["library-card-title"])
        .build());

    let subtitle = match item.kind {
        MediaKind::Audio => item.artist.clone()
            .or_else(|| item.album.clone())
            .unwrap_or_default(),
        MediaKind::Video => item.path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        MediaKind::Stream => url_hostname(&item.path.to_string_lossy()),
    };
    if !subtitle.is_empty() {
        overlay_box.append(&Label::builder()
            .label(&subtitle)
            .halign(Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(22)
            .css_classes(vec!["library-card-subtitle"])
            .build());
    }
    thumb_overlay.add_overlay(&overlay_box);

    // Duration badge (top-right).
    if let Some(dur) = item.duration_secs {
        thumb_overlay.add_overlay(&Label::builder()
            .label(&fmt_duration(dur as u64))
            .css_classes(vec!["library-badge", "library-badge-duration"])
            .halign(Align::End)
            .valign(Align::Start)
            .margin_end(6)
            .margin_top(6)
            .build());
    }

    // Hover play overlay (hidden by default).
    let play_overlay = gtk::Box::builder()
        .halign(Align::Center)
        .valign(Align::Center)
        .css_classes(vec!["library-play-overlay"])
        .visible(false)
        .build();
    play_overlay.append(&gtk::Image::builder()
        .icon_name("media-playback-start-symbolic")
        .pixel_size(28)
        .build());
    thumb_overlay.add_overlay(&play_overlay);

    card_box.append(&thumb_overlay);

    let kind_name = match item.kind {
        MediaKind::Video  => "video",
        MediaKind::Audio  => "audio",
        MediaKind::Stream => "stream",
    };

    let mut classes = vec!["library-card-child"];
    if is_playing { classes.push("library-card-playing"); }

    let child = FlowBoxChild::builder()
        .child(&card_box)
        .css_classes(classes)
        .build();
    // Encode kind + original insertion index — both filter and activation use this.
    child.set_widget_name(&encode_name(kind_name, orig_idx));
    child.set_cursor_from_name(Some("pointer"));
    // Force a fixed natural size so FlowBox homogeneous mode uses the same
    // cell dimensions for all cards (audio, video, stream) in every filter view.
    child.set_size_request(200, 120);
    child.set_hexpand(false);

    // Hover controller.
    let motion = gtk::EventControllerMotion::new();
    {
        let ov = play_overlay.clone();
        motion.connect_enter(move |_, _, _| ov.set_visible(true));
    }
    {
        let ov = play_overlay.clone();
        motion.connect_leave(move |_| ov.set_visible(false));
    }
    child.add_controller(motion);

    child
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_placeholder(item: &MediaItem) -> gtk::Box {
    let icon = gtk::Image::from_icon_name(match item.kind {
        MediaKind::Video  => "video-x-generic-symbolic",
        MediaKind::Audio  => "audio-x-generic-symbolic",
        MediaKind::Stream => "applications-internet-symbolic",
    });
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
        .hexpand(false)
        .vexpand(false)
        .css_classes(vec!["library-card-placeholder"])
        .build();
    placeholder.set_size_request(200, 120);
    placeholder.set_overflow(gtk::Overflow::Hidden);
    placeholder.append(&icon);
    placeholder
}

fn fmt_duration(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}
