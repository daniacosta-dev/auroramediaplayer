/// Background metadata extraction via ffprobe.
///
/// Uses the same ffprobe binary that playlist.rs already relies on for duration.
/// Runs in a std::thread and notifies the GTK main thread via glib::MainContext::channel.
use std::path::{Path, PathBuf};
use std::time::Duration;

use glib;

use super::scanner::MediaItem;

/// Full metadata extracted from a single file by ffprobe.
#[derive(Debug)]
pub struct ProbeResult {
    pub path: PathBuf,
    pub duration_secs: Option<f64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub thumbnail_path: Option<PathBuf>,
}

impl ProbeResult {
    /// Merge this probe result into an existing MediaItem (in-place).
    pub fn apply_to(&self, item: &mut MediaItem) {
        if item.duration_secs.is_none() { item.duration_secs = self.duration_secs; }
        if item.title == item.path.file_stem()
            .and_then(|s| s.to_str()).unwrap_or("Unknown")
        {
            // Only override the filename-derived title if ffprobe found a real one.
            if let Some(t) = &self.title { item.title = t.clone(); }
        }
        if item.artist.is_none()       { item.artist       = self.artist.clone(); }
        if item.album.is_none()        { item.album        = self.album.clone(); }
        if item.year.is_none()         { item.year         = self.year; }
        if item.genre.is_none()        { item.genre        = self.genre.clone(); }
        if item.track_number.is_none() { item.track_number = self.track_number; }
        if item.width.is_none()        { item.width        = self.width; }
        if item.height.is_none()       { item.height       = self.height; }
        if item.thumbnail_path.is_none() { item.thumbnail_path = self.thumbnail_path.clone(); }
    }
}

/// Run ffprobe on the given path and return a ProbeResult.
/// Blocks the calling thread — always call from a background thread.
pub fn probe_file(path: &Path) -> ProbeResult {
    let mut result = ProbeResult {
        path: path.to_path_buf(),
        duration_secs: None,
        title: None,
        artist: None,
        album: None,
        year: None,
        genre: None,
        track_number: None,
        width: None,
        height: None,
        thumbnail_path: None,
    };

    // ffprobe JSON output: format tags + first video stream dimensions.
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output();

    let Ok(out) = output else { return result };
    let Ok(json_str) = String::from_utf8(out.stdout) else { return result };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) else { return result };

    // ── Format / duration ──────────────────────────────────────────────────
    if let Some(fmt) = json.get("format") {
        result.duration_secs = fmt.get("duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());

        if let Some(tags) = fmt.get("tags") {
            result.title       = tag(&tags, &["title", "TITLE"]);
            result.artist      = tag(&tags, &["artist", "ARTIST", "album_artist", "ALBUM_ARTIST"]);
            result.album       = tag(&tags, &["album", "ALBUM"]);
            result.genre       = tag(&tags, &["genre", "GENRE"]);
            result.year        = tag(&tags, &["date", "DATE", "year", "YEAR"])
                .and_then(|s| s[..4.min(s.len())].parse::<u32>().ok());
            result.track_number = tag(&tags, &["track", "TRACK"])
                .and_then(|s| s.split('/').next()?.parse::<u32>().ok());
        }
    }

    // ── Streams: video dimensions ──────────────────────────────────────────
    if let Some(streams) = json.get("streams").and_then(|s| s.as_array()) {
        for stream in streams {
            if stream.get("codec_type").and_then(|v| v.as_str()) == Some("video") {
                result.width  = stream.get("width").and_then(|v| v.as_u64()).map(|v| v as u32);
                result.height = stream.get("height").and_then(|v| v.as_u64()).map(|v| v as u32);
                break;
            }
        }
    }

    // ── Thumbnail extraction ───────────────────────────────────────────────
    result.thumbnail_path = extract_thumbnail(path, result.duration_secs);

    result
}

/// Extract embedded cover art (audio) or a key frame (video) to the cache dir.
/// Returns the cache path on success.
fn extract_thumbnail(source: &Path, duration_secs: Option<f64>) -> Option<PathBuf> {
    let cache_dir = dirs::cache_dir()?.join("aurora-media").join("thumbs");
    std::fs::create_dir_all(&cache_dir).ok()?;

    // Use a hash of the path as the filename to avoid collisions.
    // Suffix "_l" marks 200×120 thumbnails — bumped from "_m" (160×100) when
    // the card size increased.  Old _m files are simply ignored (different name).
    let hash = fnv_hash(source.to_string_lossy().as_bytes());
    let thumb_path = cache_dir.join(format!("{hash:016x}_l.jpg"));

    // Skip extraction if the thumbnail already exists.
    if thumb_path.exists() {
        return Some(thumb_path);
    }

    // Seek to 10% of the duration (clamped 1–60 s) so we avoid black leader
    // frames at the start.  For audio with embedded cover art, the seek has no
    // practical effect because the image stream has no time dimension.
    let seek_secs = duration_secs
        .map(|d| (d * 0.10).clamp(1.0, 60.0))
        .unwrap_or(3.0);
    let seek_arg = format!("{seek_secs:.1}");

    // ffmpeg: input-seek to avoid black frames, scale to fit within 200×120,
    // pad to exactly 200×120 so gtk::Picture always has a fixed natural size.
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v", "quiet",
            "-ss", &seek_arg,        // seek before input (fast keyframe seek)
            "-y",                    // overwrite
            "-i", &source.to_string_lossy(),
            "-an",                   // no audio
            "-vframes", "1",         // single frame
            "-vf", "scale=200:120:force_original_aspect_ratio=decrease,pad=200:120:(ow-iw)/2:(oh-ih)/2:color=black",
            "-q:v", "1",             // highest JPEG quality (1=best, 31=worst)
        ])
        .arg(&thumb_path)
        .status();

    match status {
        Ok(s) if s.success() && thumb_path.exists() => Some(thumb_path),
        _ => None,
    }
}

/// FNV-1a 64-bit hash — no external dependency.
pub fn fnv_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// Look up a tag value trying multiple key variants (case-insensitive fallback).
fn tag(tags: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for &key in keys {
        if let Some(v) = tags.get(key).and_then(|v| v.as_str()) {
            let s = v.trim().to_string();
            if !s.is_empty() { return Some(s); }
        }
    }
    None
}

// ── Async probe ───────────────────────────────────────────────────────────────

/// Probe a list of paths in a background thread.
/// Calls `on_result` on the GTK main thread for each completed probe.
/// Call this after a rescan to enrich the library store without blocking the UI.
///
/// Uses the same std::sync::mpsc + glib::timeout_add_local polling pattern
/// as probe_duration_async in playlist.rs.
pub fn probe_batch_async<F>(paths: Vec<PathBuf>, on_result: F)
where
    F: Fn(ProbeResult) + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<ProbeResult>();

    std::thread::spawn(move || {
        for path in paths {
            let result = probe_file(&path);
            if tx.send(result).is_err() {
                break; // receiver dropped — UI gone
            }
        }
    });

    // Poll every 200 ms from the GTK main thread.
    glib::timeout_add_local(Duration::from_millis(200), move || {
        loop {
            match rx.try_recv() {
                Ok(result) => on_result(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return glib::ControlFlow::Break;
                }
            }
        }
    });
}
