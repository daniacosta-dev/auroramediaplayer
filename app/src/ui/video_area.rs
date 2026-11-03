use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use std::rc::Rc;
use std::cell::Cell;
use gtk4::{self as gtk, GLArea, Overlay, Label, Stack, Box, Orientation, Image, Spinner};
use gtk4::prelude::*;
use glib;
use libc;

use crate::i18n::t;
use crate::state::SharedState;

const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;

fn current_fbo() -> i32 {
    // Resolve glGetIntegerv at runtime via dlsym so we use whatever GL
    // implementation epoxy/GTK has already loaded (GLX or EGL).
    type GlGetIntegervFn = unsafe extern "C" fn(pname: u32, params: *mut i32);
    let sym = unsafe {
        libc::dlsym(libc::RTLD_DEFAULT, b"glGetIntegerv\0".as_ptr() as *const libc::c_char)
    };
    if sym.is_null() {
        return 0;
    }
    let gl_get_integerv: GlGetIntegervFn = unsafe { std::mem::transmute(sym) };
    let mut fbo: i32 = 0;
    unsafe { gl_get_integerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut fbo) };
    fbo
}

pub struct VideoArea {
    /// Root widget returned to the window layout (Overlay wrapping the Stack).
    root: Overlay,
    stack: Stack,
    // ── audio page ────────────────────────────────────────────────────────
    audio_cover: Image,
    audio_title: Label,
    audio_artist: Label,
    audio_album: Label,
    wave_playing: Rc<Cell<bool>>,
    // ── video page ────────────────────────────────────────────────────────
    idle_label: Label,
    spinner_box: Box,
    buffering_spinner: Spinner,
}

impl VideoArea {
    pub fn new(state: SharedState) -> Self {
        // ── Video page ────────────────────────────────────────────────────
        let gl_area = GLArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();

        let idle_label = Label::builder()
            .label(t("Open a file to start playing"))
            .css_classes(vec!["dim-label"])
            .build();

        let video_overlay = Overlay::builder()
            .child(&gl_area)
            .hexpand(true)
            .vexpand(true)
            .build();
        video_overlay.add_overlay(&idle_label);

        // ── Audio page ────────────────────────────────────────────────────
        let audio_cover = Image::builder()
            .icon_name("audio-x-generic-symbolic")
            .pixel_size(128)
            .css_classes(vec!["audio-cover-icon"])
            .build();

        let audio_title = Label::builder()
            .label("Unknown Track")
            .css_classes(vec!["title-2"])
            .justify(gtk::Justification::Center)
            .wrap(true)
            .max_width_chars(40)
            .build();

        let audio_artist = Label::builder()
            .label("")
            .css_classes(vec!["dim-label"])
            .justify(gtk::Justification::Center)
            .wrap(true)
            .max_width_chars(40)
            .build();

        let audio_album = Label::builder()
            .label("")
            .css_classes(vec!["caption", "dim-label"])
            .justify(gtk::Justification::Center)
            .wrap(true)
            .max_width_chars(40)
            .build();

        // ── Waveform visualizer ───────────────────────────────────────────
        let wave_phase   = Rc::new(Cell::new(0.0_f64));
        let wave_playing = Rc::new(Cell::new(false));

        let waveform = gtk::DrawingArea::builder()
            .width_request(220)
            .height_request(48)
            .css_classes(vec!["waveform"])
            .build();

        {
            let phase_c = wave_phase.clone();
            waveform.set_draw_func(move |_area, cr, w, h| {
                let phase   = phase_c.get();
                let n: i32  = 28;
                let bar_w   = 3.0_f64;
                let gap     = 2.5_f64;
                let total   = n as f64 * (bar_w + gap) - gap;
                let x0      = (w as f64 - total) / 2.0;
                let max_h   = h as f64 - 6.0;
                let min_h   = 3.0_f64;

                for i in 0..n {
                    let fi  = i as f64 / n as f64;
                    let t   = phase + fi * std::f64::consts::TAU;
                    // Two overlapping sines → organic movement
                    let amp = ((t.sin() * 0.55
                        + (t * 2.1).sin() * 0.30
                        + (t * 0.6).cos() * 0.15)
                        + 1.0) / 2.0;
                    let bar_h = min_h + amp * (max_h - min_h);

                    // Taper opacity at both edges
                    let edge  = (fi * std::f64::consts::PI).sin();
                    let alpha = 0.55 * edge + 0.25;

                    let x = x0 + i as f64 * (bar_w + gap);
                    let y = (h as f64 - bar_h) / 2.0;

                    cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
                    cr.rectangle(x, y, bar_w, bar_h);
                    cr.fill().ok();
                }
            });
        }

        // Advance the phase only while playing; always queue a redraw.
        {
            let phase_c   = wave_phase.clone();
            let playing_c = wave_playing.clone();
            let wave_weak = waveform.downgrade();
            glib::timeout_add_local(Duration::from_millis(50), move || {
                if playing_c.get() {
                    phase_c.set(phase_c.get() + 0.18);
                }
                if let Some(w) = wave_weak.upgrade() {
                    w.queue_draw();
                }
                glib::ControlFlow::Continue
            });
        }

        let meta_box = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        meta_box.append(&audio_cover);
        meta_box.append(&waveform);
        meta_box.append(&audio_title);
        meta_box.append(&audio_artist);
        meta_box.append(&audio_album);

        // Center the meta box in the available space
        let audio_page = Box::builder()
            .orientation(Orientation::Vertical)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .vexpand(true)
            .margin_bottom(100)
            .build();
        audio_page.append(&meta_box);

        // ── Stack: "video" | "audio" ──────────────────────────────────────
        let stack = Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        stack.add_named(&video_overlay, Some("video"));
        stack.add_named(&audio_page, Some("audio"));
        stack.set_visible_child_name("video");

        // ── Buffering spinner — overlaid on the whole stack ───────────────
        let spinner_box = Box::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(vec!["buffering-backdrop"])
            .build();
        let buffering_spinner = Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        spinner_box.append(&buffering_spinner);
        spinner_box.set_visible(false);

        let root = Overlay::builder()
            .child(&stack)
            .hexpand(true)
            .vexpand(true)
            .build();
        root.add_overlay(&spinner_box);

        // ── Wakeup flag ───────────────────────────────────────────────────
        let needs_render = Arc::new(AtomicBool::new(false));

        // ── realize ───────────────────────────────────────────────────────
        {
            let state_c = state.clone();
            let flag_c = needs_render.clone();
            gl_area.connect_realize(move |area| {
                area.make_current();
                if let Some(err) = area.error() {
                    log::error!("GLArea realize error: {err}");
                    return;
                }

                let render_ctx = {
                    let mut s = state_c.borrow_mut();
                    if let Some(ref mut player) = s.player {
                        match player.create_render_context() {
                            Ok(ctx) => {
                                log::info!("render context created successfully");
                                Some(ctx)
                            }
                            Err(e) => {
                                log::error!("create_render_context failed: {e}");
                                None
                            }
                        }
                    } else {
                        log::error!("GLArea realized but player is None");
                        None
                    }
                };

                if let Some(mut ctx) = render_ctx {
                    let flag = flag_c.clone();
                    ctx.set_update_callback(move || {
                        flag.store(true, Ordering::Relaxed);
                    });
                    state_c.borrow_mut().render_ctx = Some(ctx);

                    // Now that the render context exists, open any session-restore file.
                    let pending = state_c.borrow_mut().pending_open.take();
                    if let Some(path) = pending {
                        if let Some(p) = state_c.borrow().player.as_ref() {
                            p.execute(crate::player::PlayerCommand::Open(path)).ok();
                        }
                    }
                }
            });
        }

        // ── Wakeup timer ──────────────────────────────────────────────────
        {
            let flag_c = needs_render.clone();
            let gl_area_weak = gl_area.downgrade();
            glib::timeout_add_local(Duration::from_millis(8), move || {
                if flag_c.swap(false, Ordering::Relaxed) {
                    if let Some(a) = gl_area_weak.upgrade() {
                        a.queue_render();
                    }
                }
                glib::ControlFlow::Continue
            });
        }

        // ── resize ────────────────────────────────────────────────────────
        // GLArea reports its buffer size here in device pixels. widget.width()
        // is in logical pixels, which would shrink the video on HiDPI displays.
        let buffer_size = Rc::new(Cell::new((0, 0)));
        {
            let buffer_size_c = buffer_size.clone();
            gl_area.connect_resize(move |_, w, h| buffer_size_c.set((w, h)));
        }

        // ── render ────────────────────────────────────────────────────────
        {
            let state_c = state.clone();
            gl_area.connect_render(move |_area, _gl_ctx| {
                let s = state_c.borrow();
                if let Some(ctx) = s.render_ctx.as_ref() {
                    let fbo = current_fbo();
                    let (w, h) = buffer_size.get();
                    if let Err(e) = ctx.render(fbo, w, h, true) {
                        log::error!("mpv render: {e}");
                    }
                    ctx.report_swap();
                }
                glib::Propagation::Stop
            });
        }

        // ── unrealize ─────────────────────────────────────────────────────
        {
            let state_c = state.clone();
            gl_area.connect_unrealize(move |_| {
                state_c.borrow_mut().render_ctx = None;
            });
        }

        Self {
            root,
            stack,
            audio_cover,
            audio_title,
            audio_artist,
            audio_album,
            wave_playing,
            idle_label,
            spinner_box,
            buffering_spinner,
        }
    }

