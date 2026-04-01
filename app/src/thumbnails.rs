//! Seek-bar thumbnail strip.
//!
//! When a local file is opened, `generate_async` spawns a background thread
//! that runs ffmpeg to extract a set of JPEG frames distributed across the
//! duration.  The results are written into a `SharedCache` (`Arc<Mutex<…>>`)
//! that the UI reads on every hover-motion event.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FRAME_COUNT: usize = 20;
const THUMB_W: u32 = 160;
const THUMB_H: u32 = 90;

// ── Public types ──────────────────────────────────────────────────────────────

pub struct ThumbnailCache {
    /// (normalised position 0.0–1.0, JPEG file path)
    frames: Vec<(f64, PathBuf)>,
}

/// Thread-safe handle shared between the background generator and the UI.
pub type SharedCache = Arc<Mutex<Option<ThumbnailCache>>>;

impl ThumbnailCache {
    /// Returns the path of the frame whose position is closest to `frac`.
    pub fn frame_at(&self, frac: f64) -> Option<&Path> {
        self.frames
            .iter()
            .min_by(|(a, _), (b, _)| {
                (a - frac)
                    .abs()
                    .partial_cmp(&(b - frac).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(_, p)| p.as_path())
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Spawn a background thread that extracts thumbnail frames with ffmpeg.
///
/// If ffmpeg is not installed the thread exits silently and `cache` remains
/// `None`.  The existing thumbnail directory is reused across runs to avoid
/// redundant extraction for the same file.
pub fn generate_async(path: PathBuf, duration: f64, cache: SharedCache) {
    std::thread::spawn(move || {
        if let Some(tc) = extract(&path, duration) {
            if let Ok(mut g) = cache.lock() {
                *g = Some(tc);
            }
        }
    });
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn thumb_dir(path: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    std::env::temp_dir().join(format!("aurora-thumbs-{:016x}", h.finish()))
}

fn collect_frames(dir: &Path, duration: f64) -> ThumbnailCache {
    let interval = duration / FRAME_COUNT as f64;
    let frames = (1..=FRAME_COUNT)
        .filter_map(|i| {
            let p = dir.join(format!("{i:04}.jpg"));
            if p.exists() {
                let pos = ((i as f64 - 0.5) * interval / duration).clamp(0.0, 1.0);
                Some((pos, p))
            } else {
                None
            }
        })
        .collect();
    ThumbnailCache { frames }
}

fn extract(path: &Path, duration: f64) -> Option<ThumbnailCache> {
    if duration <= 0.0 {
        return None;
    }

    let dir = thumb_dir(path);

    // Re-use cached strip if all frames are present.
    let existing = collect_frames(&dir, duration);
    if existing.frames.len() == FRAME_COUNT {
        return Some(existing);
    }

    std::fs::create_dir_all(&dir).ok()?;

    let interval = duration / FRAME_COUNT as f64;
    let fps = format!("1/{:.3}", interval.max(0.1));
    let filter = format!(
        "fps={fps},scale={THUMB_W}:{THUMB_H}:\
         force_original_aspect_ratio=decrease,\
         pad={THUMB_W}:{THUMB_H}:(ow-iw)/2:(oh-ih)/2:black"
    );
    let out_pattern = dir.join("%04d.jpg");
    let out_str = out_pattern.to_str()?;

    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            path.to_str()?,
            "-vf",
            &filter,
            "-q:v",
            "5",
            "-y",
            out_str,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ok {
        return None;
    }

    let result = collect_frames(&dir, duration);
    if result.frames.is_empty() { None } else { Some(result) }
}
