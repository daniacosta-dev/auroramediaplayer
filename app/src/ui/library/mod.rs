mod sidebar;
mod grid;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{NavigationPage, ToolbarView, HeaderBar};
use gtk4::{self as gtk, Button, Label};
use gtk4::prelude::*;

use crate::i18n::t;
use crate::library::{LibraryStore, Playlist, metadata::probe_batch_async};
use crate::state::SharedState;

pub use sidebar::LibrarySidebar;
pub use grid::MediaGrid;

// ── LibraryView ───────────────────────────────────────────────────────────────

pub struct LibraryView {
    page: NavigationPage,
    sidebar: LibrarySidebar,
    grid: MediaGrid,
    store: Rc<RefCell<LibraryStore>>,
    now_playing_btn: Button,
    on_play: Rc<RefCell<Option<std::boxed::Box<dyn Fn(PathBuf)>>>>,
    on_add_folder: Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>>,
    on_now_playing: Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>>,
}

impl LibraryView {
    pub fn new(_state: SharedState) -> Self {
        let store = Rc::new(RefCell::new(LibraryStore::load()));
        let on_play: Rc<RefCell<Option<std::boxed::Box<dyn Fn(PathBuf)>>>> =
            Rc::new(RefCell::new(None));
        let on_add_folder: Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>> =
            Rc::new(RefCell::new(None));
        let on_now_playing: Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>> =
            Rc::new(RefCell::new(None));

        // ── Sidebar ───────────────────────────────────────────────────────
        let sidebar = LibrarySidebar::new();

        // ── Grid ─────────────────────────────────────────────────────────
        let grid = MediaGrid::new();

        // ── Split layout ──────────────────────────────────────────────────
        let split = adw::NavigationSplitView::builder()
            .sidebar_width_fraction(0.25)
            .min_sidebar_width(180.0)
            .max_sidebar_width(280.0)
            .build();

        split.set_sidebar(Some(sidebar.page()));
        split.set_content(Some(grid.page()));

        // ── HeaderBar ─────────────────────────────────────────────────────
        let headerbar = HeaderBar::new();

        // "Now Playing" compact button — hidden until media is playing.
        let now_playing_btn = Button::builder()
            .tooltip_text(t("Back to player"))
            .visible(false)
            .css_classes(vec!["suggested-action"])
            .build();
        now_playing_btn.set_cursor_from_name(Some("pointer"));

        let btn_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let btn_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
        let btn_lbl = Label::new(Some(t("Now Playing")));
        btn_box.append(&btn_icon);
        btn_box.append(&btn_lbl);
        now_playing_btn.set_child(Some(&btn_box));

        let scan_btn = Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(t("Scan folders"))
            .build();
        scan_btn.set_cursor_from_name(Some("pointer"));

        let add_btn = Button::builder()
            .icon_name("folder-new-symbolic")
            .tooltip_text(t("Add folder"))
            .build();
        add_btn.set_cursor_from_name(Some("pointer"));

        headerbar.pack_start(&now_playing_btn);
        headerbar.pack_end(&scan_btn);
        headerbar.pack_end(&add_btn);

        // ── ToolbarView ───────────────────────────────────────────────────
        let toolbar = ToolbarView::new();
        toolbar.add_top_bar(&headerbar);
        toolbar.set_content(Some(&split));

        // ── NavigationPage ────────────────────────────────────────────────
        let page = NavigationPage::builder()
            .title("Aurora")
            .tag("library")
            .child(&toolbar)
            .build();

        // ── Sidebar → grid filter / playlist ─────────────────────────────────
        {
            let grid_c = grid.clone_ref();
            let store_c = store.clone();
            sidebar.connect_filter_changed(move |filter| {
                if let Some(id) = filter.strip_prefix("playlist:") {
                    let store = store_c.borrow();
                    if let Some(pl) = store.playlists.iter().find(|p| p.id == id) {
                        let paths = pl.paths.clone();
                        drop(store);
                        grid_c.apply_playlist_filter(paths);
                    }
                } else if matches!(filter.as_str(), "all" | "video" | "audio") {
                    grid_c.apply_filter(&filter);
                }
            });
        }

        // ── Sidebar remove folder ─────────────────────────────────────────
        {
            let store_c   = store.clone();
            let grid_c    = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            sidebar.connect_remove_folder(move |folder| {
                let mut s = store_c.borrow_mut();
                s.remove_folder(&folder);
                s.save();
                sidebar_c.update_folders(s.watched_folders.clone());
                grid_c.show_items(s.items.clone());
            });
        }

        // ── Grid → add to existing playlist ──────────────────────────────────
        {
            let store_c = store.clone();
            grid.connect_add_to_playlist(move |path, playlist_id| {
                let mut s = store_c.borrow_mut();
                s.add_to_playlist(&playlist_id, path);
                s.save();
            });
        }

        // ── Grid → new playlist dialog (via right-click on card) ─────────────
        {
            let store_c = store.clone();
            let grid_c = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            let page_c = page.clone();
            grid.connect_new_playlist(move |path| {
                show_new_playlist_dialog(
                    &page_c,
                    Some(path),
                    store_c.clone(),
                    grid_c.clone_ref(),
                    sidebar_c.clone_ref(),
                );
            });
        }

        // ── Sidebar "+" → new empty playlist ─────────────────────────────────
        {
            let store_c = store.clone();
            let grid_c = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            let page_c = page.clone();
            sidebar.connect_new_playlist(move || {
                show_new_playlist_dialog(
                    &page_c,
                    None,
                    store_c.clone(),
                    grid_c.clone_ref(),
                    sidebar_c.clone_ref(),
                );
            });
        }

        // ── Sidebar → delete playlist ─────────────────────────────────────────
        {
            let store_c = store.clone();
            let grid_c = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            sidebar.connect_delete_playlist(move |playlist_id| {
                let mut s = store_c.borrow_mut();
                s.delete_playlist(&playlist_id);
                s.save();
                let playlists = s.playlists.clone();
                drop(s);
                sidebar_c.update_playlists(playlists.clone());
                grid_c.set_playlists(playlists);
                // Reset view in case the deleted playlist was active.
                grid_c.apply_filter("all");
            });
        }

        // ── Grid activation → on_play ─────────────────────────────────────
        {
            let on_play_c = on_play.clone();
            grid.connect_item_activated(move |path| {
                if let Some(cb) = &*on_play_c.borrow() {
                    cb(path);
                }
            });
        }

        // ── Scan button ───────────────────────────────────────────────────
        {
            let store_c   = store.clone();
            let grid_c    = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            scan_btn.connect_clicked(move |_| {
                {
                    let mut s = store_c.borrow_mut();
                    s.rescan();
                    s.save();
                    sidebar_c.update_folders(s.watched_folders.clone());
                    grid_c.show_items(s.items.clone());
                }
                probe_unprobed(&store_c, &grid_c);
            });
        }

        // ── Add folder button ─────────────────────────────────────────────
        {
            let on_add_c = on_add_folder.clone();
            add_btn.connect_clicked(move |_| {
                if let Some(cb) = &*on_add_c.borrow() {
                    cb();
                }
            });
        }

        // ── Now Playing button ─────────────────────────────────────────────
        {
            let on_np_c = on_now_playing.clone();
            now_playing_btn.connect_clicked(move |_| {
                if let Some(cb) = &*on_np_c.borrow() {
                    cb();
                }
            });
        }

        let view = Self {
            page, sidebar, grid, store,
            now_playing_btn,
            on_play, on_add_folder, on_now_playing,
        };
        view.reload_from_store();
        view
    }

