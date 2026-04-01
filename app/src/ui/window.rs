use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::{ApplicationWindow, Toast, ToastOverlay, ToolbarView, Breakpoint, BreakpointCondition};
use gtk4::{self as gtk};
use glib;
use gio;
use gdk4;

use crate::library::scan_directory;
use crate::mpris::{MprisCommand, MprisState};
use crate::player::{PlayerCommand, MpvSnapshot, RepeatMode};
use crate::state::{PlayerState, SharedState};

use super::{
    headerbar::MediaHeaderBar,
    video_area::VideoArea,
    controls::PlayerControls,
    playlist::PlaylistPanel,
};

pub struct MediaWindow {
    window: ApplicationWindow,
    state: SharedState,
}

// ── Session persistence ───────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Session {
    playlist: Vec<PathBuf>,
    current_idx: Option<usize>,
    position: f64,
}

fn load_session() -> Option<Session> {
    let path = PlayerState::session_path()?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_session(state: &SharedState, pos: f64) {
    let Some(path) = PlayerState::session_path() else { return };
    let s = state.borrow();
    // Don't restore to EOF — next open would show last frame frozen.
    let position = if s.player.as_ref().map(|p| p.eof_reached()).unwrap_or(false) {
        0.0
    } else {
        pos
    };
    let session = Session {
        playlist: s.playlist.clone(),
        current_idx: s.current_idx,
        position,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(&session) {
        std::fs::write(path, json).ok();
    }
}

// ── Playlist helpers ──────────────────────────────────────────────────────────

/// Extract a human-readable display title from a path or URL.
/// For URLs: returns the hostname (e.g. "youtube.com") as a loading placeholder.
/// For file paths: returns the file stem (e.g. "My Video").
fn title_for_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
        rest.split('/').next()
            .and_then(|host| host.split('?').next())
            .unwrap_or("URL")
            .to_string()
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }
}

/// Parse an M3U or M3U8 playlist file and return a list of (title, path) pairs.
/// Handles `#EXTINF` titles, relative paths, absolute paths, and URLs.
fn parse_m3u(file_path: &Path) -> Vec<(String, PathBuf)> {
    let dir = file_path.parent().unwrap_or(Path::new("."));
    let Ok(content) = std::fs::read_to_string(file_path) else {
        log::warn!("Could not read M3U file: {:?}", file_path);
        return vec![];
    };

    let mut result = Vec::new();
    let mut pending_title: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // #EXTINF:duration,Title
            if let Some((_, title)) = rest.split_once(',') {
                let title = title.trim();
                if !title.is_empty() {
                    pending_title = Some(title.to_string());
                }
            }
        } else if line.starts_with('#') {
            continue; // skip other comments / directives
        } else {
            let path = if line.starts_with("http://") || line.starts_with("https://") {
                PathBuf::from(line)
            } else {
                let p = Path::new(line);
                if p.is_absolute() { p.to_path_buf() } else { dir.join(p) }
            };
            let title = pending_title.take().unwrap_or_else(|| title_for_path(&path));
            result.push((title, path));
        }
    }
    result
}

/// Returns true if the given path is an M3U/M3U8 playlist.
fn is_m3u(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).as_deref(),
        Some("m3u") | Some("m3u8")
    )
}

/// Load a pre-built list of (title, path) items into the playlist, in order
/// (no sorting).  If `play_first` is true, open the first item automatically.
fn load_playlist_items(
    items: Vec<(String, PathBuf)>,
    state: &SharedState,
    playlist_ui: &PlaylistPanel,
    play_first: bool,
) {
    {
        let mut s = state.borrow_mut();
        s.playlist = items.iter().map(|(_, p)| p.clone()).collect();
        s.current_idx = if items.is_empty() { None } else { Some(0) };
    }

    playlist_ui.clear();
    for (title, path) in &items {
        playlist_ui.add_item(title, path);
    }

    if play_first {
        if let Some((_, path)) = items.first().cloned() {
            state.borrow_mut().pending_seek = None;
            playlist_ui.select_row(0);
            if let Some(p) = state.borrow().player.as_ref() {
                p.execute(PlayerCommand::Open(path)).ok();
            }
        }
    }
}

/// Sort `paths` in natural order, update `state.playlist`, and populate the
/// playlist UI.  If `play_first` is true, open the first file automatically.
fn load_playlist(
    paths: Vec<PathBuf>,
    state: &SharedState,
    playlist_ui: &PlaylistPanel,
    play_first: bool,
) {
    let mut paths = paths;

    // Sort only file-path playlists; URL playlists preserve user-defined order.
    let is_url_playlist = paths.first()
        .map(|p| { let s = p.to_string_lossy(); s.starts_with("http://") || s.starts_with("https://") })
        .unwrap_or(false);
    if !is_url_playlist {
        paths.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                .cmp(
                    &b.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase(),
                )
        });
    }

    let items: Vec<(String, PathBuf)> = paths
        .into_iter()
        .map(|p| { let t = title_for_path(&p); (t, p) })
        .collect();
    load_playlist_items(items, state, playlist_ui, play_first);
}

/// Resolve a dropped file or folder into (title, path) pairs.
/// Does NOT handle M3U — those are handled separately.
fn resolve_drop(path: &Path) -> Vec<(String, PathBuf)> {
    if path.is_dir() {
        scan_directory(path)
            .into_iter()
            .map(|item| (title_for_path(&item.path), item.path))
            .collect()
    } else {
        vec![(title_for_path(path), path.to_path_buf())]
    }
}

/// Append items to the existing queue without replacing it.
/// Starts playback automatically only when the queue was empty before.
fn append_to_playlist(
    items: Vec<(String, PathBuf)>,
    state: &SharedState,
    playlist_ui: &PlaylistPanel,
) {
    let playlist_was_empty = state.borrow().playlist.is_empty();
    let start_idx = state.borrow().playlist.len();

    {
        let mut s = state.borrow_mut();
        for (_, path) in &items {
            s.playlist.push(path.clone());
        }
        if s.current_idx.is_none() && !items.is_empty() {
            s.current_idx = Some(start_idx);
        }
    }

    for (title, path) in &items {
        playlist_ui.add_item(title, path);
    }

    // Auto-play only when nothing was queued before the drop.
    if playlist_was_empty {
        if let Some((_, path)) = items.first().cloned() {
            state.borrow_mut().pending_seek = None;
            playlist_ui.select_row(start_idx);
            if let Some(p) = state.borrow().player.as_ref() {
                p.execute(PlayerCommand::Open(path)).ok();
            }
        }
    }
}

// ── MediaWindow ───────────────────────────────────────────────────────────────

