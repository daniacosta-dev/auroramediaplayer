use std::ffi::CString;
use anyhow::Result;
use libmpv::Mpv;
use libmpv_sys;
use libc;

use super::pipeline::{PlayerCommand, RepeatMode};
use super::render::RenderContext;

/// Call mpv_command using the safe null-terminated pointer array API.
/// This handles paths with spaces and special characters, unlike mpv_command_string.
fn mpv_command_array(ctx: *mut libmpv_sys::mpv_handle, args: &[&str]) -> Result<()> {
    let cstrings: Vec<CString> = args
        .iter()
        .map(|s| CString::new(*s).map_err(|e| anyhow::anyhow!("{e}")))
        .collect::<Result<_>>()?;
    let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let ret = unsafe { libmpv_sys::mpv_command(ctx, ptrs.as_mut_ptr()) };
    if ret != 0 {
        anyhow::bail!("mpv_command failed: code {ret}");
    }
    Ok(())
}

/// Snapshot of all commonly-polled mpv properties, populated from a background thread
/// so the GTK main thread never blocks on mpv IPC calls.
#[derive(Clone, Default)]
pub struct MpvSnapshot {
    pub idle:      bool,
    pub pos:       f64,
    pub dur:       f64,
    pub paused:    bool,
    pub muted:     bool,
    pub volume:    f64,
    pub speed:     f64,
    pub title:     Option<String>,
    pub has_video: bool,
    pub artist:    Option<String>,
    pub album:     Option<String>,
    pub eof:       bool,
    pub buffering: bool,
    pub seeking:   bool,
    pub thumbnail: Option<String>,
    pub path:      Option<String>, // current URL/path being played
    pub uploader:  Option<String>, // yt-dlp uploader / channel name
    /// HDR format string when content is HDR, None when SDR.
    /// Values: "HDR10" (BT.2020 + PQ), "HLG" (BT.2020 + HLG), "HDR" (generic BT.2020)
    pub hdr_type:     Option<String>,
    /// Actual decoded video height in pixels (from video-params/h), None when no video.
    pub video_height: Option<i32>,
}

impl MpvSnapshot {
    /// Sensible non-zero defaults for "player just started, no media loaded".
    pub fn idle_defaults() -> Self {
        Self { idle: true, volume: 100.0, speed: 1.0, paused: true, ..Default::default() }
    }
}

/// Wraps a raw `mpv_handle` pointer for background-thread property reads.
/// SAFETY: libmpv's `mpv_get_property` is explicitly documented as thread-safe.
pub struct MpvPoller(*mut libmpv_sys::mpv_handle);
unsafe impl Send for MpvPoller {}

impl MpvPoller {
    fn get_f64(&self, prop: &str) -> f64 {
        let Ok(name) = CString::new(prop) else { return 0.0 };
        let mut val: f64 = 0.0;
        let ret = unsafe {
            libmpv_sys::mpv_get_property(
                self.0, name.as_ptr(),
                libmpv_sys::mpv_format_MPV_FORMAT_DOUBLE,
                &mut val as *mut f64 as *mut _,
            )
        };
        if ret == 0 { val } else { 0.0 }
    }

    fn get_bool(&self, prop: &str, default: bool) -> bool {
        let Ok(name) = CString::new(prop) else { return default };
        let mut val: i32 = 0;
        let ret = unsafe {
            libmpv_sys::mpv_get_property(
                self.0, name.as_ptr(),
                libmpv_sys::mpv_format_MPV_FORMAT_FLAG,
                &mut val as *mut i32 as *mut _,
            )
        };
        if ret == 0 { val != 0 } else { default }
    }

    fn get_i64(&self, prop: &str) -> Option<i64> {
        let Ok(name) = CString::new(prop) else { return None };
        let mut val: i64 = 0;
        let ret = unsafe {
            libmpv_sys::mpv_get_property(
                self.0, name.as_ptr(),
                libmpv_sys::mpv_format_MPV_FORMAT_INT64,
                &mut val as *mut i64 as *mut _,
            )
        };
        if ret == 0 { Some(val) } else { None }
    }

    fn get_str(&self, prop: &str) -> Option<String> {
        let Ok(name) = CString::new(prop) else { return None };
        let mut val: *mut libc::c_char = std::ptr::null_mut();
        let ret = unsafe {
            libmpv_sys::mpv_get_property(
                self.0, name.as_ptr(),
                libmpv_sys::mpv_format_MPV_FORMAT_STRING,
                &mut val as *mut *mut libc::c_char as *mut _,
            )
        };
        if ret == 0 && !val.is_null() {
            let s = unsafe { std::ffi::CStr::from_ptr(val).to_string_lossy().into_owned() };
            unsafe { libmpv_sys::mpv_free(val as *mut _) };
            Some(s)
        } else {
            None
        }
    }