    pub fn widget(&self) -> &Overlay {
        &self.root
    }

    pub fn relabel(&self) {
        self.idle_label.set_label(t("Open a file to start playing"));
    }

    /// Switch to the video/idle page.
    pub fn show_video(&self) {
        self.stack.set_visible_child_name("video");
    }

    /// Switch to the audio-info page and populate the metadata labels.
    pub fn show_audio(&self, title: &str, artist: &str, album: &str) {
        self.stack.set_visible_child_name("audio");
        // While yt-dlp is resolving a URL, mpv's media-title can be:
        //   - the full URL  ("https://youtube.com/watch?v=…")
        //   - the path fragment ("watch?v=UGua3…&list=…")
        //   - the hostname   ("youtube.com")
        // All of these look like URL noise — show a clean placeholder instead.
        let is_url_noise = title.starts_with("http://")
            || title.starts_with("https://")
            || (title.contains('?') && title.contains('=') && !title.contains(' '));
        let display = if is_url_noise { "Loading…" } else if title.is_empty() { "Unknown Track" } else { title };
        self.audio_title.set_label(display);
        self.audio_artist.set_label(artist);
        self.audio_artist.set_visible(!artist.is_empty());
        self.audio_album.set_label(album);
        self.audio_album.set_visible(!album.is_empty());
    }

    /// Drive the waveform animation — call every polling tick.
    pub fn set_audio_playing(&self, playing: bool) {
        self.wave_playing.set(playing);
    }

    /// Show or hide the "open a file" placeholder on the video page.
    pub fn set_idle(&self, idle: bool) {
        self.idle_label.set_visible(idle);
    }

    /// Show/hide the buffering spinner on the video overlay.
    pub fn set_buffering(&self, buffering: bool) {
        self.spinner_box.set_visible(buffering);
        if buffering {
            self.buffering_spinner.start();
        } else {
            self.buffering_spinner.stop();
        }
    }
}