impl MediaWindow {
    pub fn new(app: &adw::Application) -> Self {
        // ── MPRIS server (background thread) ──────────────────────────────
        let (mpris, mpris_cmd_rx) = crate::mpris::spawn();
        let mpris = Rc::new(mpris);
        let mpris_cmd_rx = Rc::new(mpris_cmd_rx);

        // ── Shared player state ───────────────────────────────────────────
        let state = PlayerState::create();

        // ── Restore global volume/mute from settings ──────────────────────
        let fixed_mode = {
            let settings = super::headerbar::load_app_settings();
            if let Some(p) = state.borrow().player.as_ref() {
                p.execute(PlayerCommand::SetVolume(settings.volume)).ok();
                p.execute(PlayerCommand::Mute(settings.muted)).ok();
            }
            state.borrow_mut().muted = settings.muted;
            settings.ui_mode.as_deref() == Some("fixed")
        };

        // ── Background mpv snapshot thread ────────────────────────────────
        // Reads all commonly-polled mpv properties on a dedicated thread so
        // the GTK main thread never blocks on mpv IPC (which can stall 1+ s
        // when mpv buffers IPTV streams).
        let snapshot: Arc<Mutex<MpvSnapshot>> = Arc::new(Mutex::new(MpvSnapshot::idle_defaults()));
        if let Some(poller) = state.borrow().player.as_ref().map(|p| p.make_poller()) {
            let snapshot_bg = snapshot.clone();
            std::thread::spawn(move || loop {
                let snap = poller.read_snapshot();
                if let Ok(mut g) = snapshot_bg.lock() { *g = snap; }
                std::thread::sleep(std::time::Duration::from_millis(80));
            });
        }

        // ── Root window ───────────────────────────────────────────────────
        // In fixed mode the control bar (~95 px) is always visible, so the
        // default and minimum heights need to be larger to avoid clipping.
        // Minimum height: header ~47px + video min 120px + controls ~95px (fixed) or ~85px (floating)
        let min_height = if fixed_mode { 625 } else { 625 };
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Aurora Media Player")
            .default_width(960)
            .default_height(if fixed_mode { 695 } else { 600 })
            .build();
        // 580px: minimum for the floating controls bar to render without overflow.
        // The playlist sidebar adds its own minimum (200px) on top when visible.
        window.set_size_request(580, min_height);

        // ── UI components ─────────────────────────────────────────────────
        // toast_overlay is created early so the screenshot callback can reference it.
        let toast_overlay = ToastOverlay::new();

        // Playlist is created first so it can be referenced in the header callback.
        let playlist = Rc::new(PlaylistPanel::new(state.clone()));
        let video = Rc::new(VideoArea::new(state.clone()));
        let controls = Rc::new(PlayerControls::new(state.clone(), {
            let toast_w = toast_overlay.downgrade();
            let window_w = window.downgrade();
            move |tmp_path: std::path::PathBuf| {
                let dialog = gtk::FileDialog::builder()
                    .title(crate::i18n::t("Save Screenshot"))
                    .initial_name("screenshot.png")
                    .build();
                let parent = window_w.upgrade().map(|w| w.upcast::<gtk::Window>());
                let toast_w2 = toast_w.clone();
                let tmp = tmp_path.clone();
                dialog.save(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                    if let Ok(file) = result {
                        if let Some(dest) = file.path() {
                            if std::fs::copy(&tmp, &dest).is_ok() {
                                if let Some(t) = toast_w2.upgrade() {
                                    t.add_toast(adw::Toast::new(crate::i18n::t("Screenshot saved")));
                                }
                            }
                        }
                    }
                    std::fs::remove_file(&tmp).ok();
                });
            }
        }));
        controls.apply_layout(fixed_mode);

        // ── Layout ────────────────────────────────────────────────────────
        // (fixed_mode already determined above, before window creation)
        let is_fixed_mode: Rc<Cell<bool>> = Rc::new(Cell::new(fixed_mode));

        // Always create the revealer; in fixed mode it starts detached from the tree.
        let controls_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .transition_duration(220)
            .reveal_child(true)
            .valign(gtk::Align::End)
            .hexpand(true)
            .halign(gtk::Align::Fill)
            .build();

        let video_controls = gtk::Overlay::builder()
            .child(video.widget())
            .hexpand(true)
            .vexpand(true)
            .height_request(120)
            // Minimum width so the floating controls bar never gets clipped by the playlist.
            .width_request(580)
            .build();

        // Initial floating setup: controls inside revealer, overlaid on video.
        if !fixed_mode {
            controls.widget().set_valign(gtk::Align::End);
            controls_revealer.set_child(Some(controls.widget()));
            video_controls.add_overlay(&controls_revealer);
            // Clip the controls revealer to the video area so it never bleeds into the playlist.
            video_controls.set_clip_overlay(&controls_revealer, true);
        }

        // Thumbnail overlay added AFTER controls_revealer so it renders on top.
        video_controls.add_overlay(controls.thumb_widget());
        controls.set_video_overlay(&video_controls);

        // ── Resizable paned layout ────────────────────────────────────────
        // gtk::Paned provides native smooth drag-resize without custom gesture
        // callbacks, avoiding the GLArea/mpv glitch that appears when using
        // OverlaySplitView + set_sidebar_width_fraction on every mouse-move.
        //
        // resize_start_child=true  → video grows/shrinks with the window
        // resize_end_child=false   → playlist keeps its pixel width on window resize
        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .hexpand(true)
            .vexpand(true)
            .build();
        const MIN_SIDEBAR_PX: i32 = 200;

        paned.set_start_child(Some(&video_controls));
        paned.set_end_child(Some(playlist.widget()));
        // Enforce sidebar minimum so GTK Paned computes window minimum correctly:
        // when playlist is visible → min window width = 460 (video) + 200 (sidebar).
        // When hidden, only video_controls (460px) contributes → window can be narrower.
        playlist.widget().set_size_request(MIN_SIDEBAR_PX, -1);
        // Playlist starts hidden; toggle button is inactive by default.
        playlist.widget().set_visible(false);

        // Preferred sidebar width in pixels — updated when user drags the handle.
        let sidebar_px: Rc<Cell<i32>> = Rc::new(Cell::new(320));
        // Max sidebar = 40% of window width, capped at 600px for ultra-wide screens.
        let max_sidebar = |w: i32| -> i32 { (w * 40 / 100).min(600) };

        // Enforce min/max sidebar width when the user drags the Paned handle.
        paned.connect_position_notify({
            let win_w = window.downgrade();
            move |paned| {
                let Some(win) = win_w.upgrade() else { return };
                let w = win.width();
                if w <= 0 { return }
                let max_px  = (w * 40 / 100).min(600);
                let sidebar_w = w - paned.position();
                if sidebar_w > max_px {
                    paned.set_position(w - max_px);
                } else if sidebar_w < MIN_SIDEBAR_PX {
                    paned.set_position(w - MIN_SIDEBAR_PX);
                }
            }
        });

        // Always use a VBox outer container so we can append controls below in fixed mode.
        let outer_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();

        // Initial fixed setup: controls below the toolbar_view, with flat CSS.
        if fixed_mode {
            controls.widget().add_css_class("controls-bar-fixed");
        }

        // ── Dynamic mode switching callback ─────────────────────────────────
        let controls_for_layout = controls.clone();
        let on_ui_mode_change: Rc<dyn Fn(&str)> = {
            let is_fixed_mode = is_fixed_mode.clone();
            let controls_revealer = controls_revealer.clone();
            let video_controls = video_controls.clone();
            let outer_box = outer_box.clone();
            let controls_widget = controls.widget().clone();
            Rc::new(move |mode: &str| {
                let now_fixed = mode == "fixed";
                if now_fixed == is_fixed_mode.get() { return; }
                is_fixed_mode.set(now_fixed);
                controls_for_layout.apply_layout(now_fixed);
                if now_fixed {
                    // Floating → Fixed: unparent from revealer, add to outer_box
                    controls_revealer.set_child(None::<&gtk::Widget>);
                    video_controls.remove_overlay(&controls_revealer);
                    controls_widget.add_css_class("controls-bar-fixed");
                    outer_box.append(&controls_widget);
                } else {
                    // Fixed → Floating: unparent from outer_box, put back in revealer
                    outer_box.remove(&controls_widget);
                    controls_widget.remove_css_class("controls-bar-fixed");
                    controls_widget.set_valign(gtk::Align::End);
                    controls_revealer.set_child(Some(&controls_widget));
                    controls_revealer.set_reveal_child(true);
                    video_controls.add_overlay(&controls_revealer);
                    video_controls.set_clip_overlay(&controls_revealer, true);
                }
            })
        };

