use std::rc::Rc;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use gtk4::{self as gtk, Box, Orientation, Button, Scale, Label, Adjustment, Popover, DrawingArea, Overlay};
#[allow(unused_imports)]
use std::path::PathBuf;
use gtk4::prelude::*;
use glib;
use gio;

use crate::i18n::t;
use crate::state::SharedState;
use crate::player::PlayerCommand;
use crate::player::RepeatMode;
use crate::thumbnails::SharedCache;

pub struct PlayerControls {
    root: Box,
    prev_btn: Button,
    play_btn: Button,
    next_btn: Button,
    repeat_btn: Button,
    shuffle_btn: Button,
    vol_btn: Button,
    seek_bar: Scale,
    vol_slider: Scale,
    elapsed: Label,
    remaining: Label,
    /// Blocked during programmatic set_value() to avoid feedback loops.
    seek_handler: glib::SignalHandlerId,
    vol_handler: glib::SignalHandlerId,
    screenshot_btn: Button,
    fullscreen_btn: Button,
    speed_btn: Button,
    tracks_btn: Button,
    podcast_btn: Button,
    tracks_popover: Popover,
    speed_popover: Popover,
    last_tracks: Rc<RefCell<Vec<crate::player::TrackInfo>>>,
    chapter_overlay: DrawingArea,
    chapter_data: Rc<RefCell<(f64, Vec<(String, f64)>)>>,
    seek_outer: Overlay,
    hover_label: Label,
    vol_label: Label,
    /// Picture widget added as overlay on video_controls (floats above the controls bar).
    thumb_picture: gtk::Picture,
    /// Shared cache written by the background ffmpeg thread.
    thumb_cache: Rc<RefCell<SharedCache>>,
    /// Reference to the video overlay for coordinate translation.
    /// Set after construction via `set_video_overlay()`.
    video_overlay_target: Rc<RefCell<Option<gtk::Overlay>>>,
    /// Tracks the current layout mode so the thumbnail motion handler can
    /// compute the correct margin_bottom (fixed: controls are below the video,
    /// so thumbnail should pin to the bottom edge of the video overlay).
    is_fixed: Rc<Cell<bool>>,
    /// True when playback reached EOF with no next track and no repeat active.
    /// The play button shows a "replay" icon and clicking it restarts playback.
    ended: Rc<Cell<bool>>,
}

impl PlayerControls {
    pub fn new(state: SharedState, on_screenshot: impl Fn(std::path::PathBuf) + 'static) -> Self {
        let root = Box::builder()
            .orientation(Orientation::Vertical)
            .css_classes(vec!["toolbar", "controls-bar"])
            .build();

        // ── Seek bar ──────────────────────────────────────────────────────
        let seek_adj = Adjustment::new(0.0, 0.0, 1.0, 0.001, 0.01, 0.0);
        let seek_bar = Scale::builder()
            .adjustment(&seek_adj)
            .draw_value(false)
            .hexpand(true)
            .build();
        seek_bar.add_css_class("seekbar");

        // ── Chapter overlay ───────────────────────────────────────────────
        let chapter_data: Rc<RefCell<(f64, Vec<(String, f64)>)>> =
            Rc::new(RefCell::new((0.0, Vec::new())));

        let chapter_overlay = DrawingArea::builder()
            .can_target(false)
            .hexpand(true)
            .build();
        chapter_overlay.set_valign(gtk::Align::Fill);
        chapter_overlay.set_halign(gtk::Align::Fill);

        {
            let data_c = chapter_data.clone();
            chapter_overlay.set_draw_func(move |widget, cr, _w, _h| {
                let (dur, chapters) = &*data_c.borrow();
                if *dur <= 0.0 || chapters.is_empty() { return; }
                let w = widget.width() as f64;
                let h = widget.height() as f64;
                if w <= 0.0 { return; }
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.5);
                for (_, time) in chapters {
                    let x = (time / dur) * w;
                    cr.rectangle(x - 1.0, h - 6.0, 2.0, 6.0);
                }
                cr.fill().ok();
            });
        }

        let seek_outer = Overlay::new();
        seek_outer.set_child(Some(&seek_bar));
        seek_outer.add_overlay(&chapter_overlay);

        // ── Thumbnail preview ─────────────────────────────────────────────
        // Added as an overlay on video_controls (window.rs) so it floats
        // above the controls bar without affecting its height.
        let thumb_picture = gtk::Picture::builder()
            .width_request(160)
            .height_request(90)
            .can_shrink(false)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .opacity(0.0)
            .can_target(false)
            .css_classes(["seek-thumb-picture"])
            .build();