    pub fn page(&self) -> &NavigationPage {
        &self.page
    }

    pub fn connect_play<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.on_play.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    pub fn connect_add_folder<F: Fn() + 'static>(&self, f: F) {
        *self.on_add_folder.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Register callback for the "Now Playing" button.
    pub fn connect_now_playing<F: Fn() + 'static>(&self, f: F) {
        *self.on_now_playing.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Show or hide the "Now Playing" button.  The label stays fixed as
    /// "Now Playing"; the track title is shown in the tooltip only.
    pub fn set_now_playing(&self, active: bool, title: &str) {
        self.now_playing_btn.set_visible(active);
        if active {
            let tip = if title.is_empty() {
                t("Back to player").to_string()
            } else {
                format!("{} — {}", t("Now Playing"), title)
            };
            self.now_playing_btn.set_tooltip_text(Some(&tip));
        }
    }

    pub fn reload_from_store(&self) {
        let s = self.store.borrow();
        self.sidebar.update_folders(s.watched_folders.clone());
        self.sidebar.update_playlists(s.playlists.clone());
        self.grid.set_playlists(s.playlists.clone());
        self.grid.show_items(s.items.clone());
        drop(s);
        probe_unprobed(&self.store, &self.grid);
    }

    pub fn add_folder(&self, folder: PathBuf) {
        let added = {
            let mut s = self.store.borrow_mut();
            if s.add_folder(folder) {
                s.rescan();
                s.save();
                self.sidebar.update_folders(s.watched_folders.clone());
                self.grid.show_items(s.items.clone());
                true
            } else {
                false
            }
        };
        if added {
            probe_unprobed(&self.store, &self.grid);
        }
    }