        // Slot filled after `header` is created — lets the language-change callback
        // reach back into the main-window components without a circular dependency.
        let lang_change_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let on_language_change: Rc<dyn Fn()> = {
            let slot = lang_change_slot.clone();
            Rc::new(move || { if let Some(f) = &*slot.borrow() { f(); } })
        };

        let header = Rc::new({
            let state_file = state.clone();
            let playlist_file = playlist.clone();
            let state_url = state.clone();
            let playlist_url = playlist.clone();
            let state_sub = state.clone();
            MediaHeaderBar::new(
                state.clone(),
                move |path: PathBuf| {
                    if is_m3u(&path) {
                        let items = parse_m3u(&path);
                        if !items.is_empty() {
                            load_playlist_items(items, &state_file, &playlist_file, true);
                        }
                    } else {
                        let title = path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("?")
                            .to_string();
                        load_playlist_items(vec![(title, path)], &state_file, &playlist_file, true);
                    }
                },
                move |items: Vec<(String, String)>| {
                    let items: Vec<(String, PathBuf)> = items
                        .into_iter()
                        .map(|(t, u)| (t, PathBuf::from(u)))
                        .collect();
                    state_url.borrow_mut().pending_seek = None;
                    load_playlist_items(items, &state_url, &playlist_url, true);
                },
                move |path: PathBuf| {
                    if let Some(p) = state_sub.borrow().player.as_ref() {
                        p.execute(PlayerCommand::AddSubtitle(path)).ok();
                    }
                },
                {
                    let state_recent = state.clone();
                    let playlist_recent = playlist.clone();
                    move |path: PathBuf| {
                        let is_url = {
                            let s = path.to_string_lossy();
                            s.starts_with("http://") || s.starts_with("https://")
                        };
                        if !is_url && is_m3u(&path) {
                            let items = parse_m3u(&path);
                            if !items.is_empty() {
                                load_playlist_items(items, &state_recent, &playlist_recent, true);
                            }
                        } else {
                            let title = title_for_path(&path);
                            load_playlist_items(vec![(title, path)], &state_recent, &playlist_recent, true);
                        }
                    }
                },
                on_ui_mode_change,
                on_language_change,
            )
        });

        // Fill the slot now that all components exist.
        *lang_change_slot.borrow_mut() = Some({
            let header_c   = header.clone();
            let controls_c = controls.clone();
            let playlist_c = playlist.clone();
            let video_c2   = video.clone();
            Rc::new(move || {
                header_c.relabel();
                controls_c.relabel();
                playlist_c.relabel();
                video_c2.relabel();
            })
        });

        let push_recent = header.push_recent_fn.clone();

        let toolbar_view = ToolbarView::builder()
            .content(&paned)
            .build();
        toolbar_view.set_vexpand(true);
        toolbar_view.add_top_bar(header.widget());

        outer_box.append(&toolbar_view);

        // In fixed mode, append controls below the toolbar_view.
        if fixed_mode {
            outer_box.append(controls.widget());
        }

        toast_overlay.set_child(Some(&outer_box));
        window.set_content(Some(&toast_overlay));

        // ── Playlist toggle ───────────────────────────────────────────────
        {
            let paned_w    = paned.downgrade();
            let playlist_w = playlist.widget().downgrade();
            let window_w   = window.downgrade();
            let px         = sidebar_px.clone();
            header.playlist_btn.connect_toggled(move |btn| {
                let Some(paned)    = paned_w.upgrade()    else { return };
                let Some(playlist) = playlist_w.upgrade() else { return };
                if btn.is_active() {
                    playlist.set_visible(true);
                    let w = paned.width();
                    if w > 0 {
                        paned.set_position((w - px.get()).max(w - max_sidebar(w)).max(0));
                    }
                    // Enforce wider minimum so video area + sidebar both fit.
                    if let Some(win) = window_w.upgrade() {
                        win.set_size_request(580 + MIN_SIDEBAR_PX, min_height);
                    }
                } else {
                    // Persist current sidebar width before hiding.
                    let w   = paned.width();
                    let pos = paned.position();
                    if w > 0 && pos > 0 && pos < w {
                        px.set((w - pos).clamp(MIN_SIDEBAR_PX, max_sidebar(w)));
                    }
                    playlist.set_visible(false);
                    // Restore narrower minimum when playlist is closed.
                    if let Some(win) = window_w.upgrade() {
                        win.set_size_request(580, min_height);
                    }
                }
            });
        }


        // ── Sync maximize button ↔ fullscreen ────────────────────────────
        // When the user clicks the title-bar restore button while fullscreen,
        // detect the unmaximize and also exit fullscreen.
        {
            let window_c = window.downgrade();
            window.connect_notify_local(Some("maximized"), move |_, _| {
                if let Some(win) = window_c.upgrade() {
                    if !win.property::<bool>("maximized") && win.property::<bool>("fullscreened") {
                        win.unfullscreen();
                    }
                }
            });
        }

        // ── Breakpoint: hide playlist on narrow windows ───────────────────
        // On windows narrower than 720sp the sidebar would be too cramped;
        // hide the toggle button and force the playlist off.
        let bp = Breakpoint::new(BreakpointCondition::parse("max-width: 720sp").unwrap());
        bp.add_setter(&header.playlist_btn, "visible", &false.to_value());
        bp.connect_apply({
            let btn = header.playlist_btn.downgrade();
            move |_| { if let Some(b) = btn.upgrade() { b.set_active(false); } }
        });
        window.add_breakpoint(bp);

        // ── Drag & drop ───────────────────────────────────────────────────
        // Accept files AND folders; folders are scanned recursively.
        let drop_target =
            gtk::DropTarget::new(gio::File::static_type(), gdk4::DragAction::COPY);
        {
            let state_c = state.clone();
            let playlist_c = playlist.clone();
            drop_target.connect_drop(move |_, value, _, _| {
                if let Ok(file) = value.get::<gio::File>() {
                    if let Some(path) = file.path() {
                        if is_m3u(&path) {
                            // M3U: load as a fresh playlist (replaces queue)
                            let items = parse_m3u(&path);
                            if !items.is_empty() {
                                load_playlist_items(items, &state_c, &playlist_c, true);
                            }
                        } else {
                            // Local file / folder: append to the existing queue
                            let items = resolve_drop(&path);
                            if !items.is_empty() {
                                append_to_playlist(items, &state_c, &playlist_c);
                            }
                        }
                    }
                }
                true
            });
        }
        window.add_controller(drop_target);

        // ── Click on video: single → play/pause, double → fullscreen ─────
        {
            let window_weak = window.downgrade();
            let state_c = state.clone();
            let dbl_click = gtk::GestureClick::new();
            dbl_click.connect_pressed(move |_, n_press, _, _| {
                if n_press == 1 {
                    if let Some(p) = state_c.borrow().player.as_ref() {
                        p.execute(PlayerCommand::TogglePause).ok();
                    }
                } else if n_press == 2 {
                    // Undo the pause toggle that fired on n_press == 1
                    if let Some(p) = state_c.borrow().player.as_ref() {
                        p.execute(PlayerCommand::TogglePause).ok();
                    }
                    if let Some(win) = window_weak.upgrade() {
                        if win.property::<bool>("fullscreened") {
                            win.unfullscreen();
                            win.unmaximize();
                        } else {
                            win.maximize();
                            win.fullscreen();
                        }
                    }
                }
            });
            video.widget().add_controller(dbl_click);
        }