        let thumb_cache: Rc<RefCell<SharedCache>> =
            Rc::new(RefCell::new(Arc::new(Mutex::new(None))));

        let video_overlay_target: Rc<RefCell<Option<gtk::Overlay>>> =
            Rc::new(RefCell::new(None));

        let is_fixed: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let ended: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // ── Hover time label ──────────────────────────────────────────────
        // Sits in the root Box ABOVE the seek bar so it never overlaps the
        // trough. opacity=0/1 is used instead of visible so layout is stable.
        let hover_label = Label::builder()
            .css_classes(["seek-hover-label"])
            .halign(gtk::Align::Start)
            .opacity(0.0)
            .build();

        {
            let data_c   = chapter_data.clone();
            let lbl_w    = hover_label.downgrade();
            let sb_w     = seek_bar.downgrade();
            let root_ref = root.downgrade();
            let pic_w    = thumb_picture.downgrade();
            let cache_c  = thumb_cache.clone();
            let vot_c    = video_overlay_target.clone();
            let is_fixed_c = is_fixed.clone();
            // Cache the last frame path to skip redundant set_file calls.
            let last_path: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
            let mc = gtk::EventControllerMotion::new();

            mc.connect_enter({
                let lbl = hover_label.downgrade();
                move |_, _, _| { if let Some(l) = lbl.upgrade() { l.set_opacity(1.0); } }
            });
            mc.connect_leave({
                let lbl = hover_label.downgrade();
                let pic = thumb_picture.downgrade();
                move |_| {
                    if let Some(l) = lbl.upgrade() { l.set_opacity(0.0); }
                    if let Some(p) = pic.upgrade() { p.set_opacity(0.0); }
                }
            });
            mc.connect_motion(move |_, x, _| {
                let (Some(l), Some(sb)) = (lbl_w.upgrade(), sb_w.upgrade()) else { return };
                let w = sb.width() as f64;
                if w <= 0.0 { return; }
                let (dur, chapters) = &*data_c.borrow();
                if *dur <= 0.0 { return; }

                // GTK Scale maps the full widget width to [min, max] linearly.
                let frac = (x / w).clamp(0.0, 1.0);
                let pos_secs = frac * dur;
                let time_str = format_time(pos_secs as u64);

                let near_mark = chapters.iter()
                    .find(|(_, t)| ((t / dur) * w - x).abs() < 8.0)
                    .map(|(n, _)| n.as_str());

                l.set_label(&match near_mark {
                    Some(name) => format!("{name}\n{time_str}"),
                    None       => time_str,
                });

                // Translate cursor x to video_controls space once; reuse for
                // both the label and the thumbnail so they share the same
                // physical center point over the cursor.
                let (vx, _lvy_label) = if let Some(ref vc) = *vot_c.borrow() {
                    sb.translate_coordinates(vc, x, 0.0).unwrap_or((x, 0.0))
                } else {
                    (x, 0.0)
                };

                // ── Hover label: center on cursor in root-Box space ──────────
                if let Some(root) = root_ref.upgrade() {
                    let (rx, _) = sb.translate_coordinates(&root, x, 0.0)
                        .unwrap_or((x, 0.0));
                    let root_w  = root.width() as f64;
                    let lbl_half = (l.width() as f64 / 2.0).max(24.0);
                    let margin = (rx - lbl_half).max(0.0).min(root_w - lbl_half * 2.0) as i32;
                    l.set_margin_start(margin);
                }

                // ── Thumbnail preview ────────────────────────────────────────
                if let Some(pic) = pic_w.upgrade() {
                    let cache_guard = cache_c.borrow();
                    let has_thumb = if let Ok(lock) = cache_guard.lock() {
                        if let Some(ref tc) = *lock {
                            if let Some(path) = tc.frame_at(frac) {
                                // Only reload the file when the frame changes.
                                let changed = last_path.borrow().as_deref() != Some(path);
                                if changed {
                                    pic.set_file(Some(&gio::File::for_path(path)));
                                    *last_path.borrow_mut() = Some(path.to_owned());
                                }
                                if let Some(ref vc) = *vot_c.borrow() {
                                    // Center thumbnail on cursor in vc space.
                                    let ms = (vx - 80.0).max(0.0)
                                        .min((vc.width() as f64 - 160.0).max(0.0))
                                        as i32;
                                    // In fixed mode the controls bar sits below the video
                                    // overlay, so pin the thumbnail to the bottom edge of
                                    // the video. In floating mode anchor above the hover
                                    // label (which is overlaid on the video area).
                                    let mb = if is_fixed_c.get() {
                                        4
                                    } else {
                                        let (_, lvy) = l.translate_coordinates(vc, 0.0, 0.0)
                                            .unwrap_or((0.0, 0.0));
                                        (vc.height() as f64 - lvy + 4.0).max(0.0) as i32
                                    };
                                    pic.set_margin_start(ms);
                                    pic.set_margin_bottom(mb);
                                }
                                pic.set_opacity(1.0);
                                true
                            } else { false }
                        } else { false }
                    } else { false };

                    if !has_thumb { pic.set_opacity(0.0); }
                }
            });
            seek_bar.add_controller(mc);
        }

