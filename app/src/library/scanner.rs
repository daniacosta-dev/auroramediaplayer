use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub path: PathBuf,
    pub title: String,
    pub kind: MediaKind,

    // Basic metadata (populated by scanner or probe_metadata)
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub track_number: Option<u32>,

    // Video-specific
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,

    // Cached thumbnail path (extracted cover art or video frame)
    #[serde(default)]
    pub thumbnail_path: Option<PathBuf>,

    // Library tracking
    #[serde(default)]
    pub date_added: Option<u64>,   // Unix timestamp
    #[serde(default)]
    pub last_played: Option<u64>,  // Unix timestamp
    #[serde(default)]
    pub play_count: u32,
}

impl MediaItem {
    /// Display resolution label e.g. "4K", "1080p", "720p".
    pub fn resolution_label(&self) -> Option<&'static str> {
        match (self.width, self.height) {
            (Some(w), _) if w >= 3840 => Some("4K"),
            (_, Some(h)) if h >= 2160 => Some("4K"),
            (_, Some(h)) if h >= 1080 => Some("1080p"),
            (_, Some(h)) if h >= 720  => Some("720p"),
            _ => None,
        }
    }
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v",
    // .ts is MPEG Transport Stream — only included if the file is large enough
    // (checked via MIN_MEDIA_BYTES) to avoid matching TypeScript source files.
    "ts", "mts", "m2ts",
];

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "aac", "m4a", "wav", "wma", "ape", "alac",
];

/// Minimum file size in bytes to be considered a real media file.
/// Filters out zero-byte placeholders, partial downloads, and TypeScript
/// source files that happen to share extensions with media formats (e.g. .ts).
const MIN_MEDIA_BYTES: u64 = 64 * 1024; // 64 KB

/// Scans a directory recursively for media files.
/// Returns basic MediaItems without probed metadata (fast).
pub fn scan_directory(dir: &Path) -> Vec<MediaItem> {
    let mut items = Vec::new();
    scan_recursive(dir, &mut items);
    items
}

fn scan_recursive(dir: &Path, out: &mut Vec<MediaItem>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();

        // Skip hidden entries (names starting with '.').
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            scan_recursive(&path, out);
            continue;
        }

        // Only process regular files — skip symlinks, device nodes, pipes, etc.
        if !path.is_file() {
            continue;
        }

        let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
        let ext_lower = ext.to_lowercase();

        let kind = if VIDEO_EXTENSIONS.contains(&ext_lower.as_str()) {
            MediaKind::Video
        } else if AUDIO_EXTENSIONS.contains(&ext_lower.as_str()) {
            MediaKind::Audio
        } else {
            continue;
        };

        // Reject files that are too small to be real media
        // (catches TypeScript .ts files, stubs, partial downloads, etc.).
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if size < MIN_MEDIA_BYTES {
            continue;
        }

        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        let date_added = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();

        out.push(MediaItem {
            path,
            title,
            kind,
            duration_secs: None,
            artist: None,
            album: None,
            year: None,
            genre: None,
            track_number: None,
            width: None,
            height: None,
            thumbnail_path: None,
            date_added,
            last_played: None,
            play_count: 0,
        });
    }
}
