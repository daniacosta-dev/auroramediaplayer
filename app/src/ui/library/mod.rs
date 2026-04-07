mod sidebar;
mod grid;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{NavigationPage, ToolbarView, HeaderBar};
use gtk4::{self as gtk, Button};
use gtk4::prelude::*;
use gtk4::glib;

use crate::i18n::t;
use crate::library::{LibraryStore, MediaKind, metadata::probe_batch_async};
use crate::player::PlayerCommand;
use crate::state::SharedState;

pub use sidebar::LibrarySidebar;
pub use grid::MediaGrid;

// ── LibraryView ───────────────────────────────────────────────────────────────

pub struct LibraryView {
    page:              NavigationPage,
    sidebar:           LibrarySidebar,
    grid:              MediaGrid,
    store:             Rc<RefCell<LibraryStore>>,
    state:             SharedState,
    now_playing_btn:   Button,
    pause_btn:         Button,
    now_playing_group: gtk::Box,
    on_play:           Rc<RefCell<Option<std::boxed::Box<dyn Fn(PathBuf)>>>>,
    on_add_folder:     Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>>,
    on_now_playing:    Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>>,
}

impl LibraryView {
    pub fn new(state: SharedState) -> Self {
        let store = Rc::new(RefCell::new(LibraryStore::load()));
        let on_play:       Rc<RefCell<Option<std::boxed::Box<dyn Fn(PathBuf)>>>> = Rc::new(RefCell::new(None));
        let on_add_folder: Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>>        = Rc::new(RefCell::new(None));
        let on_now_playing: Rc<RefCell<Option<std::boxed::Box<dyn Fn()>>>>       = Rc::new(RefCell::new(None));

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

        // Pause / resume button — left side of the linked group.
        let pause_btn = Button::builder()
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text(t("Pause"))
            .build();
        pause_btn.set_cursor_from_name(Some("pointer"));
        {
            let state_c = state.clone();
            pause_btn.connect_clicked(move |_| {
                if let Some(player) = state_c.borrow().player.as_ref() {
                    player.execute(PlayerCommand::TogglePause).ok();
                }
            });
        }

        // "Now Playing" button — right side, navigates back to player.
        let now_playing_btn = Button::builder()
            .label(t("Now Playing"))
            .tooltip_text(t("Back to player"))
            .css_classes(vec!["suggested-action"])
            .build();
        now_playing_btn.set_cursor_from_name(Some("pointer"));

        // Linked pill — hidden until media is playing.
        let now_playing_group = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(vec!["linked"])
            .visible(false)
            .build();
        now_playing_group.append(&pause_btn);
        now_playing_group.append(&now_playing_btn);

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

        headerbar.pack_start(&now_playing_group);
        headerbar.pack_end(&scan_btn);
        headerbar.pack_end(&add_btn);

        // ── ToolbarView ───────────────────────────────────────────────────
        let toolbar = ToolbarView::new();
        toolbar.add_top_bar(&headerbar);
        toolbar.set_content(Some(&split));

        // ── Space shortcut: pause/resume from library ─────────────────────
        {
            let state_c = state.clone();
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_ctrl.connect_key_pressed(move |_, key, _, _| {
                if key == gdk4::Key::space {
                    if let Some(player) = state_c.borrow().player.as_ref() {
                        player.execute(PlayerCommand::TogglePause).ok();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            toolbar.add_controller(key_ctrl);
        }

        // ── NavigationPage ────────────────────────────────────────────────
        let page = NavigationPage::builder()
            .title("Aurora")
            .tag("library")
            .child(&toolbar)
            .build();

        // ── Sidebar → grid filter ─────────────────────────────────────────
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
                } else if filter == "recent" {
                    grid_c.apply_recent_filter();
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
                let folders = folder_counts(&s);
                let counts  = category_counts(&s, &grid_c);
                let items   = s.items.clone();
                drop(s);
                sidebar_c.update_folders(folders);
                sidebar_c.update_category_counts(counts.0, counts.1, counts.2, counts.3);
                grid_c.show_items(items);
            });
        }

        // ── Grid → add to existing playlist ──────────────────────────────
        {
            let store_c = store.clone();
            let sidebar_c = sidebar.clone_ref();
            let grid_c = grid.clone_ref();
            grid.connect_add_to_playlist(move |path, playlist_id| {
                let mut s = store_c.borrow_mut();
                s.add_to_playlist(&playlist_id, path);
                s.save();
                let playlists = s.playlists.clone();
                drop(s);
                sidebar_c.update_playlists(playlists.clone());
                grid_c.set_playlists(playlists);
            });
        }

        // ── Grid → new playlist dialog ────────────────────────────────────
        {
            let store_c   = store.clone();
            let grid_c    = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            let page_c    = page.clone();
            grid.connect_new_playlist(move |path| {
                show_new_playlist_dialog(
                    &page_c, Some(path),
                    store_c.clone(), grid_c.clone_ref(), sidebar_c.clone_ref(),
                );
            });
        }

        // ── Sidebar "+" → new empty playlist ─────────────────────────────
        {
            let store_c   = store.clone();
            let grid_c    = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            let page_c    = page.clone();
            sidebar.connect_new_playlist(move || {
                show_new_playlist_dialog(
                    &page_c, None,
                    store_c.clone(), grid_c.clone_ref(), sidebar_c.clone_ref(),
                );
            });
        }

        // ── Sidebar → delete playlist ─────────────────────────────────────
        {
            let store_c   = store.clone();
            let grid_c    = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            sidebar.connect_delete_playlist(move |playlist_id| {
                let mut s = store_c.borrow_mut();
                s.delete_playlist(&playlist_id);
                s.save();
                let playlists = s.playlists.clone();
                drop(s);
                sidebar_c.update_playlists(playlists.clone());
                grid_c.set_playlists(playlists);
                grid_c.apply_filter("all");
            });
        }

        // ── Sidebar → rename playlist ─────────────────────────────────────
        {
            let store_c   = store.clone();
            let grid_c    = grid.clone_ref();
            let sidebar_c = sidebar.clone_ref();
            sidebar.connect_rename_playlist(move |playlist_id, new_name| {
                let mut s = store_c.borrow_mut();
                if let Some(pl) = s.playlists.iter_mut().find(|p| p.id == playlist_id) {
                    pl.name = new_name;
                }
                s.save();
                let playlists = s.playlists.clone();
                drop(s);
                sidebar_c.update_playlists(playlists.clone());
                grid_c.set_playlists(playlists);
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
                let (folders, counts, items) = {
                    let mut s = store_c.borrow_mut();
                    s.rescan();
                    s.save();
                    let folders = folder_counts(&s);
                    let counts  = category_counts(&s, &grid_c);
                    let items   = s.items.clone();
                    (folders, counts, items)
                };
                sidebar_c.update_folders(folders);
                sidebar_c.update_category_counts(counts.0, counts.1, counts.2, counts.3);
                grid_c.show_items(items);
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

        // ── Now Playing button ────────────────────────────────────────────
        {
            let on_np_c = on_now_playing.clone();
            now_playing_btn.connect_clicked(move |_| {
                if let Some(cb) = &*on_np_c.borrow() {
                    cb();
                }
            });
        }

        let view = Self {
            page, sidebar, grid, store, state,
            now_playing_btn, pause_btn, now_playing_group,
            on_play, on_add_folder, on_now_playing,
        };
        view.reload_from_store();
        view
    }

    pub fn page(&self) -> &NavigationPage { &self.page }

    pub fn connect_play<F: Fn(PathBuf) + 'static>(&self, f: F) {
        *self.on_play.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    pub fn connect_add_folder<F: Fn() + 'static>(&self, f: F) {
        *self.on_add_folder.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    pub fn connect_now_playing<F: Fn() + 'static>(&self, f: F) {
        *self.on_now_playing.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Show or hide the "Now Playing" / pause group.
    pub fn set_now_playing(&self, active: bool, title: &str) {
        self.now_playing_group.set_visible(active);
        if active {
            let display = {
                let chars: Vec<char> = title.chars().collect();
                if title.is_empty() {
                    t("Now Playing").to_string()
                } else if chars.len() > 28 {
                    format!("{}…", chars[..27].iter().collect::<String>())
                } else {
                    title.to_string()
                }
            };
            self.now_playing_btn.set_label(&display);
            let tip = if title.is_empty() {
                t("Back to player").to_string()
            } else {
                format!("{} ↩", title)
            };
            self.now_playing_btn.set_tooltip_text(Some(&tip));
        }
    }

    /// Update the pause/resume button icon and group style.
    pub fn set_play_pause_state(&self, paused: bool) {
        if paused {
            self.pause_btn.set_icon_name("media-playback-start-symbolic");
            self.pause_btn.set_tooltip_text(Some(t("Resume")));
            self.now_playing_btn.remove_css_class("suggested-action");
        } else {
            self.pause_btn.set_icon_name("media-playback-pause-symbolic");
            self.pause_btn.set_tooltip_text(Some(t("Pause")));
            self.now_playing_btn.add_css_class("suggested-action");
        }
    }

    /// Forward the now-playing path to the grid and refresh sidebar counts.
    pub fn set_now_playing_path(&self, path: Option<PathBuf>) {
        self.grid.set_now_playing_path(path);
        self.refresh_sidebar_counts();
    }

    /// Sync play metadata for one item from the store into the grid's item cache.
    /// Must be called after `record_play` so the "Recently Played" filter works.
    pub fn sync_play_data(&self, path: &std::path::Path) {
        let s = self.store.borrow();
        if let Some(item) = s.items.iter().find(|i| i.path == path) {
            self.grid.sync_play_data(path, item.last_played, item.play_count);
        }
    }

    pub fn reload_from_store(&self) {
        let s = self.store.borrow();
        let folders  = folder_counts(&s);
        let playlists = s.playlists.clone();
        let items    = s.items.clone();
        drop(s);

        self.sidebar.update_folders(folders);
        self.sidebar.update_playlists(playlists.clone());
        self.grid.set_playlists(playlists);
        self.grid.show_items(items);
        self.refresh_sidebar_counts();
        probe_unprobed(&self.store, &self.grid);
    }

    pub fn add_folder(&self, folder: PathBuf) {
        let added = {
            let mut s = self.store.borrow_mut();
            if s.add_folder(folder) {
                s.rescan();
                s.save();
                let folders = folder_counts(&s);
                let items   = s.items.clone();
                drop(s);
                self.sidebar.update_folders(folders);
                self.grid.show_items(items);
                true
            } else {
                false
            }
        };
        if added {
            self.refresh_sidebar_counts();
            probe_unprobed(&self.store, &self.grid);
        }
    }

    /// Recompute and push category counts to the sidebar.
    pub fn refresh_sidebar_counts(&self) {
        let s = self.store.borrow();
        let counts = category_counts(&s, &self.grid);
        drop(s);
        self.sidebar.update_category_counts(counts.0, counts.1, counts.2, counts.3);
    }

    pub fn store(&self) -> Rc<RefCell<LibraryStore>> {
        self.store.clone()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// (all, video, audio, recently_played) — computed entirely from the store.
fn category_counts(store: &LibraryStore, _grid: &MediaGrid) -> (usize, usize, usize, usize) {
    let total  = store.items.len();
    let video  = store.items.iter().filter(|i| i.kind == MediaKind::Video).count();
    let audio  = store.items.iter().filter(|i| i.kind == MediaKind::Audio).count();
    let recent = store.items.iter().filter(|i| i.last_played.is_some()).count();
    (total, video, audio, recent)
}

fn folder_counts(store: &LibraryStore) -> Vec<(PathBuf, usize)> {
    store.watched_folders.iter().map(|folder| {
        let count = store.items.iter().filter(|i| i.path.starts_with(folder)).count();
        (folder.clone(), count)
    }).collect()
}

// ── New playlist dialog ───────────────────────────────────────────────────────

fn show_new_playlist_dialog(
    parent: &impl gtk4::prelude::IsA<gtk4::Widget>,
    initial_path: Option<PathBuf>,
    store: Rc<RefCell<LibraryStore>>,
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

fn probe_unprobed(store: &Rc<RefCell<LibraryStore>>, grid: &MediaGrid) {
    let paths: Vec<PathBuf> = {
        let s = store.borrow();
        s.items.iter()
            .filter(|i| match &i.thumbnail_path {
                None    => true,
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
        {
            let mut s = store_c.borrow_mut();
            if let Some(item) = s.items.iter_mut().find(|i| i.path == result.path) {
                result.apply_to(item);
            }
            s.save();
        }
        if let Some(thumb) = thumb_path {
            grid_c.update_item_thumbnail(&result.path, thumb);
        }
    });
}