        // ── Time labels ───────────────────────────────────────────────────
        let elapsed = Label::builder()
            .label("0:00")
            .css_classes(vec!["caption"])
            .build();
        let remaining = Label::builder()
            .label("-0:00")
            .css_classes(vec!["caption"])
            .build();

        // ── Playback buttons ──────────────────────────────────────────────
        let prev_btn = Button::builder()
            .icon_name("media-skip-backward-symbolic")
            .build();
        let play_btn = Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(vec!["circular", "suggested-action"])
            .build();
        let next_btn = Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .build();

        // ── Repeat ────────────────────────────────────────────────────────
        let repeat_btn = Button::builder()
            .icon_name("media-playlist-repeat-symbolic")
            .css_classes(vec!["flat", "repeat-btn"])
            .build();

        // ── Shuffle ───────────────────────────────────────────────────────
        let shuffle_btn = Button::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .css_classes(vec!["flat", "shuffle-btn"])
            .tooltip_text(t("Shuffle"))
            .build();
        
        // ── Volume ────────────────────────────────────────────────────────
        let vol_btn = Button::builder()
            .icon_name("audio-volume-high-symbolic")
            .build();
        let vol_adj = Adjustment::new(100.0, 0.0, 100.0, 1.0, 10.0, 0.0);
        let vol_slider = Scale::builder()
            .adjustment(&vol_adj)
            .draw_value(false)
            .width_request(90)
            .build();

        let vol_label = Label::builder()
            .label("100%")
            .css_classes(vec!["caption", "vol-pct"])
            .width_chars(4)
            .xalign(1.0)
            .build();

        // ── Podcast mode button ───────────────────────────────────────────
        let podcast_btn = Button::builder()
            .icon_name("audio-headphones-symbolic")
            .tooltip_text(t("Podcast mode — audio only (saves bandwidth)"))
            .css_classes(vec!["flat"])
            .build();

        // ── Screenshot button ─────────────────────────────────────────────
        let screenshot_btn = Button::builder()
            .icon_name("camera-photo-symbolic")
            .tooltip_text(t("Take screenshot"))
            .css_classes(vec!["flat"])
            .build();

        // ── Fullscreen button ─────────────────────────────────────────────
        let fullscreen_btn = Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text(t("Fullscreen"))
            .css_classes(vec!["flat"])
            .build();

        // ── Tracks button + popover ───────────────────────────────────────
        let tracks_btn = Button::builder()
            .icon_name("media-optical-symbolic")
            .tooltip_text(t("Audio & Subtitle tracks"))
            .css_classes(vec!["flat"])
            .sensitive(false)
            .build();
        tracks_btn.set_opacity(0.5);
        let tracks_popover = Popover::new();
        tracks_popover.set_position(gtk4::PositionType::Top);
        tracks_popover.set_parent(&tracks_btn);
        {
            let tp = tracks_popover.clone();
            tracks_btn.connect_clicked(move |_| { tp.popup(); });
        }

        // ── Speed button + popover ────────────────────────────────────────
        let speed_btn = Button::builder()
            .label("1×")
            .tooltip_text(t("Playback speed"))
            .css_classes(vec!["flat"])
            .build();
        let speed_popover = Popover::new();
        speed_popover.set_position(gtk4::PositionType::Top);
        {
            let speed_box = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(2)
                .margin_top(4)
                .margin_bottom(4)
                .margin_start(4)
                .margin_end(4)
                .build();
            for (lbl, val) in [
                ("0.25×", 0.25f64), ("0.5×", 0.5), ("0.75×", 0.75),
                ("1×", 1.0), ("1.25×", 1.25), ("1.5×", 1.5), ("2×", 2.0),
            ] {
                let btn = Button::builder()
                    .label(lbl)
                    .css_classes(vec!["flat"])
                    .build();
                let state_s = state.clone();
                let popover_s = speed_popover.clone();
                btn.connect_clicked(move |_| {
                    if let Some(p) = state_s.borrow().player.as_ref() {
                        p.execute(PlayerCommand::SetSpeed(val)).ok();
                    }
                    popover_s.popdown();
                });
                speed_box.append(&btn);
            }
            speed_popover.set_child(Some(&speed_box));
            speed_popover.set_parent(&speed_btn);
        }
        {
            let sp_c = speed_popover.clone();
            speed_btn.connect_clicked(move |_| { sp_c.popup(); });
        }