    /// Read all polled properties in one go. May block if mpv holds its internal lock.
    /// Called from the background thread only.
    pub fn read_snapshot(&self) -> MpvSnapshot {
        let volume = self.get_f64("volume");
        let speed   = self.get_f64("speed");
        MpvSnapshot {
            idle:      self.get_bool("idle-active", true),
            pos:       self.get_f64("time-pos"),
            dur:       self.get_f64("duration"),
            paused:    self.get_bool("pause", true),
            muted:     self.get_bool("mute", false),
            volume:    if volume == 0.0 { 100.0 } else { volume },
            speed:     if speed  == 0.0 { 1.0   } else { speed  },
            title:     self.get_str("media-title"),
            has_video: self.get_i64("width").map(|w| w > 0).unwrap_or(false),
            artist:    self.get_str("metadata/by-key/Artist")
                           .or_else(|| self.get_str("metadata/by-key/artist")),
            album:     self.get_str("metadata/by-key/Album")
                           .or_else(|| self.get_str("metadata/by-key/album")),
            eof:       self.get_bool("eof-reached", false),
            buffering: self.get_i64("paused-for-cache").map(|v| v != 0).unwrap_or(false),
            seeking:   self.get_i64("seeking").map(|v| v != 0).unwrap_or(false),
            thumbnail: self.get_str("metadata/by-key/thumbnail")
                           .or_else(|| self.get_str("metadata/by-key/Thumbnail")),
            path:      self.get_str("path"),
            uploader:  self.get_str("metadata/by-key/uploader")
                           .or_else(|| self.get_str("metadata/by-key/Uploader"))
                           .or_else(|| self.get_str("metadata/by-key/channel"))
                           .or_else(|| self.get_str("metadata/by-key/Channel"))
                           .or_else(|| self.get_str("metadata/by-key/album_artist"))
                           .or_else(|| self.get_str("metadata/by-key/Album_Artist")),
            video_height: self.get_i64("video-params/h").map(|h| h as i32),
            hdr_type:  {
                let primaries = self.get_str("video-params/primaries").unwrap_or_default();
                let gamma     = self.get_str("video-params/gamma").unwrap_or_default();
                if primaries == "bt.2020" {
                    if gamma == "pq"  { Some("HDR10".into()) }
                    else if gamma == "hlg" { Some("HLG".into()) }
                    else               { Some("HDR".into()) }
                } else if gamma == "pq" || gamma == "hlg" {
                    Some("HDR".into())
                } else {
                    None
                }
            },
        }
    }
}

#[derive(Clone)]
pub struct TrackInfo {
    pub id: i64,
    pub kind: String,   // "audio" | "sub" | "video"
    pub title: Option<String>,
    pub lang: Option<String>,
    pub selected: bool,
}

/// Returns the path to the yt-dlp binary bundled for the current sandbox.
/// None means yt-dlp is expected on PATH (development / distro installs).
pub fn ytdl_path() -> Option<String> {
    if let Ok(snap) = std::env::var("SNAP") {
        Some(format!("{}/usr/bin/yt-dlp", snap))
    } else if std::env::var("FLATPAK_ID").is_ok() {
        Some("/app/bin/yt-dlp".to_string())
    } else {
        None
    }
}

pub struct MpvPlayer {
    pub(crate) mpv: Mpv,
}