        // ── Keyboard shortcuts ────────────────────────────────────────────
        {
            let state_c        = state.clone();
            let win_weak       = window.downgrade();
            let toast_w_kb     = toast_overlay.downgrade();
            let playlist_btn_w = header.playlist_btn.downgrade();
            let open_file_w    = header.open_file_btn.downgrade();
            let open_url_w     = header.open_url_btn.downgrade();
            let open_sub_w     = header.open_sub_btn.downgrade();
            let settings_w     = header.settings_btn.downgrade();
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_ctrl.connect_key_pressed(move |_, key, _, modifiers| {
                // Don't steal keys when a text entry has focus.
                if let Some(win) = win_weak.upgrade() {
                    use gtk4::prelude::GtkWindowExt;
                    if gtk4::prelude::GtkWindowExt::focus(&win)
                        .map(|w: gtk::Widget| w.is::<gtk::Text>() || w.is::<gtk::Entry>() || w.is::<gtk::SearchEntry>())
                        .unwrap_or(false)
                    {
                        return glib::Propagation::Proceed;
                    }
                }

                let ctrl  = modifiers.contains(gdk4::ModifierType::CONTROL_MASK);
                let shift = modifiers.contains(gdk4::ModifierType::SHIFT_MASK);

                // ── Helper: seek relative ────────────────────────────────
                let seek = |delta: f64| {
                    let s = state_c.borrow();
                    if let Some(p) = s.player.as_ref() {
                        if let Some(pos) = p.position() {
                            let dur = p.duration().unwrap_or(f64::MAX);
                            p.execute(PlayerCommand::Seek((pos + delta).clamp(0.0, dur))).ok();
                        }
                    }
                };

                match key {
                    // ── Playback ─────────────────────────────────────────
                    gdk4::Key::space => {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::TogglePause).ok();
                        }
                    }

                    // ── Seek ─────────────────────────────────────────────
                    gdk4::Key::Left  if shift => seek(-30.0),
                    gdk4::Key::Left            => seek(-5.0),
                    gdk4::Key::Right if shift  => seek(30.0),
                    gdk4::Key::Right           => seek(5.0),

                    // ── Volume ───────────────────────────────────────────
                    gdk4::Key::Up => {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            let v = (p.volume() + 5.0).min(100.0);
                            p.execute(PlayerCommand::SetVolume(v)).ok();
                        }
                    }
                    gdk4::Key::Down => {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            let v = (p.volume() - 5.0).max(0.0);
                            p.execute(PlayerCommand::SetVolume(v)).ok();
                        }
                    }
                    gdk4::Key::m | gdk4::Key::M => {
                        let muted = {
                            let mut s = state_c.borrow_mut();
                            s.muted = !s.muted;
                            s.muted
                        };
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::Mute(muted)).ok();
                        }
                    }

                    // ── Tracks ───────────────────────────────────────────
                    gdk4::Key::n | gdk4::Key::N => {
                        let mut s = state_c.borrow_mut();
                        let next = s.current_idx
                            .and_then(|i| s.effective_next_idx(i))
                            .and_then(|i| s.playlist.get(i).cloned().map(|p| (i, p)));
                        if let Some((idx, path)) = next {
                            s.current_idx = Some(idx);
                            drop(s);
                            if let Some(p) = state_c.borrow().player.as_ref() {
                                p.execute(PlayerCommand::Open(path)).ok();
                            }
                        }
                    }
                    gdk4::Key::b | gdk4::Key::B => {
                        let mut s = state_c.borrow_mut();
                        let prev = s.current_idx
                            .and_then(|i| i.checked_sub(1))
                            .and_then(|i| s.playlist.get(i).cloned().map(|p| (i, p)));
                        if let Some((idx, path)) = prev {
                            s.current_idx = Some(idx);
                            drop(s);
                            if let Some(p) = state_c.borrow().player.as_ref() {
                                p.execute(PlayerCommand::Open(path)).ok();
                            }
                        }
                    }

                    // ── Speed ────────────────────────────────────────────
                    gdk4::Key::bracketright => {
                        const STEPS: &[f64] = &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            let cur = p.speed();
                            let next = STEPS.iter().find(|&&s| s > cur + 0.01).copied().unwrap_or(2.0);
                            p.execute(PlayerCommand::SetSpeed(next)).ok();
                        }
                    }
                    gdk4::Key::bracketleft => {
                        const STEPS: &[f64] = &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            let cur = p.speed();
                            let prev = STEPS.iter().rev().find(|&&s| s < cur - 0.01).copied().unwrap_or(0.25);
                            p.execute(PlayerCommand::SetSpeed(prev)).ok();
                        }
                    }
                    gdk4::Key::BackSpace => {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::SetSpeed(1.0)).ok();
                        }
                    }

                    // ── Screenshot ───────────────────────────────────────
                    gdk4::Key::s | gdk4::Key::S => {
                        let tmp_path = std::env::temp_dir().join(format!(
                            "aurora-screenshot-{}.png",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis()
                        ));
                        let saved = if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::ScreenshotToFile(tmp_path.clone())).is_ok()
                        } else {
                            false
                        };
                        if saved {
                            let dialog = gtk::FileDialog::builder()
                                .title(crate::i18n::t("Save Screenshot"))
                                .initial_name("screenshot.png")
                                .build();
                            let parent = win_weak.upgrade().map(|w| w.upcast::<gtk::Window>());
                            let toast_w2 = toast_w_kb.clone();
                            let tmp = tmp_path.clone();
                            dialog.save(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                                if let Ok(file) = result {
                                    if let Some(dest) = file.path() {
                                        if std::fs::copy(&tmp, &dest).is_ok() {
                                            if let Some(t) = toast_w2.upgrade() {
                                                t.add_toast(adw::Toast::new(crate::i18n::t("Screenshot saved")));
                                            }
                                        }
                                    }
                                }
                                std::fs::remove_file(&tmp).ok();
                            });
                        }
                    }

                    // ── File menu shortcuts ───────────────────────────────
                    gdk4::Key::o | gdk4::Key::O if ctrl => {
                        if let Some(b) = open_file_w.upgrade() { b.activate(); }
                    }
                    gdk4::Key::u | gdk4::Key::U if ctrl => {
                        if let Some(b) = open_url_w.upgrade() { b.activate(); }
                    }
                    gdk4::Key::t | gdk4::Key::T if ctrl => {
                        if let Some(b) = open_sub_w.upgrade() { b.activate(); }
                    }
                    gdk4::Key::comma if ctrl => {
                        if let Some(b) = settings_w.upgrade() { b.activate(); }
                    }

                    // ── Playlist sidebar (Ctrl+P) ─────────────────────────
                    gdk4::Key::p | gdk4::Key::P if ctrl => {
                        if let Some(b) = playlist_btn_w.upgrade() {
                            b.set_active(!b.is_active());
                        }
                    }

                    // ── Fullscreen ───────────────────────────────────────
                    gdk4::Key::f | gdk4::Key::F | gdk4::Key::F11 => {
                        if let Some(win) = win_weak.upgrade() {
                            if win.property::<bool>("fullscreened") {
                                win.unfullscreen();
                                win.unmaximize();
                            } else {
                                win.maximize();
                                win.fullscreen();
                            }
                        }
                    }
                    gdk4::Key::Escape => {
                        if let Some(win) = win_weak.upgrade() {
                            if win.property::<bool>("fullscreened") {
                                win.unfullscreen();
                                win.unmaximize();
                            }
                        }
                    }

                    _ => return glib::Propagation::Proceed,
                }
                let _ = (ctrl, shift); // suppress unused warnings
                glib::Propagation::Stop
            });
            window.add_controller(key_ctrl);
        }

        // ── Track whether mouse is over the controls bar ──────────────────
        let mouse_over_controls: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let moc_enter = mouse_over_controls.clone();
            let moc_leave = mouse_over_controls.clone();
            let mc = gtk::EventControllerMotion::new();
            mc.connect_enter(move |_, _, _| { moc_enter.set(true); });
            mc.connect_leave(move |_| { moc_leave.set(false); });
            controls_revealer.add_controller(mc);
        }

        // ── Mouse motion → reset auto-hide timer ──────────────────────────
        // Use a 4-px threshold so sub-pixel jitter from the compositor doesn't
        // keep resetting the timer when the user's mouse is effectively still.
        let last_motion = Rc::new(Cell::new(Instant::now()));
        let last_cursor_pos = Rc::new(Cell::new((-999.0f64, -999.0f64)));
        {
            let last_motion_c = last_motion.clone();
            let last_pos_c = last_cursor_pos.clone();
            let toolbar_view_c = toolbar_view.downgrade();
            let controls_revealer_c = controls_revealer.downgrade();
            let playlist_widget_c = playlist.widget().downgrade();
            let playlist_btn_c = header.playlist_btn.downgrade();
            let window_c = window.downgrade();
            let motion_ctrl = gtk::EventControllerMotion::new();
            motion_ctrl.connect_motion(move |_, x, y| {
                let (px, py) = last_pos_c.get();
                if (x - px).abs() <= 4.0 && (y - py).abs() <= 4.0 {
                    return; // ignore micro-jitter
                }
                last_pos_c.set((x, y));
                last_motion_c.set(Instant::now());
                if let Some(tv) = toolbar_view_c.upgrade() {
                    tv.set_reveal_top_bars(true);
                }
                if let Some(r) = controls_revealer_c.upgrade() {
                    r.set_reveal_child(true);
                }
                if let (Some(pw), Some(btn)) = (playlist_widget_c.upgrade(), playlist_btn_c.upgrade()) {
                    pw.set_visible(btn.is_active());
                }
                if let Some(win) = window_c.upgrade() {
                    win.set_cursor(None::<&gdk4::Cursor>);
                }
            });
            window.add_controller(motion_ctrl);
        }

        // ── Session restore ───────────────────────────────────────────────
        if let Some(session) = load_session() {
            // Grab the previously-playing path before consuming the vec.
            let prev_current = session.current_idx.and_then(|i| session.playlist.get(i).cloned());
            // Drop files that were moved or deleted since last run.
            let paths: Vec<PathBuf> = session.playlist.into_iter().filter(|p| p.exists()).collect();
            if !paths.is_empty() {
                // Find the new index of the previously-playing file; fall back to 0.
                let new_idx = prev_current
                    .as_ref()
                    .and_then(|orig| paths.iter().position(|p| p == orig))
                    .or(Some(0));
                {
                    let mut s = state.borrow_mut();
                    s.playlist = paths.clone();
                    s.current_idx = new_idx;
                }
                for path in &paths {
                    let title = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string();
                    playlist.add_item(&title, path);
                }
                // Defer opening the file until the GL render context is ready.
                // With vo=libmpv, loadfile before the render context causes mpv
                // to fail video output init and stay idle.
                if let Some(idx) = new_idx {
                    if let Some(path) = paths.get(idx).cloned() {
                        playlist.select_row(idx);
                        state.borrow_mut().pending_open = Some(path);
                        if session.position > 1.0 {
                            state.borrow_mut().pending_seek = Some(session.position);
                        }
                    }
                }
            }
        }

        // ── Session save on close ─────────────────────────────────────────
        {
            let state_c = state.clone();
            window.connect_close_request(move |_| {
                let pos = state_c
                    .borrow()
                    .player
                    .as_ref()
                    .and_then(|p| p.position())
                    .unwrap_or(0.0);
                save_session(&state_c, pos);
                glib::Propagation::Proceed
            });
        }

        // ── Fast position timer: seek bar + time labels at 50 ms ─────────
        {
            let controls_c = controls.clone();
            let pos_tick = Rc::new(Cell::new(0u8));
            let snapshot_50 = snapshot.clone();
            glib::timeout_add_local(Duration::from_millis(50), move || {
                let snap = match snapshot_50.lock() {
                    Ok(g) => g.clone(),
                    Err(_) => return glib::ControlFlow::Continue,
                };
                if snap.idle { return glib::ControlFlow::Continue; }
                let tick = pos_tick.get().wrapping_add(1);
                pos_tick.set(tick);
                // Active: every tick (50 ms).
                // Paused: every 10th tick (500 ms) — position is static, no need for 20 fps.
                if !snap.paused || tick % 10 == 0 {
                    controls_c.update_position(snap.pos, snap.dur);
                }
                glib::ControlFlow::Continue
            });
        }

        // ── Polling timeout: sync controls, auto-advance, auto-hide ───────
        let window_weak = window.downgrade();
        let toolbar_view_weak = toolbar_view.downgrade();
        let controls_revealer_weak = controls_revealer.downgrade();
        let is_fixed_mode_c = is_fixed_mode.clone();
        let mouse_over_controls_c = mouse_over_controls.clone();
        let playlist_widget_weak = playlist.widget().downgrade();
        let playlist_btn_weak = header.playlist_btn.downgrade();
        let state_c = state.clone();
        let controls_c = controls.clone();
        let playlist_c = playlist.clone();
        let video_c = video.clone();
        let mpris_c = mpris.clone();
        let mpris_cmd_rx_c = mpris_cmd_rx.clone();
        let toast_overlay_c = toast_overlay.clone();
        let push_recent_c = push_recent.clone();
        let window_title_c = header.window_title.clone();
        // Cooldown counter to avoid double-advancing when eof is briefly still true.
        let advance_cooldown = Rc::new(Cell::new(0u32));
        // Track previous idle state to detect unexpected stops (playback errors).
        let prev_idle = Rc::new(Cell::new(true));
        // Track volume/mute to persist changes to global settings.
        let last_saved_volume = Rc::new(Cell::new(super::headerbar::load_app_settings().volume));
        let last_saved_muted = Rc::new(Cell::new(super::headerbar::load_app_settings().muted));
        // For URL items, defer the recent-files push until yt-dlp resolves the real title.
        let recent_pending_idx: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        // Track current_idx so any external change (prev/next buttons) syncs the playlist UI.
        let last_known_idx: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
        // Detect frozen playback (post-seek buffer stall) by watching time-pos.
        let prev_pos = Rc::new(Cell::new(-1.0_f64));
        let stuck_ticks = Rc::new(Cell::new(0u32));
        // Throttle expensive mpv IPC calls: track_list (every 10 ticks=2s), chapters (every 5=1s).
        let slow_tick = Rc::new(Cell::new(0u32));
        // Debounce volume/mute disk writes: only flush after N ticks of no further changes.
        let settings_save_cooldown = Rc::new(Cell::new(0u32));
        let snapshot_200 = snapshot.clone();
        let fallback_icon_uri = aurora_icon_uri();
        // Track the last file+duration key for which thumbnails were generated
        // to avoid re-running ffmpeg every 200 ms for the same file.
        let last_thumb_key: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

        glib::timeout_add_local(Duration::from_millis(200), move || {
            let _tick_start = std::time::Instant::now();
            // Read mpv properties from the background-thread snapshot — no blocking IPC.
            let (pos, dur, paused, muted, volume, speed, title, idle, has_video,
                 artist, album, thumbnail, eof, buffering, seeking, snap_path) = {
                let snap = match snapshot_200.lock() {
                    Ok(g) => g.clone(),
                    Err(_) => return glib::ControlFlow::Continue,
                };
                // For online streams (YouTube etc.), artist may come from uploader/channel.
                let artist = snap.artist
                    .or(snap.uploader)
                    .unwrap_or_default();
                // For YouTube, derive the thumbnail URL from the video ID when no
                // embedded thumbnail is available.
                let thumbnail = snap.thumbnail.unwrap_or_default();
                let thumbnail = if thumbnail.is_empty() {
                    snap.path.as_deref()
                        .and_then(youtube_thumbnail_url)
                        .unwrap_or_default()
                } else {
                    thumbnail
                };
                (snap.pos, snap.dur, snap.paused, snap.muted, snap.volume, snap.speed,
                 snap.title, snap.idle, snap.has_video,
                 artist, snap.album.unwrap_or_default(),
                 thumbnail,
                 snap.eof, snap.buffering, snap.seeking,
                 snap.path)
            };
            // Read Rust-side state (no mpv IPC — always fast).
            let (pending_seek, repeat_mode, shuffle, podcast_mode, has_prev, has_next) = {
                let s = state_c.borrow();
                if s.player.is_none() { return glib::ControlFlow::Continue; }
                let cur = s.current_idx;
                let has_prev = !idle && cur.map(|i| {
                    if s.shuffle {
                        s.shuffle_order.iter().position(|&x| x == i).map(|p| p > 0).unwrap_or(false)
                    } else {
                        i > 0
                    }
                }).unwrap_or(false);
                let has_next = !idle && cur.map(|i| s.effective_next_idx(i).is_some()).unwrap_or(false);
                (s.pending_seek, s.repeat_mode, s.shuffle, s.podcast_mode, has_prev, has_next)
            };

            // ── Playback error detection ────────────────────────────────
            // If playback was active and suddenly becomes idle (not due to
            // EOF), mpv likely failed to open the source. Show a toast.
            let was_idle = prev_idle.get();
            if !was_idle && idle && !eof {
                let err_msg = state_c
                    .borrow()
                    .player
                    .as_ref()
                    .and_then(|p| p.last_error())
                    .unwrap_or_else(|| "Could not open the media source.".into());
                toast_overlay_c.add_toast(Toast::new(&err_msg));
            }
            prev_idle.set(idle);

            // ── Persist volume/mute to global settings when they change ──
            // Debounced: start a countdown on change, flush to disk only when it expires.
            // Avoids a disk read+write on the GTK main thread every 200 ms during volume drag.
            if (volume - last_saved_volume.get()).abs() > 0.5 || muted != last_saved_muted.get() {
                last_saved_volume.set(volume);
                last_saved_muted.set(muted);
                settings_save_cooldown.set(5); // flush after ~1 s of no further changes
            }
            let cd = settings_save_cooldown.get();
            if cd > 0 {
                settings_save_cooldown.set(cd - 1);
                if cd == 1 {
                    let mut s = super::headerbar::load_app_settings();
                    s.volume = last_saved_volume.get();
                    s.muted = last_saved_muted.get();
                    super::headerbar::save_app_settings(&s);
                }
            }

            // ── Recent files + playlist selection sync ───────────────────
            // Both are driven by current_idx changes so we handle them together.
            // This fires on every track change: initial play, auto-advance, prev/next,
            // and after idle (was_idle is no longer needed for recent tracking).
            {
                let cur_idx = state_c.borrow().current_idx;

                // Flush a pending URL recent entry when the player goes idle or
                // moves to a different track before the title could resolve.
                if let Some(pending) = recent_pending_idx.get() {
                    let track_changed = cur_idx != Some(pending);
                    if idle || track_changed {
                        let path = state_c.borrow().playlist.get(pending).cloned();
                        if let Some(path) = path {
                            let best = playlist_c.item_title(pending)
                                .unwrap_or_else(|| title_for_path(&path));
                            push_recent_c(&path, &best);
                        }
                        recent_pending_idx.set(None);
                    }
                }

                if cur_idx != last_known_idx.get() {
                    // New track started — add to recent (skip M3U, handled in push_recent_fn).
                    if !idle {
                        if let Some(idx) = cur_idx {
                            let path = state_c.borrow().playlist.get(idx).cloned();
                            if let Some(ref path) = path {
                                let path_str = path.to_string_lossy();
                                if path_str.starts_with("http://") || path_str.starts_with("https://") {
                                    // URL: defer until yt-dlp resolves the real title.
                                    recent_pending_idx.set(Some(idx));
                                } else {
                                    // Local file: title known immediately from filename.
                                    push_recent_c(path, &title_for_path(path));
                                }
                            }
                        }
                    }

                    // Sync playlist panel selection.
                    if let Some(idx) = cur_idx {
                        playlist_c.select_row(idx);
                    }
                    last_known_idx.set(cur_idx);
                }
            }

            // ── Pending seek (session restore) ─────────────────────────
            if let Some(seek_to) = pending_seek {
                if dur > 1.0 {
                    if let Some(p) = state_c.borrow().player.as_ref() {
                        p.execute(PlayerCommand::Seek(seek_to)).ok();
                    }
                    state_c.borrow_mut().pending_seek = None;
                }
            }

            // ── Auto-advance to next track ─────────────────────────────
            if advance_cooldown.get() > 0 {
                advance_cooldown.set(advance_cooldown.get() - 1);
            } else if eof && !idle {
                let next = {
                    let s = state_c.borrow();
                    s.current_idx
                        .and_then(|i| {
                            s.effective_next_idx(i).and_then(|next| {
                                s.playlist.get(next).cloned().map(|p| (next, p))
                            })
                        })
                };
                if let Some((next_idx, next_path)) = next {
                    state_c.borrow_mut().current_idx = Some(next_idx);
                    playlist_c.select_row(next_idx);
                    if let Some(p) = state_c.borrow().player.as_ref() {
                        p.execute(PlayerCommand::Open(next_path)).ok();
                    }
                    advance_cooldown.set(5); // 5 × 200 ms = 1 s cooldown
                }
            }

            controls_c.update(pos, dur, paused, muted, volume, speed, idle, has_video, repeat_mode, shuffle, podcast_mode, has_prev, has_next);

            // ── Seek-bar thumbnail strip ────────────────────────────────
            // Track every path change (local or URL) so switching to a URL
            // clears any thumbnails left over from the previous local file.
            {
                let new_key = if !idle && dur > 1.0 {
                    snap_path.as_deref().map(|p| p.to_owned()).unwrap_or_default()
                } else {
                    String::new()
                };

                if *last_thumb_key.borrow() != new_key {
                    *last_thumb_key.borrow_mut() = new_key.clone();

                    let is_local = !new_key.is_empty()
                        && !new_key.starts_with("http://")
                        && !new_key.starts_with("https://");

                    if is_local {
                        let new_cache = std::sync::Arc::new(std::sync::Mutex::new(None));
                        controls_c.set_thumb_cache(new_cache.clone());
                        crate::thumbnails::generate_async(
                            std::path::PathBuf::from(&new_key),
                            dur,
                            new_cache,
                        );
                    } else {
                        // URL or idle: clear any stale thumbnails.
                        controls_c.set_thumb_cache(
                            std::sync::Arc::new(std::sync::Mutex::new(None))
                        );
                    }
                }
            }

            // ── Header title / subtitle ─────────────────────────────────
            {
                let raw = title.as_deref().unwrap_or("");
                let is_url_noise = raw.starts_with("http://")
                    || raw.starts_with("https://")
                    || (raw.contains('?') && raw.contains('=') && !raw.contains(' '));
                let resolved = if idle || is_url_noise || raw.is_empty() { None } else { Some(raw) };
                match resolved {
                    None => {
                        window_title_c.set_title("Aurora Media Player");
                        window_title_c.set_subtitle("");
                    }
                    Some(name) => {
                        window_title_c.set_title(&format!("Aurora Media Player — {name}"));
                        window_title_c.set_subtitle(&artist);
                    }
                }
            }

            // ── Slow-tick counter for throttled IPC calls ───────────────
            let tick = slow_tick.get().wrapping_add(1);
            slow_tick.set(tick);

            // ── Tracks (audio + subtitles) — every ~2 s ─────────────────
            // track_list() is an mpv IPC call; no need to query it 5×/sec.
            if tick % 10 == 0 {
                let t0 = std::time::Instant::now();
                let tracks = {
                    let s = state_c.borrow();
                    s.player.as_ref().map(|p| p.track_list()).unwrap_or_default()
                };
                let elapsed = t0.elapsed().as_millis();
                if elapsed > 20 { log::warn!("[perf] track_list() took {}ms", elapsed); }
                controls_c.update_tracks(tracks, &state_c);
            }

            // ── Chapter marks on seek bar — every ~1 s ──────────────────
            if tick % 5 == 0 {
                let t0 = std::time::Instant::now();
                let chapters = {
                    let s = state_c.borrow();
                    s.player.as_ref().filter(|_| !idle)
                        .map(|p| p.chapter_list())
                        .unwrap_or_default()
                };
                let elapsed = t0.elapsed().as_millis();
                if elapsed > 20 { log::warn!("[perf] chapter_list() took {}ms", elapsed); }
                controls_c.update_chapters(chapters, dur);
            }

            // ── Update URL playlist row title once mpv/yt-dlp resolves it ─
            // Only replace if the title is still the hostname fallback — don't
            // clobber a proper #EXTINF channel name with mpv stream metadata.
            if !idle {
                if let (Some(real_title), Some(idx)) = (title.as_deref(), state_c.borrow().current_idx) {
                    let still_placeholder = state_c.borrow().playlist.get(idx)
                        .and_then(|p| {
                            let path_str = p.to_string_lossy();
                            // Only applies to URL items — file items don't need title updates.
                            if !path_str.starts_with("http://") && !path_str.starts_with("https://") {
                                return None;
                            }
                            let fallback = title_for_path(p);
                            playlist_c.item_title(idx).map(|t| {
                                // Still a placeholder if:
                                // 1. hostname fallback ("youtube.com")
                                // 2. mpv returned the raw full URL before yt-dlp resolved
                                // 3. mpv returned just the URL path fragment ("watch?v=xxx")
                                //    which has no spaces and contains a query-string marker
                                // Real titles (from yt-dlp) have spaces and no URL syntax.
                                t == fallback
                                    || t.starts_with("http://")
                                    || t.starts_with("https://")
                                    || (t.contains('?') && !t.contains(' '))
                            })
                        })
                        .unwrap_or(false);
                    if still_placeholder {
                        playlist_c.update_row_title(idx, real_title);

                        // If this is the real resolved title (not another URL fragment),
                        // push to recent now and clear the pending flag.
                        let title_is_real = !real_title.starts_with("http://")
                            && !real_title.starts_with("https://")
                            && !(real_title.contains('?') && !real_title.contains(' '));
                        if title_is_real && recent_pending_idx.get() == Some(idx) {
                            if let Some(path) = state_c.borrow().playlist.get(idx).cloned() {
                                push_recent_c(&path, real_title);
                                recent_pending_idx.set(None);
                            }
                        }
                    }
                }
            }

            // Detect frozen playback: position not advancing while not paused.
            // Triggers after 2 consecutive ticks (~400 ms) to avoid false positives.
            if !idle && !eof && !paused && dur > 0.0 {
                if (pos - prev_pos.get()).abs() < 0.05 {
                    stuck_ticks.set(stuck_ticks.get().saturating_add(1));
                } else {
                    stuck_ticks.set(0);
                }
            } else {
                stuck_ticks.set(0);
            }
            prev_pos.set(pos);
            let frozen = stuck_ticks.get() >= 2;

            // Show spinner when: initially loading, cache stall, seeking, or frozen after seek.
            let show_spinner = !idle && !eof && (buffering || seeking || dur == 0.0 || frozen);

            if idle {
                video_c.set_idle(true);
                video_c.set_audio_playing(false);
                video_c.set_buffering(false);
                video_c.show_video();
            } else if !podcast_mode && (has_video || dur == 0.0) {
                // While duration is unknown (initial load), stay on video page so
                // only the spinner is visible — no redundant "Loading…" text.
                video_c.set_idle(false);
                video_c.set_audio_playing(false);
                video_c.set_buffering(show_spinner);
                video_c.show_video();
            } else {
                let track_title = title.as_deref().unwrap_or("");
                video_c.set_buffering(seeking || buffering);
                video_c.show_audio(track_title, &artist, &album);
                video_c.set_audio_playing(!paused);
            }

            if let Some(win) = window_weak.upgrade() {
                win.set_title(Some("Aurora Media Player"));

                let idle_secs = last_motion.get().elapsed().as_secs_f64();
                let popover_open = controls_c.has_open_popover();
                let hide_after = if mouse_over_controls_c.get() || popover_open { 5.0 } else { 2.0 };

                if win.property::<bool>("fullscreened") {
                    if idle_secs > hide_after && !popover_open {
                        if let Some(tv) = toolbar_view_weak.upgrade() {
                            tv.set_reveal_top_bars(false);
                        }
                        if !is_fixed_mode_c.get() {
                            if let Some(r) = controls_revealer_weak.upgrade() {
                                r.set_reveal_child(false);
                            }
                        }
                        if let Some(pw) = playlist_widget_weak.upgrade() {
                            pw.set_visible(false);
                        }
                        win.set_cursor(
                            gdk4::Cursor::from_name("none", None::<&gdk4::Cursor>).as_ref(),
                        );
                    } else {
                        win.set_cursor(None::<&gdk4::Cursor>);
                    }
                } else {
                    if let Some(tv) = toolbar_view_weak.upgrade() {
                        tv.set_reveal_top_bars(true);
                    }
                    if !is_fixed_mode_c.get() {
                        if idle_secs > hide_after && !idle && !popover_open {
                            if let Some(r) = controls_revealer_weak.upgrade() {
                                r.set_reveal_child(false);
                            }
                        } else {
                            if let Some(r) = controls_revealer_weak.upgrade() {
                                r.set_reveal_child(true);
                            }
                            if let (Some(pw), Some(btn)) = (playlist_widget_weak.upgrade(), playlist_btn_weak.upgrade()) {
                                pw.set_visible(btn.is_active());
                            }
                        }
                    }
                    win.set_cursor(None::<&gdk4::Cursor>);
                }
            }

            // ── MPRIS: drain incoming commands ─────────────────────────
            while let Ok(cmd) = mpris_cmd_rx_c.try_recv() {
                match cmd {
                    MprisCommand::PlayPause => {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::TogglePause).ok();
                        }
                    }
                    MprisCommand::Next => {
                        let next = {
                            let s = state_c.borrow();
                            s.current_idx.and_then(|i| {
                                s.effective_next_idx(i).and_then(|ni| s.playlist.get(ni).cloned().map(|p| (ni, p)))
                            })
                        };
                        if let Some((idx, path)) = next {
                            state_c.borrow_mut().current_idx = Some(idx);
                            playlist_c.select_row(idx);
                            if let Some(p) = state_c.borrow().player.as_ref() {
                                p.execute(PlayerCommand::Open(path)).ok();
                            }
                            advance_cooldown.set(5);
                        }
                    }
                    MprisCommand::Previous => {
                        let prev = {
                            let s = state_c.borrow();
                            s.current_idx.and_then(|i| {
                                if i > 0 {
                                    s.playlist.get(i - 1).cloned().map(|p| (i - 1, p))
                                } else {
                                    None
                                }
                            })
                        };
                        if let Some((idx, path)) = prev {
                            state_c.borrow_mut().current_idx = Some(idx);
                            playlist_c.select_row(idx);
                            if let Some(p) = state_c.borrow().player.as_ref() {
                                p.execute(PlayerCommand::Open(path)).ok();
                            }
                            advance_cooldown.set(5);
                        }
                    }
                    MprisCommand::Seek(offset_us) => {
                        let s = state_c.borrow();
                        if let Some(p) = s.player.as_ref() {
                            let new_pos = (p.position().unwrap_or(0.0)
                                + offset_us as f64 / 1_000_000.0)
                                .max(0.0)
                                .min(p.duration().unwrap_or(f64::MAX));
                            p.execute(PlayerCommand::Seek(new_pos)).ok();
                        }
                    }
                    MprisCommand::SetVolume(v) => {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::SetVolume(v * 100.0)).ok();
                        }
                    }
                }
            }

            // ── MPRIS: push current state ───────────────────────────────
            {
                let (can_next, can_prev) = {
                    let s = state_c.borrow();
                    let n = s.playlist.len();
                    let i = s.current_idx.unwrap_or(0);
                    (i + 1 < n, i > 0 && n > 0)
                };
                let playback_status = if idle {
                    "Stopped"
                } else if paused {
                    "Paused"
                } else {
                    "Playing"
                };
                let loop_status = match repeat_mode {
                    RepeatMode::None => "None",
                    RepeatMode::One => "Track",
                    RepeatMode::Playlist => "Playlist",
                };
                mpris_c.update(MprisState {
                    playback_status: playback_status.into(),
                    loop_status: loop_status.into(),
                    title: title.as_deref().unwrap_or("").into(),
                    artist: artist.clone(),
                    album: album.clone(),
                    art_url: if thumbnail.is_empty() { fallback_icon_uri.clone() } else { thumbnail.clone() },
                    position_us: (pos * 1_000_000.0) as i64,
                    duration_us: (dur * 1_000_000.0) as i64,
                    volume: volume / 100.0,
                    can_go_next: can_next,
                    can_go_previous: can_prev,
                });
            }

            // ── Perf: warn if this tick took too long (blocks GTK main loop) ──
            let tick_ms = _tick_start.elapsed().as_millis();
            if tick_ms > 50 {
                log::warn!("[perf] 200ms tick took {}ms — main thread stall!", tick_ms);
            }

            // ── Periodic stats dump every ~30 s (150 ticks) ─────────────
            if tick % 150 == 1 {
                let playlist_len = state_c.borrow().playlist.len();
                let mem_kb = std::fs::read_to_string("/proc/self/status")
                    .ok()
                    .and_then(|s| {
                        s.lines()
                            .find(|l| l.starts_with("VmRSS:"))
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|v| v.parse::<u64>().ok())
                    })
                    .unwrap_or(0);
                log::info!(
                    "[stats] playlist={} items | idle={} | buffering={} | mem={}MB",
                    playlist_len, idle, buffering,
                    mem_kb / 1024
                );
            }

            glib::ControlFlow::Continue
        });

        Self { window, state }
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn open_file(&self, path: &Path) {
        log::info!("Opening from CLI: {:?}", path);
        // Cancel any pending session seek — it belongs to the restored file, not this one.
        self.state.borrow_mut().pending_seek = None;
        if let Some(p) = self.state.borrow().player.as_ref() {
            p.execute(PlayerCommand::Open(path.to_path_buf())).ok();
        }
    }
}