        // Pointer cursor on all interactive controls
        for w in [
            prev_btn.upcast_ref::<gtk::Widget>(),
            play_btn.upcast_ref(),
            next_btn.upcast_ref(),
            repeat_btn.upcast_ref(),
            shuffle_btn.upcast_ref(),
            podcast_btn.upcast_ref(),
            vol_btn.upcast_ref(),
            screenshot_btn.upcast_ref(),
            fullscreen_btn.upcast_ref(),
            speed_btn.upcast_ref(),
            tracks_btn.upcast_ref(),
            seek_bar.upcast_ref(),
            vol_slider.upcast_ref(),
        ] {
            w.set_cursor_from_name(Some("pointer"));
        }

        // ── Signal: podcast mode ─────────────────────────────────────────
        {
            let state_c = state.clone();
            podcast_btn.connect_clicked(move |_| {
                let podcast = {
                    let mut s = state_c.borrow_mut();
                    s.podcast_mode = !s.podcast_mode;
                    s.podcast_mode
                };
                if let Some(p) = state_c.borrow().player.as_ref() {
                    p.execute(PlayerCommand::SetVideoEnabled(!podcast)).ok();
                }
            });
        }

        // ── Signal: screenshot ────────────────────────────────────────────
        {
            let state_c = state.clone();
            screenshot_btn.connect_clicked(move |_| {
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
                    on_screenshot(tmp_path);
                }
            });
        }

        // ── Signal: play/pause ────────────────────────────────────────────
        {
            let state_c = state.clone();
            let ended_c = ended.clone();
            play_btn.connect_clicked(move |_| {
                if ended_c.get() {
                    // Playback ended with no next track — restart from beginning.
                    let (path_to_open, new_idx) = {
                        let s = state_c.borrow();
                        if s.playlist.len() <= 1 {
                            // Single file: replay the same file.
                            (s.current_idx.and_then(|i| s.playlist.get(i).cloned()), s.current_idx)
                        } else {
                            // Playlist ended: restart from index 0.
                            (s.playlist.first().cloned(), Some(0))
                        }
                    };
                    if let Some(path) = path_to_open {
                        state_c.borrow_mut().current_idx = new_idx;
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::Open(path)).ok();
                        }
                    }
                } else if let Some(p) = state_c.borrow().player.as_ref() {
                    p.execute(PlayerCommand::TogglePause).ok();
                }
            });
        }

        // ── Signal: prev / next ───────────────────────────────────────────
        {
            let state_c = state.clone();
            prev_btn.connect_clicked(move |_| {
                let mut s = state_c.borrow_mut();
                let new_idx = s.current_idx.and_then(|i| i.checked_sub(1));
                if let Some(idx) = new_idx {
                    if let Some(path) = s.playlist.get(idx).cloned() {
                        s.current_idx = Some(idx);
                        drop(s);
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::Open(path)).ok();
                        }
                    }
                }
            });
        }
        {
            let state_c = state.clone();
            next_btn.connect_clicked(move |_| {
                let mut s = state_c.borrow_mut();
                let new_idx = s.current_idx.map(|i| i + 1).filter(|&i| i < s.playlist.len());
                if let Some(idx) = new_idx {
                    if let Some(path) = s.playlist.get(idx).cloned() {
                        s.current_idx = Some(idx);
                        drop(s);
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(PlayerCommand::Open(path)).ok();
                        }
                    }
                }
            });
        }

        // ── Signal: repeat ───────────────────────────────────────────────
        {
            let state_c = state.clone();
            repeat_btn.connect_clicked(move |_| {
                let next_mode = {
                    let mut s = state_c.borrow_mut();
                    s.repeat_mode = match s.repeat_mode {
                        RepeatMode::None     => RepeatMode::Playlist,
                        RepeatMode::Playlist => RepeatMode::One,
                        RepeatMode::One      => RepeatMode::None,
                    };
                    s.repeat_mode
                };
                if let Some(p) = state_c.borrow().player.as_ref() {
                    p.execute(PlayerCommand::SetRepeat(next_mode)).ok();
                }
            });
        }

        // ── Signal: shuffle ───────────────────────────────────────────────
        {
            let state_c = state.clone();
            shuffle_btn.connect_clicked(move |_| {
                let mut s = state_c.borrow_mut();
                s.shuffle = !s.shuffle;
                if s.shuffle {
                    s.rebuild_shuffle_order();
                }
            });
        }

        // ── Signal: seek bar ──────────────────────────────────────────────
        let seek_handler = {
            let state_c = state.clone();
            seek_bar.connect_value_changed(move |scale| {
                let s = state_c.borrow();
                if let Some(p) = s.player.as_ref() {
                    if let Some(dur) = p.duration() {
                        p.execute(PlayerCommand::Seek(scale.value() * dur)).ok();
                    }
                }
            })
        };

        // ── Signal: volume slider ─────────────────────────────────────────
        let vol_handler = {
            let state_c = state.clone();
            vol_slider.connect_value_changed(move |scale| {
                if let Some(p) = state_c.borrow().player.as_ref() {
                    p.execute(PlayerCommand::SetVolume(scale.value())).ok();
                }
            })
        };

        // ── Signal: mute ─────────────────────────────────────────────────
        {
            let state_c = state.clone();
            vol_btn.connect_clicked(move |_| {
                let mut s = state_c.borrow_mut();
                s.muted = !s.muted;
                let muted = s.muted;
                if let Some(p) = s.player.as_ref() {
                    p.execute(PlayerCommand::Mute(muted)).ok();
                }
            });
        }

        let this = Self {
            root,
            prev_btn,
            play_btn,
            next_btn,
            repeat_btn,
            shuffle_btn,
            vol_btn,
            seek_bar,
            vol_slider,
            elapsed,
            remaining,
            seek_handler,
            vol_handler,
            screenshot_btn,
            fullscreen_btn,
            speed_btn,
            tracks_btn,
            podcast_btn,
            tracks_popover,
            speed_popover,
            last_tracks: Rc::new(RefCell::new(Vec::new())),
            chapter_overlay,
            chapter_data: chapter_data.clone(),
            seek_outer,
            hover_label,
            vol_label,
            thumb_picture,
            thumb_cache,
            video_overlay_target,
            is_fixed,
            ended,
        };
        this.apply_layout("floating");
        this
    }

    pub fn apply_layout(&self, mode: &str) {
        self.is_fixed.set(mode == "fixed");

        // Popovers are outside the controls widget's CSS tree so they need their
        // own class to pick up modern styling.
        for pop in [&self.speed_popover, &self.tracks_popover] {
            if mode == "modern" {
                pop.add_css_class("popover-modern");
            } else {
                pop.remove_css_class("popover-modern");
            }
        }

        while let Some(child) = self.root.first_child() {
            self.root.remove(&child);
        }
        if mode == "fixed" {
            let seek_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_start(12)
                .margin_end(12)
                .margin_top(6)
                .margin_bottom(2)
                .build();
            self.seek_outer.set_hexpand(true);
            self.elapsed.set_valign(gtk::Align::Center);
            self.remaining.set_valign(gtk::Align::Center);
            seek_row.append(&self.elapsed);
            seek_row.append(&self.seek_outer);
            seek_row.append(&self.remaining);

            let btn_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .margin_start(8)
                .margin_end(8)
                .margin_bottom(6)
                .build();
            btn_row.append(&self.prev_btn);
            btn_row.append(&self.play_btn);
            btn_row.append(&self.next_btn);
            let sp1 = gtk::Box::builder().hexpand(true).build();
            btn_row.append(&sp1);
            btn_row.append(&self.repeat_btn);
            btn_row.append(&self.shuffle_btn);
            btn_row.append(&self.screenshot_btn);
            btn_row.append(&self.podcast_btn);
            let sp2 = gtk::Box::builder().hexpand(true).build();
            btn_row.append(&sp2);
            btn_row.append(&self.tracks_btn);
            btn_row.append(&self.speed_btn);
            btn_row.append(&self.vol_btn);
            btn_row.append(&self.vol_slider);
            btn_row.append(&self.vol_label);
            btn_row.append(&self.fullscreen_btn);

            self.root.append(&self.hover_label);
            self.root.append(&seek_row);
            self.root.append(&btn_row);
        } else if mode == "modern" {
            // Modern: gradient overlay, seek bar flanked by times, single button row below
            self.seek_outer.set_hexpand(true);
            self.elapsed.set_valign(gtk::Align::Center);
            self.remaining.set_valign(gtk::Align::Center);

            let seek_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_start(12)
                .margin_end(12)
                .margin_top(4)
                .margin_bottom(2)
                .build();
            seek_row.append(&self.elapsed);
            seek_row.append(&self.seek_outer);
            seek_row.append(&self.remaining);

            let btn_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .margin_start(8)
                .margin_end(8)
                .margin_bottom(4)
                .build();
            btn_row.append(&self.prev_btn);
            btn_row.append(&self.play_btn);
            btn_row.append(&self.next_btn);
            let sp = gtk::Box::builder().hexpand(true).build();
            btn_row.append(&sp);
            btn_row.append(&self.repeat_btn);
            btn_row.append(&self.shuffle_btn);
            btn_row.append(&self.podcast_btn);
            btn_row.append(&self.screenshot_btn);
            btn_row.append(&self.tracks_btn);
            btn_row.append(&self.speed_btn);
            btn_row.append(&self.vol_btn);
            btn_row.append(&self.vol_slider);
            btn_row.append(&self.vol_label);
            btn_row.append(&self.fullscreen_btn);

            self.root.append(&self.hover_label);
            self.root.append(&seek_row);
            self.root.append(&btn_row);
        } else {
            let time_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .margin_start(12)
                .margin_end(12)
                .build();
            let time_spacer = gtk::Box::builder().hexpand(true).build();
            self.elapsed.set_valign(gtk::Align::Baseline);
            self.remaining.set_valign(gtk::Align::Baseline);
            time_row.append(&self.elapsed);
            time_row.append(&time_spacer);
            time_row.append(&self.remaining);

            let left_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .halign(gtk::Align::Start)
                .build();
            left_box.append(&self.repeat_btn);
            left_box.append(&self.shuffle_btn);
            left_box.append(&self.podcast_btn);
            left_box.append(&self.screenshot_btn);

            let center_btns = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .halign(gtk::Align::Center)
                .build();
            center_btns.append(&self.prev_btn);
            center_btns.append(&self.play_btn);
            center_btns.append(&self.next_btn);

            let vol_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .halign(gtk::Align::End)
                .build();
            vol_box.append(&self.tracks_btn);
            vol_box.append(&self.speed_btn);
            vol_box.append(&self.vol_btn);
            vol_box.append(&self.vol_slider);
            vol_box.append(&self.vol_label);
            vol_box.append(&self.fullscreen_btn);

            // CenterBox guarantees the play controls are always centred
            // regardless of the widths of the left and right groups.
            let end_row = gtk::CenterBox::builder()
                .margin_start(12)
                .margin_end(12)
                .margin_bottom(4)
                .build();
            end_row.set_start_widget(Some(&left_box));
            end_row.set_center_widget(Some(&center_btns));
            end_row.set_end_widget(Some(&vol_box));

            self.root.append(&self.hover_label);
            self.root.append(&self.seek_outer);
            self.root.append(&time_row);
            self.root.append(&end_row);
        }
    }

    pub fn widget(&self) -> &Box {
        &self.root
    }

    pub fn fullscreen_btn(&self) -> &Button {
        &self.fullscreen_btn
    }

    /// Replace the active thumbnail cache (called from window.rs when a new
    /// local file is loaded or when the player goes idle).
    pub fn set_thumb_cache(&self, cache: SharedCache) {
        self.thumb_picture.set_opacity(0.0);
        *self.thumb_cache.borrow_mut() = cache;
    }

    /// Returns the thumbnail picture widget to be added as an overlay on
    /// video_controls in window.rs.
    pub fn thumb_widget(&self) -> &gtk::Picture {
        &self.thumb_picture
    }

    /// Tell the motion handler which overlay to use for coordinate translation.
    /// Must be called after video_controls is created (window.rs).
    pub fn set_video_overlay(&self, overlay: &gtk::Overlay) {
        *self.video_overlay_target.borrow_mut() = Some(overlay.clone());
    }

    pub fn relabel(&self) {
        self.podcast_btn.set_tooltip_text(Some(t("Podcast mode — audio only (saves bandwidth)")));
        self.screenshot_btn.set_tooltip_text(Some(t("Take screenshot")));
        self.tracks_btn.set_tooltip_text(Some(t("Audio & Subtitle tracks")));
        self.speed_btn.set_tooltip_text(Some(t("Playback speed")));
        self.shuffle_btn.set_tooltip_text(Some(t("Shuffle")));
        // Force tracks popover to rebuild on next update_tracks() call.
        self.last_tracks.borrow_mut().clear();
    }

    /// Returns true if any popover (speed, tracks) is currently open.
    /// Used by the auto-hide timer to prevent hiding the control bar while
    /// the user is navigating a dropdown.
    pub fn has_open_popover(&self) -> bool {
        self.speed_popover.is_visible() || self.tracks_popover.is_visible()
    }

    /// Called at ~50 ms — only updates the seek bar and time labels.
    pub fn update_position(&self, pos: f64, dur: f64) {
        self.seek_bar.block_signal(&self.seek_handler);
        if dur > 0.0 {
            self.seek_bar.set_value(pos / dur);
        }
        self.seek_bar.unblock_signal(&self.seek_handler);

        self.elapsed.set_label(&format_time(pos as u64));
        if dur > 0.0 {
            self.remaining
                .set_label(&format!("-{}", format_time((dur - pos) as u64)));
        }
    }

    /// Called at ~200 ms — updates buttons and state-driven UI.
    pub fn update(&self, pos: f64, dur: f64, paused: bool, muted: bool, volume: f64, speed: f64, idle: bool, has_video: bool, repeat: RepeatMode, shuffle: bool, podcast_mode: bool, has_prev: bool, has_next: bool, eof: bool) {
        let has_media = !idle;
        // "ended" = reached EOF, no repeat active, no next track to auto-advance to.
        let is_ended = eof && !idle && repeat == RepeatMode::None && !has_next;
        self.ended.set(is_ended);

        self.play_btn.set_sensitive(has_media);
        self.prev_btn.set_sensitive(has_prev);
        self.next_btn.set_sensitive(has_next);
        self.seek_bar.set_sensitive(has_media);
        self.screenshot_btn.set_visible(has_media && has_video);

        self.update_position(pos, dur);

        self.play_btn.set_icon_name(if is_ended {
            "media-playlist-repeat-symbolic"
        } else if paused {
            "media-playback-start-symbolic"
        } else {
            "media-playback-pause-symbolic"
        });

        self.vol_slider.block_signal(&self.vol_handler);
        self.vol_slider.set_value(volume);
        self.vol_slider.unblock_signal(&self.vol_handler);

        self.vol_btn.set_icon_name(if muted || volume == 0.0 {
            "audio-volume-muted-symbolic"
        } else if volume < 33.0 {
            "audio-volume-low-symbolic"
        } else if volume < 66.0 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        self.vol_label.set_label(&format!("{}%", volume as u32));

        // Repeat button: icon + opacity reflect current mode.
        match repeat {
            RepeatMode::None => {
                self.repeat_btn.set_icon_name("media-playlist-repeat-symbolic");
                self.repeat_btn.remove_css_class("toggle-active");
            }
            RepeatMode::Playlist => {
                self.repeat_btn.set_icon_name("media-playlist-repeat-symbolic");
                self.repeat_btn.add_css_class("toggle-active");
            }
            RepeatMode::One => {
                self.repeat_btn.set_icon_name("media-playlist-repeat-song-symbolic");
                self.repeat_btn.add_css_class("toggle-active");
            }
        }
        self.speed_btn.set_label(&format!("{}×", speed));
        if shuffle {
            self.shuffle_btn.add_css_class("toggle-active");
        } else {
            self.shuffle_btn.remove_css_class("toggle-active");
        }


        // Podcast button: only relevant for video streams (saves bandwidth).
        // Show when there's video, or while podcast mode is on (video disabled but still a stream).
        let show_podcast = has_media && (has_video || podcast_mode);
        self.podcast_btn.set_visible(show_podcast);
        if podcast_mode {
            self.podcast_btn.add_css_class("toggle-active");
        } else {
            self.podcast_btn.remove_css_class("toggle-active");
        }
    }

    pub fn update_tracks(&self, tracks: Vec<crate::player::TrackInfo>, state: &SharedState) {
        let audio_count = tracks.iter().filter(|t| t.kind == "audio").count();
        let sub_count   = tracks.iter().filter(|t| t.kind == "sub").count();
        // Enable only when there's something to choose: multiple audio tracks or any subtitle.
        let has_tracks  = audio_count > 1 || sub_count > 0;
        self.tracks_btn.set_sensitive(has_tracks);
        self.tracks_btn.set_opacity(if has_tracks { 1.0 } else { 0.5 });

        {
            let last = self.last_tracks.borrow();
            let unchanged = last.len() == tracks.len()
                && last.iter().zip(&tracks).all(|(a, b)| {
                    a.id == b.id && a.kind == b.kind && a.selected == b.selected
                });
            if unchanged { return; }
        }
        *self.last_tracks.borrow_mut() = tracks.clone();

        // Rebuild popover content
        let popover_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();

        let audio_tracks: Vec<_> = tracks.iter().filter(|t| t.kind == "audio").collect();
        let sub_tracks: Vec<_> = tracks.iter().filter(|t| t.kind == "sub").collect();

        if !audio_tracks.is_empty() {
            let lbl = gtk::Label::builder()
                .label(t("Audio"))
                .halign(gtk::Align::Start)
                .css_classes(vec!["heading"])
                .build();
            popover_box.append(&lbl);

            let first_check: Rc<RefCell<Option<gtk::CheckButton>>> = Rc::new(RefCell::new(None));
            for t in &audio_tracks {
                let label = track_label(t);
                let check = gtk::CheckButton::builder()
                    .label(&label)
                    .active(t.selected)
                    .build();
                {
                    let mut fc = first_check.borrow_mut();
                    if let Some(ref first) = *fc {
                        check.set_group(Some(first));
                    } else {
                        *fc = Some(check.clone());
                    }
                }
                let id = t.id;
                let state_c = state.clone();
                let popover_c = self.tracks_popover.clone();
                check.connect_toggled(move |btn| {
                    if btn.is_active() {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(crate::player::PlayerCommand::SetAudioTrack(id)).ok();
                        }
                        popover_c.popdown();
                    }
                });
                popover_box.append(&check);
            }
        }

        if !sub_tracks.is_empty() {
            let lbl = gtk::Label::builder()
                .label(t("Subtitles"))
                .halign(gtk::Align::Start)
                .css_classes(vec!["heading"])
                .margin_top(if audio_tracks.is_empty() { 0 } else { 8 })
                .build();
            popover_box.append(&lbl);

            let first_check: Rc<RefCell<Option<gtk::CheckButton>>> = Rc::new(RefCell::new(None));

            // "Disable" option
            let none_check = gtk::CheckButton::builder()
                .label(t("Disabled"))
                .active(sub_tracks.iter().all(|t| !t.selected))
                .build();
            {
                let mut fc = first_check.borrow_mut();
                *fc = Some(none_check.clone());
            }
            {
                let state_c = state.clone();
                let popover_c = self.tracks_popover.clone();
                none_check.connect_toggled(move |btn| {
                    if btn.is_active() {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(crate::player::PlayerCommand::SetSubtitleTrack(0)).ok();
                        }
                        popover_c.popdown();
                    }
                });
            }
            popover_box.append(&none_check);

            for t in &sub_tracks {
                let label = track_label(t);
                let check = gtk::CheckButton::builder()
                    .label(&label)
                    .active(t.selected)
                    .build();
                {
                    let fc = first_check.borrow();
                    if let Some(ref first) = *fc {
                        check.set_group(Some(first));
                    }
                }
                let id = t.id;
                let state_c = state.clone();
                let popover_c = self.tracks_popover.clone();
                check.connect_toggled(move |btn| {
                    if btn.is_active() {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(crate::player::PlayerCommand::SetSubtitleTrack(id)).ok();
                        }
                        popover_c.popdown();
                    }
                });
                popover_box.append(&check);
            }
        }

        // Visibility: show button only when there are non-video tracks

        self.tracks_popover.set_child(Some(&popover_box));
    }

    pub fn update_chapters(&self, chapters: Vec<(String, f64)>, dur: f64) {
        let mut data = self.chapter_data.borrow_mut();
        if data.0 != dur || data.1.len() != chapters.len() {
            *data = (dur, chapters);
            drop(data);
            self.chapter_overlay.queue_draw();
        }
    }
}

fn track_label(t: &crate::player::TrackInfo) -> String {
    let base = t.title.as_deref()
        .or(t.lang.as_deref())
        .unwrap_or("Unknown");
    if let Some(ref lang) = t.lang {
        if t.title.is_some() {
            format!("{} ({})", t.title.as_deref().unwrap_or(""), lang)
        } else {
            lang.clone()
        }
    } else {
        base.to_string()
    }
}

fn format_time(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