    pub fn store(&self) -> Rc<RefCell<LibraryStore>> {
        self.store.clone()
    }
}

// ── New playlist dialog ───────────────────────────────────────────────────────

/// Show the "New Playlist" dialog.
/// If `initial_path` is Some, the item is added to the new playlist on confirm.
/// If None, an empty playlist is created (entry point from the sidebar "+" button).
fn show_new_playlist_dialog(
    parent: &impl gtk4::prelude::IsA<gtk4::Widget>,
    initial_path: Option<PathBuf>,
    store: std::rc::Rc<std::cell::RefCell<LibraryStore>>,
    grid: MediaGrid,
    sidebar: LibrarySidebar,
) {
    let dialog = adw::AlertDialog::new(Some(&t("New Playlist")), None::<&str>);
    dialog.add_response("cancel", &t("Cancel"));
    dialog.add_response("create", &t("Create"));
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");

    let entry = gtk::Entry::builder()
        .placeholder_text(t("Playlist name"))
        .activates_default(true)
        .margin_top(12)
        .build();
    dialog.set_extra_child(Some(&entry));

    dialog.connect_response(None, move |_, response| {
        if response != "create" { return; }
        let name = entry.text().trim().to_string();
        if name.is_empty() { return; }
        let mut s = store.borrow_mut();
        let id = s.create_playlist(&name);
        if let Some(ref path) = initial_path {
            s.add_to_playlist(&id, path.clone());
        }
        s.save();
        let playlists = s.playlists.clone();
        drop(s);
        grid.set_playlists(playlists.clone());
        sidebar.update_playlists(playlists);
    });

    dialog.present(parent);
}

// ── Background metadata probe ─────────────────────────────────────────────────

/// Collect items that still lack a thumbnail and probe them in the background.
/// On each result, update the store item and swap just that card in the grid.
fn probe_unprobed(store: &Rc<RefCell<LibraryStore>>, grid: &MediaGrid) {
    let paths: Vec<PathBuf> = {
        let s = store.borrow();
        s.items.iter()
            .filter(|i| match &i.thumbnail_path {
                None => true,
                Some(p) => !p.exists(),
            })
            .map(|i| i.path.clone())
            .collect()
    };
    if paths.is_empty() { return; }

    let store_c = store.clone();
    let grid_c  = grid.clone_ref();

    probe_batch_async(paths, move |result| {
        let thumb_path = result.thumbnail_path.clone();

        // Merge all probed metadata into the store item.
        {
            let mut s = store_c.borrow_mut();
            if let Some(item) = s.items.iter_mut().find(|i| i.path == result.path) {
                result.apply_to(item);
            }
            s.save();
        }

        // If we got a thumbnail, update just that card in the grid.
        if let Some(thumb) = thumb_path {
            grid_c.update_item_thumbnail(&result.path, thumb);
        }
    });
}
