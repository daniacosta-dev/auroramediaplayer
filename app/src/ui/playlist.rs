use adw::{NavigationPage, ToolbarView, HeaderBar};
use adw::prelude::NavigationPageExt;
use gtk4::{self as gtk, ListBox, ListBoxRow, Label, ScrolledWindow, SelectionMode,
           Box, Orientation, Button, Image};
use gtk4::prelude::*;
use glib;
use gdk4;
use std::time::Duration;
use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;

use crate::i18n::t;
use crate::state::SharedState;
use crate::player::PlayerCommand;

// ── Types ────────────────────────────────────────────────────────────────────

type Items = Rc<RefCell<Vec<(String, PathBuf)>>>;

pub struct PlaylistPanel {
    page: NavigationPage,
    list: ListBox,
    items: Items,
    state: SharedState,
    header_title_label: gtk::Label,
    ph_label: Label,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fmt_duration(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Probe duration via ffprobe (local files) or yt-dlp (URLs) in a background
/// thread; update label when done.
fn probe_duration_async(path: PathBuf, label: Label) {
    let weak = label.downgrade();
    let (tx, rx) = std::sync::mpsc::channel::<Option<f64>>();

    std::thread::spawn(move || {
        let path_str = path.to_string_lossy();
        let is_url = path_str.starts_with("http://") || path_str.starts_with("https://");

        let duration = if is_url {
            // Resolve duration via yt-dlp without downloading the media.
            // Honour the snap bundle path the same way mpv.rs does.
            let ytdlp = std::env::var("SNAP")
                .map(|s| format!("{}/usr/bin/yt-dlp", s))
                .unwrap_or_else(|_| "yt-dlp".to_string());
            std::process::Command::new(&ytdlp)
                .args(["--print", "duration", "--no-warnings", "--quiet"])
                .arg(path_str.as_ref())
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse::<f64>().ok())
        } else {
            std::process::Command::new("ffprobe")
                .args([
                    "-v", "error",
                    "-show_entries", "format=duration",
                    "-of", "default=noprint_wrappers=1:nokey=1",
                ])
                .arg(&path)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse::<f64>().ok())
        };
        tx.send(duration).ok();
    });

