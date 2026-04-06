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
use crate::library::{LibraryStore, metadata::probe_batch_async};
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

        // "Now Playing" pill button — start/end with icon + label
        let now_playing_btn = Button::builder()
            .tooltip_text(t("Back to player"))
            .visible(false)
            .css_classes(vec!["suggested-action", "pill"])
            .build();

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

        let add_btn = Button::builder()
            .icon_name("folder-new-symbolic")
            .tooltip_text(t("Add folder"))
            .build();

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

        // ── Sidebar → grid filter ─────────────────────────────────────────
        {
            let grid_c = grid.clone_ref();
            sidebar.connect_filter_changed(move |filter| {
                grid_c.apply_filter(&filter);
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

    /// Show or hide the "Now Playing" button and update the track title.
    pub fn set_now_playing(&self, active: bool, title: &str) {
        self.now_playing_btn.set_visible(active);
        if active && !title.is_empty() {
            if let Some(btn_box) = self.now_playing_btn.child()
                .and_then(|w| w.downcast::<gtk::Box>().ok())
            {
                // Second child of btn_box is the Label
                let mut child = btn_box.first_child();
                while let Some(c) = child {
                    if let Ok(lbl) = c.clone().downcast::<Label>() {
                        // Truncate long titles to keep the button compact
                        let display = if title.chars().count() > 30 {
                            format!("{}…", title.chars().take(30).collect::<String>())
                        } else {
                            title.to_string()
                        };
                        lbl.set_label(&display);
                        break;
                    }
                    child = c.next_sibling();
                }
            }
        }
    }

    pub fn reload_from_store(&self) {
        let s = self.store.borrow();
        self.sidebar.update_folders(s.watched_folders.clone());
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
