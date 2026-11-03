//! Seek-bar thumbnail strip.
//!
//! When a local file is opened, `generate_async` spawns a background thread
//! that runs ffmpeg once per thumbnail, seeking directly to each timestamp so
//! the cost does not grow with the length of the video.  The results are
//! written into a `SharedCache` (`Arc<Mutex<…>>`) that the UI reads on every
//! hover-motion event.

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
/// redundant extraction for the same file.  Extraction stops early once the
/// caller drops its handle to `cache`, e.g. when another file is opened.
pub fn generate_async(path: PathBuf, duration: f64, cache: SharedCache) {
    std::thread::spawn(move || {
        if let Some(tc) = extract(&path, duration, &cache) {
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

fn extract(path: &Path, duration: f64, cache: &SharedCache) -> Option<ThumbnailCache> {
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
    let filter = format!(
        "scale={THUMB_W}:{THUMB_H}:\
         force_original_aspect_ratio=decrease,\
         pad={THUMB_W}:{THUMB_H}:(ow-iw)/2:(oh-ih)/2:black"
    );
    let input = path.to_str()?;

    for i in 1..=FRAME_COUNT {
        // This thread holds the only remaining handle: nobody wants the result.
        if Arc::strong_count(cache) == 1 {
            return None;
        }

        let out = dir.join(format!("{i:04}.jpg"));
        if out.exists() {
            continue;
        }

        // Write to a temporary name so an interrupted run never leaves a
        // truncated JPEG that a later run would accept as a finished frame.
        let tmp = dir.join(format!("{i:04}.tmp.jpg"));
        let timestamp = format!("{:.3}", (i as f64 - 0.5) * interval);

        // `-ss` before `-i` seeks to the nearest keyframe instead of decoding
        // from the start of the file.
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-threads",
                "1",
                "-noaccurate_seek",
                "-ss",
                &timestamp,
                "-i",
                input,
                "-frames:v",
                "1",
                "-update",
                "1",
                "-an",
                "-sn",
                "-vf",
                &filter,
                "-q:v",
                "5",
                "-y",
            ])
            .arg(&tmp)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?; // ffmpeg is not installed.

        // ffmpeg can succeed without writing anything if the seek lands past
        // the last frame, so check for the file too.
        if !status.success() || std::fs::rename(&tmp, &out).is_err() {
            std::fs::remove_file(&tmp).ok();
        }
    }

    let result = collect_frames(&dir, duration);
    if result.frames.is_empty() { None } else { Some(result) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a short test video, or returns `None` if ffmpeg is unavailable.
    fn test_video(name: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("aurora-thumb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(name);
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x180:rate=10:duration=20")
            .arg(&path)
            .status()
            .ok()?
            .success()
            .then_some(path)
    }

    fn clean_up(video: &Path) {
        std::fs::remove_dir_all(thumb_dir(video)).ok();
        std::fs::remove_file(video).ok();
    }

    #[test]
    fn extracts_every_frame() {
        let Some(video) = test_video("all.mp4") else { return };
        let cache: SharedCache = Arc::new(Mutex::new(None));
        let _ui_handle = cache.clone();

        let result = extract(&video, 20.0, &cache).expect("thumbnails");

        assert_eq!(result.frames.len(), FRAME_COUNT);
        let names: Vec<_> = std::fs::read_dir(thumb_dir(&video))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.contains("tmp")), "{names:?}");
        clean_up(&video);
    }

    #[test]
    fn stops_when_the_caller_dropped_its_handle() {
        let Some(video) = test_video("cancelled.mp4") else { return };
        let cache: SharedCache = Arc::new(Mutex::new(None));

        assert!(extract(&video, 20.0, &cache).is_none());
        assert_eq!(collect_frames(&thumb_dir(&video), 20.0).frames.len(), 0);
        clean_up(&video);
    }
}