    // Poll from the main thread every 100 ms — no Send requirement.
    glib::timeout_add_local(Duration::from_millis(100), move || {
        match rx.try_recv() {
            Ok(duration) => {
                if let Some(lbl) = weak.upgrade() {
                    if let Some(d) = duration {
                        lbl.set_label(&fmt_duration(d as u64));
                    }
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

// ── Row factory ──────────────────────────────────────────────────────────────

/// Build a single playlist row widget with track number, play-indicator,
/// title, duration, and a remove button.
fn make_row(
    idx: usize,
    title: &str,
    path: &PathBuf,
    items: &Items,
    state: &SharedState,
    list: &ListBox,
) -> ListBoxRow {
    let row = ListBoxRow::builder()
        .css_classes(vec!["playlist-row"])
        .build();

    // ── Inner layout ──────────────────────────────────────────────────────
    let content = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(10)
        .margin_start(10)
        .margin_end(6)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    // ── Track number ──────────────────────────────────────────────────────
    let track_num = Label::builder()
        .label(&format!("{}", idx + 1))
        .css_classes(vec!["track-number", "caption"])
        .width_chars(3)
        .xalign(1.0)
        .build();

    // ── Title ─────────────────────────────────────────────────────────────
    let title_lbl = Label::builder()
        .label(title)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["playlist-title"])
        .build();

    // ── Duration ──────────────────────────────────────────────────────────
    let dur_lbl = Label::builder()
        .label("--:--")
        .css_classes(vec!["caption", "dim-label", "playlist-duration"])
        .build();

    // ── Remove button — invisible by default (opacity 0 keeps layout stable)
    let remove_btn = Button::builder()
        .icon_name("list-remove-symbolic")
        .css_classes(vec!["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();
    remove_btn.set_opacity(0.0);
    remove_btn.set_cursor_from_name(Some("pointer"));

    content.append(&track_num);
    content.append(&title_lbl);
    content.append(&dur_lbl);
    content.append(&remove_btn);
    row.set_child(Some(&content));

    // Fade remove button in/out on row hover — opacity keeps the layout stable.
    {
        let btn_enter = remove_btn.downgrade();
        let btn_leave = remove_btn.downgrade();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_enter(move |_, _, _| {
            if let Some(b) = btn_enter.upgrade() { b.set_opacity(1.0); }
        });
        motion.connect_leave(move |_| {
            if let Some(b) = btn_leave.upgrade() { b.set_opacity(0.0); }
        });
        row.add_controller(motion);
    }

    probe_duration_async(path.clone(), dur_lbl);

    // ── Remove ────────────────────────────────────────────────────────────
    {
        let items_c = items.clone();
        let state_c = state.clone();
        let list_c = list.downgrade();
        let row_c = row.downgrade();
        remove_btn.connect_clicked(move |_| {
            let Some(list) = list_c.upgrade() else { return };
            let Some(row) = row_c.upgrade() else { return };
            let idx = row.index() as usize;

            items_c.borrow_mut().remove(idx);

            {
                let mut s = state_c.borrow_mut();
                if idx < s.playlist.len() {
                    s.playlist.remove(idx);
                }
                if let Some(cur) = s.current_idx {
                    s.current_idx = if cur == idx {
                        None
                    } else if cur > idx {
                        Some(cur - 1)
                    } else {
                        Some(cur)
                    };
                }
            }

            rebuild_list(&list, &items_c, &state_c);
        });
    }

    // ── Drag source ───────────────────────────────────────────────────────
    let drag_src = gtk::DragSource::builder()
        .actions(gdk4::DragAction::MOVE)
        .build();
    {
        let row_c = row.downgrade();
        drag_src.connect_prepare(move |src, _, _| {
            let row = row_c.upgrade()?;
            let idx = row.index() as u64;
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            src.set_icon(Some(&paintable), 0, 0);
            Some(gdk4::ContentProvider::for_value(&idx.to_value()))
        });
    }
    row.add_controller(drag_src);

    // ── Drop target ───────────────────────────────────────────────────────
    let drop_tgt = gtk::DropTarget::new(u64::static_type(), gdk4::DragAction::MOVE);
    {
        let items_c = items.clone();
        let state_c = state.clone();
        let list_c = list.downgrade();
        let row_c = row.downgrade();
        drop_tgt.connect_drop(move |_, value, _, _| {
            let Ok(src_idx) = value.get::<u64>() else { return false };
            let src_idx = src_idx as usize;
            let Some(row) = row_c.upgrade() else { return false };
            let dst_idx = row.index() as usize;
            if src_idx == dst_idx { return false; }

            {
                let mut its = items_c.borrow_mut();
                if src_idx >= its.len() || dst_idx >= its.len() { return false; }
                let item = its.remove(src_idx);
                its.insert(dst_idx, item);
            }

            {
                let mut s = state_c.borrow_mut();
                let len = s.playlist.len();
                if src_idx < len && dst_idx < len {
                    let p = s.playlist.remove(src_idx);
                    s.playlist.insert(dst_idx, p);
                    if let Some(cur) = s.current_idx {
                        s.current_idx = Some(if cur == src_idx {
                            dst_idx
                        } else if src_idx < cur && cur <= dst_idx {
                            cur - 1
                        } else if dst_idx <= cur && cur < src_idx {
                            cur + 1
                        } else {
                            cur
                        });
                    }
                }
            }

            if let Some(list) = list_c.upgrade() {
                rebuild_list(&list, &items_c, &state_c);
                // Re-select the moved item
                if let Some(row) = list.row_at_index(dst_idx as i32) {
                    list.select_row(Some(&row));
                }
            }
            true
        });
    }
    row.add_controller(drop_tgt);

    row
}

/// Remove all list rows and re-create them from `items`.
fn rebuild_list(list: &ListBox, items: &Items, state: &SharedState) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let its = items.borrow();
    for (idx, (title, path)) in its.iter().enumerate() {
        let row = make_row(idx, title, path, items, state, list);
        list.append(&row);
    }
}

// ── PlaylistPanel ─────────────────────────────────────────────────────────────

impl PlaylistPanel {
    pub fn new(state: SharedState) -> Self {
        let items: Items = Rc::new(RefCell::new(Vec::new()));

        let list = ListBox::builder()
            .selection_mode(SelectionMode::Single)
            .css_classes(vec!["playlist-list"])
            .build();

        // ── Empty state placeholder ────────────────────────────────────────
        let placeholder = Box::builder()
            .orientation(Orientation::Vertical)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .spacing(10)
            .margin_top(48)
            .margin_bottom(48)
            .build();
        let ph_icon = Image::builder()
            .icon_name("folder-videos-symbolic")
            .pixel_size(48)
            .css_classes(vec!["dim-label"])
            .build();
        let ph_label = Label::builder()
            .label(t("Drop files or folders here"))
            .css_classes(vec!["dim-label"])
            .build();
        placeholder.append(&ph_icon);
        placeholder.append(&ph_label);
        list.set_placeholder(Some(&placeholder));

        let scroll = ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .build();

        let toolbar = ToolbarView::builder().content(&scroll).build();

        let header_title_label = gtk::Label::new(Some(t("Playlist")));
        let header = HeaderBar::builder()
            .title_widget(&header_title_label)
            .show_back_button(false)
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();
        toolbar.add_top_bar(&header);

        let page = NavigationPage::builder()
            .child(&toolbar)
            .title(t("Playlist"))
            .build();

        // Row click → play from beginning (cancel any session-restore seek)
        {
            let state_c = state.clone();
            list.connect_row_activated(move |_, row| {
                let idx = row.index() as usize;
                let path = {
                    let mut s = state_c.borrow_mut();
                    s.current_idx = Some(idx);
                    s.pending_seek = None;
                    s.playlist.get(idx).cloned()
                };
                if let Some(path) = path {
                    if let Some(p) = state_c.borrow().player.as_ref() {
                        p.execute(PlayerCommand::Open(path)).ok();
                    }
                }
            });
        }

        Self { page, list, items, state, header_title_label, ph_label }
    }

    pub fn widget(&self) -> &NavigationPage {
        &self.page
    }

    pub fn set_mode(&self, mode: &str) {
        if mode == "modern" {
            self.page.add_css_class("playlist-panel-modern");
        } else {
            self.page.remove_css_class("playlist-panel-modern");
        }
    }

    pub fn relabel(&self) {
        self.page.set_title(t("Playlist"));
        self.header_title_label.set_label(t("Playlist"));
        self.ph_label.set_label(t("Drop files or folders here"));
    }

    pub fn add_item(&self, title: &str, path: &PathBuf) {
        let idx = {
            let mut items = self.items.borrow_mut();
            let idx = items.len();
            items.push((title.to_string(), path.clone()));
            idx
        };
        let row = make_row(idx, title, path, &self.items, &self.state, &self.list);
        self.list.append(&row);
    }

    pub fn clear(&self) {
        self.items.borrow_mut().clear();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
    }

    pub fn select_row(&self, idx: usize) {
        if let Some(row) = self.list.row_at_index(idx as i32) {
            self.list.select_row(Some(&row));
            row.grab_focus();
        }
    }

    /// Return the current display title stored for `idx`.
    pub fn item_title(&self, idx: usize) -> Option<String> {
        self.items.borrow().get(idx).map(|(t, _)| t.clone())
    }

    /// Update the display title of a row (used to replace a URL placeholder
    /// with the real title once mpv/yt-dlp resolves it).
    pub fn update_row_title(&self, idx: usize, title: &str) {
        if let Some(row) = self.list.row_at_index(idx as i32) {
            if let Some(content) = row.child().and_downcast::<gtk::Box>() {
                let mut child = content.first_child();
                while let Some(widget) = child {
                    if let Ok(label) = widget.clone().downcast::<Label>() {
                        if label.has_css_class("playlist-title") {
                            label.set_label(title);
                            break;
                        }
                    }
                    child = widget.next_sibling();
                }
            }
            if let Some(item) = self.items.borrow_mut().get_mut(idx) {
                item.0 = title.to_string();
            }
        }
    }
}
