use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MediaKind {
    Video,
    Audio,
    /// URL/stream item from the "Open URL" dialog — stored in the library
    /// but excluded from All / Video / Audio browsing views.
    Stream,
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
    /// Create a stream item from a URL and display title.
    pub fn new_stream(url: String, title: String) -> Self {
        let date_added = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        Self {
            path: std::path::PathBuf::from(url),
            title,
            kind: MediaKind::Stream,
            duration_secs: None,
            artist: None, album: None, year: None, genre: None, track_number: None,
            width: None, height: None, thumbnail_path: None,
            date_added,
            last_played: None,
            play_count: 0,
        }
    }

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
/// Filters out zero-byte placeholders, partial downloads, and small code files
/// that share extensions with media formats (e.g. .ts TypeScript).
const MIN_MEDIA_BYTES: u64 = 64 * 1024; // 64 KB

/// Directory names that are never media sources — always skipped during scan.
const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", "dist", "build", ".git", ".svn",
    "__pycache__", "vendor", "bower_components", ".cache", ".npm",
    ".yarn", "out", "coverage", ".next", ".nuxt",
];

/// Returns true if the lowercased file name looks like a code/build artifact
/// rather than a real media file, even if its final extension matches a media format.
///
/// Examples caught:
///   foo.d.ts        → TypeScript declaration file  (ext = "ts")
///   foo.js.map      → source map                   (ext = "map" — not in our list anyway)
///   index.d.mts     → ESM declaration              (ext = "mts")
///   chunk.d.m2ts    → would be absurd but caught   (ext = "m2ts")
fn is_code_artifact(file_name_lower: &str) -> bool {
    // Multi-extension patterns that indicate code files sharing a media extension.
    const CODE_SUFFIXES: &[&str] = &[
        ".d.ts", ".d.mts", ".d.cts",   // TypeScript declarations
        ".min.ts",                       // minified TS (rare but possible)
        ".spec.ts", ".test.ts",          // test files
        ".stories.ts",                   // Storybook
    ];
    CODE_SUFFIXES.iter().any(|suffix| file_name_lower.ends_with(suffix))
}

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
        // Use DirEntry::file_type() — no extra stat(), uses the type already
        // returned by readdir(3). Falls back to skip on error.
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        let path = entry.path();

        // Skip hidden entries (names starting with '.').
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip hidden entries and known non-media directories.
        if name.starts_with('.') || SKIP_DIRS.contains(&name) {
            continue;
        }

        if file_type.is_dir() {
            scan_recursive(&path, out);
            continue;
        }

        // Only process plain regular files.
        // Rejects symlinks, device nodes, named pipes, sockets, etc.
        if !file_type.is_file() {
            continue;
        }

        // path.extension() only returns the LAST segment after a dot.
        // For multi-extension files like "foo.d.ts", extension() = "ts" but
        // the full file_name is "foo.d.ts". We check the full name to catch
        // TypeScript declaration files, source maps, and other code artifacts
        // that would otherwise match media extensions.
        let file_name_lower = name.to_lowercase();
        if is_code_artifact(&file_name_lower) {
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