/// Extracts a YouTube thumbnail `https://` URL from a YouTube video URL.
/// Supports `youtube.com/watch?v=ID`, `youtu.be/ID`, and `youtube.com/shorts/ID`.
/// Returns `None` for non-YouTube URLs.
fn youtube_thumbnail_url(url: &str) -> Option<String> {
    let extract_id = |start: &str| -> Option<String> {
        let id: String = start
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if id.is_empty() { None } else { Some(id) }
    };

    // youtube.com/watch?v=ID or youtube.com/watch?…&v=ID
    if url.contains("youtube.com/watch") {
        if let Some(pos) = url.find("v=") {
            if let Some(id) = extract_id(&url[pos + 2..]) {
                return Some(format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", id));
            }
        }
    }
    // youtu.be/ID
    if let Some(pos) = url.find("youtu.be/") {
        if let Some(id) = extract_id(&url[pos + 9..]) {
            return Some(format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", id));
        }
    }
    // youtube.com/shorts/ID
    if let Some(pos) = url.find("youtube.com/shorts/") {
        if let Some(id) = extract_id(&url[pos + 19..]) {
            return Some(format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", id));
        }
    }

    None
}

/// Returns a `file://` URI for Aurora's own icon, used as MPRIS artUrl fallback
/// when no media-specific thumbnail is available. Prefers PNG over SVG because
/// some MPRIS clients (e.g. GNOME Shell media controls) don't render SVG artUrls.
fn aurora_icon_uri() -> String {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    if let Ok(snap) = std::env::var("SNAP") {
        let snap = std::path::PathBuf::from(snap);
        // PNG from snap build (128×128, guaranteed to render in any MPRIS client)
        candidates.push(snap.join("meta/gui/icon.png"));
        // SVG fallback within snap
        candidates.push(snap.join("share/icons/hicolor/scalable/apps")
            .join("io.github.daniacosta_dev.AuroraMediaPlayer.svg"));
    }

    // XDG_DATA_HOME (~/.local/share) — dev mode copies icon here at startup
    if let Some(data_home) = dirs::data_dir() {
        candidates.push(data_home.join("icons/hicolor/scalable/apps")
            .join("io.github.daniacosta_dev.AuroraMediaPlayer.svg"));
    }

    // XDG_DATA_DIRS (system install, Flatpak, etc.)
    let xdg_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in xdg_dirs.split(':') {
        candidates.push(std::path::PathBuf::from(dir)
            .join("icons/hicolor/scalable/apps")
            .join("io.github.daniacosta_dev.AuroraMediaPlayer.svg"));
    }

    for p in candidates {
        if p.exists() {
            return format!("file://{}", p.display());
        }
    }

    String::new()
}