impl MpvPlayer {
    /// Create a new mpv instance.  Video is rendered via the OpenGL render
    /// API — no window ID required.
    pub fn new() -> Result<Self> {
        // GTK resets LC_ALL; restore LC_NUMERIC=C before mpv_create().
        unsafe {
            libc::setlocale(libc::LC_NUMERIC, b"C\0".as_ptr() as *const libc::c_char);
        }

        let mpv = Mpv::with_initializer(|init| {
            // Must be set before mpv_initialize() so the render API works.
            init.set_property("vo", "libmpv")?;
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;

        mpv.set_property("hwdec", "auto-safe").ok();
        mpv.set_property("hr-seek", "yes").ok();
        mpv.set_property("volume", 100.0_f64).ok();
        mpv.set_property("keep-open", "yes").ok();
        mpv.set_property("ytdl", true).ok();

        // HDR: hint the display driver of the content's colorspace (Wayland/HDR displays)
        // and enable automatic HDR peak detection for tone mapping.
        mpv.set_property("target-colorspace-hint", true).ok();
        mpv.set_property("hdr-compute-peak", "auto").ok();
        // Only apply tone mapping when the content is actually HDR.
        mpv.set_property("tone-mapping-mode", "auto").ok();
        // Load user's preferred tone mapping algorithm (default: bt.2446a).
        let tone_mapping = super::super::ui::headerbar::load_app_settings()
            .tone_mapping
            .unwrap_or_else(|| "bt.2446a".into());
        mpv.set_property("tone-mapping", tone_mapping.as_str()).ok();
        // Point mpv to the bundled yt-dlp binary when running inside a sandbox
        // where PATH may not include the binary's location.
        if let Some(path) = ytdl_path() {
            let opts = format!("ytdl_hook-ytdl_path={}", path);
            mpv.set_property("script-opts", opts.as_str()).ok();
        }

        // Disable mpv's built-in OSD/input — we build our own.
        mpv.set_property("osc", false).ok();
        mpv.set_property("osd-bar", false).ok();
        mpv.set_property("osd-level", 0i64).ok();
        mpv.set_property("input-default-bindings", false).ok();
        mpv.set_property("input-vo-keyboard", false).ok();

        // Screenshots are saved to a temp path via ScreenshotToFile so that
        // the app can present a file-chooser dialog (portal) to let the user
        // pick the final destination — no broad filesystem permissions needed.

        Ok(Self { mpv })
    }

    /// Create a `MpvPoller` that can be moved to a background thread for non-blocking reads.
    pub fn make_poller(&self) -> MpvPoller {
        MpvPoller(self.mpv.ctx.as_ptr())
    }

    /// Create an OpenGL render context for this player.
    /// **Must be called while a GL context is current.**
    pub fn create_render_context(&mut self) -> Result<RenderContext> {
        // SAFETY: ctx is valid as long as self.mpv is alive.
        // We keep both together in PlayerState.
        RenderContext::new(unsafe { self.mpv.ctx.as_ptr() })
    }

    pub fn execute(&self, cmd: PlayerCommand) -> Result<()> {
        match cmd {
            PlayerCommand::Open(path) => {
                let path_str = path.to_str().ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?;
                mpv_command_array(self.mpv.ctx.as_ptr(), &["loadfile", path_str, "replace"])?;
                self.mpv.set_property("pause", false).ok();
            }
            PlayerCommand::OpenUrl(url) => {
                mpv_command_array(self.mpv.ctx.as_ptr(), &["loadfile", &url, "replace"])?;
                self.mpv.set_property("pause", false).ok();
            }
            PlayerCommand::Play => {
                self.mpv.set_property("pause", false).ok();
            }
            PlayerCommand::Pause => {
                self.mpv.set_property("pause", true).ok();
            }
            PlayerCommand::TogglePause => {
                self.mpv.command("cycle", &["pause"]).ok();
            }
            PlayerCommand::Stop => {
                self.mpv.command("stop", &[]).ok();
            }
            PlayerCommand::Seek(secs) => {
                self.mpv
                    .command("seek", &[&secs.to_string(), "absolute"])
                    .ok();
            }
            PlayerCommand::SetVolume(vol) => {
                self.mpv.set_property("volume", vol).ok();
            }
            PlayerCommand::Mute(muted) => {
                self.mpv.set_property("mute", muted).ok();
            }
            PlayerCommand::SetSpeed(speed) => {
                self.mpv.set_property("speed", speed).ok();
            }
            PlayerCommand::NextFrame => {
                self.mpv.command("frame-step", &[]).ok();
            }
            PlayerCommand::PrevFrame => {
                self.mpv.command("frame-back-step", &[]).ok();
            }
            PlayerCommand::Screenshot => {
                self.mpv.command("screenshot", &[]).ok();
            }
            PlayerCommand::ScreenshotToFile(path) => {
                let path_str = path.to_str().ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?;
                mpv_command_array(self.mpv.ctx.as_ptr(), &["screenshot-to-file", path_str, "video"])?;
            }
            PlayerCommand::SetAudioTrack(id) => {
                self.mpv.set_property("aid", id).ok();
            }
            PlayerCommand::SetSubtitleTrack(id) => {
                if id == 0 {
                    self.mpv.set_property("sid", "no").ok();
                } else {
                    self.mpv.set_property("sid", id).ok();
                }
            }
            PlayerCommand::SetVideoEnabled(enabled) => {
                if enabled {
                    self.mpv.set_property("vid", "auto").ok();
                } else {
                    self.mpv.set_property("vid", "no").ok();
                }
            }
            PlayerCommand::SetToneMapping(mode) => {
                self.mpv.set_property("tone-mapping", mode.as_str()).ok();
            }
            PlayerCommand::AddSubtitle(path) => {
                let path_str = path.to_str().ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?;
                mpv_command_array(self.mpv.ctx.as_ptr(), &["sub-add", path_str, "select"])?;
            }
            PlayerCommand::SetQuality { format, url, start_pos } => {
                self.mpv.set_property("ytdl-format", format.as_str()).ok();
                let start_opt = format!("start={}", start_pos as u64);
                mpv_command_array(self.mpv.ctx.as_ptr(), &["loadfile", &url, "replace", &start_opt])?;
                self.mpv.set_property("pause", false).ok();
            }
            PlayerCommand::SetRepeat(mode) => match mode {
                RepeatMode::None => {
                    self.mpv.set_property("loop-playlist", "no").ok();
                    self.mpv.set_property("loop-file", "no").ok();
                }
                RepeatMode::Playlist => {
                    self.mpv.set_property("loop-playlist", "inf").ok();
                    self.mpv.set_property("loop-file", "no").ok();
                }
                RepeatMode::One => {
                    self.mpv.set_property("loop-file", "inf").ok();
                    self.mpv.set_property("loop-playlist", "no").ok();
                }
            },
        }
        Ok(())
    }

    pub fn duration(&self) -> Option<f64> {
        self.mpv.get_property("duration").ok()
    }

    pub fn position(&self) -> Option<f64> {
        self.mpv.get_property("time-pos").ok()
    }

    pub fn volume(&self) -> f64 {
        self.mpv.get_property("volume").unwrap_or(100.0)
    }

    pub fn is_paused(&self) -> bool {
        self.mpv.get_property("pause").unwrap_or(true)
    }

    pub fn is_muted(&self) -> bool {
        self.mpv.get_property("mute").unwrap_or(false)
    }

    pub fn media_title(&self) -> Option<String> {
        self.mpv.get_property("media-title").ok()
    }

    /// True when mpv has played to the end of the current file.
    /// With keep-open=yes, mpv pauses at the last frame instead of closing.
    pub fn eof_reached(&self) -> bool {
        self.mpv
            .get_property::<bool>("eof-reached")
            .unwrap_or(false)
    }

    pub fn is_idle(&self) -> bool {
        self.mpv
            .get_property::<bool>("idle-active")
            .unwrap_or(true)
    }

    /// True when mpv has paused playback because the network cache ran out.
    pub fn is_buffering(&self) -> bool {
        self.mpv
            .get_property::<i64>("paused-for-cache")
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    /// True while mpv is actively seeking (including network re-buffer after seek).
    pub fn is_seeking(&self) -> bool {
        self.mpv
            .get_property::<i64>("seeking")
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    /// Returns the last playback error string, if any.
    /// mpv resets this when a new file loads successfully.
    pub fn last_error(&self) -> Option<String> {
        self.mpv
            .get_property::<String>("error-string")
            .ok()
            .filter(|s| !s.is_empty() && s != "(empty)")
    }

    /// Returns true when the current file has a real video track.
    /// Audio-only files (mp3, flac, ogg…) return false.
    /// Note: mpv also exposes embedded album art as a video track, so files
    /// with cover art embedded will return true and render through the GLArea,
    /// which is exactly what we want.
    pub fn has_video(&self) -> bool {
        self.mpv
            .get_property::<i64>("width")
            .map(|w| w > 0)
            .unwrap_or(false)
    }

    pub fn metadata_artist(&self) -> Option<String> {
        self.mpv
            .get_property("metadata/by-key/Artist")
            .ok()
            .or_else(|| self.mpv.get_property("metadata/by-key/artist").ok())
    }

    pub fn metadata_album(&self) -> Option<String> {
        self.mpv
            .get_property("metadata/by-key/Album")
            .ok()
            .or_else(|| self.mpv.get_property("metadata/by-key/album").ok())
    }

    pub fn speed(&self) -> f64 {
        self.mpv.get_property("speed").unwrap_or(1.0)
    }

    pub fn track_list(&self) -> Vec<TrackInfo> {
        let count: i64 = self.mpv.get_property("track-list/count").unwrap_or(0);
        (0..count).filter_map(|i| {
            let kind: String = self.mpv.get_property::<String>(&format!("track-list/{i}/type")).ok()?;
            let id: i64 = self.mpv.get_property(&format!("track-list/{i}/id")).unwrap_or(0);
            let title: Option<String> = self.mpv.get_property(&format!("track-list/{i}/title")).ok();
            let lang: Option<String> = self.mpv.get_property(&format!("track-list/{i}/lang")).ok();
            let selected: bool = self.mpv.get_property(&format!("track-list/{i}/selected")).unwrap_or(false);
            Some(TrackInfo { id, kind, title, lang, selected })
        }).collect()
    }

    pub fn chapter_list(&self) -> Vec<(String, f64)> {
        let count: i64 = self.mpv.get_property("chapter-list/count").unwrap_or(0);
        (0..count).map(|i| {
            let title: String = self.mpv.get_property::<String>(&format!("chapter-list/{i}/title"))
                .unwrap_or_else(|_| format!("Chapter {}", i + 1));
            let time: f64 = self.mpv.get_property(&format!("chapter-list/{i}/time")).unwrap_or(0.0);
            (title, time)
        }).collect()
    }
}
