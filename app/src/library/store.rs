use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use super::scanner::{scan_directory, MediaItem};

// ── Playlist ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub paths: Vec<PathBuf>,
}

// ── LibraryStore ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryStore {
    pub watched_folders: Vec<PathBuf>,
    pub items: Vec<MediaItem>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
}

impl LibraryStore {
    /// Path to the persisted library JSON file.
    pub fn store_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("aurora-media").join("library.json"))
    }

    /// Load from disk, or return an empty store on error.
    pub fn load() -> Self {
        let Some(path) = Self::store_path() else { return Self::default() };
        let Ok(data) = std::fs::read_to_string(&path) else { return Self::default() };
        let mut store: Self = serde_json::from_str(&data).unwrap_or_default();
        // Drop items whose files no longer exist on disk — covers renamed/deleted
        // files and stale entries from previous scanner bugs (e.g. .d nodes).
        store.items.retain(|item| item.path.is_file());
        store
    }

    /// Persist the current state to disk.
    pub fn save(&self) {
        let Some(path) = Self::store_path() else { return };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            std::fs::write(path, json).ok();
        }
    }

    /// Add a folder to the watch list (no-op if already present).
    /// Returns true if it was newly added.
    pub fn add_folder(&mut self, folder: PathBuf) -> bool {
        if self.watched_folders.contains(&folder) {
            return false;
        }
        self.watched_folders.push(folder);
        true
    }

    /// Remove a folder and all items sourced from it.
    pub fn remove_folder(&mut self, folder: &Path) {
        self.watched_folders.retain(|f| f != folder);
        self.items.retain(|item| !item.path.starts_with(folder));
    }

    /// Scan all watched folders and merge results.
    /// Preserves existing metadata (play_count, last_played, thumbnail_path)
    /// for items that are already in the store.
    pub fn rescan(&mut self) {
        let folders: Vec<PathBuf> = self.watched_folders.clone();
        let mut fresh: Vec<MediaItem> = Vec::new();

        for folder in &folders {
            fresh.extend(scan_directory(folder));
        }

        // Merge: carry forward existing metadata for known paths.
        for item in &mut fresh {
            if let Some(existing) = self.items.iter().find(|e| e.path == item.path) {
                item.duration_secs  = item.duration_secs.or(existing.duration_secs);
                item.artist         = item.artist.clone().or_else(|| existing.artist.clone());
                item.album          = item.album.clone().or_else(|| existing.album.clone());
                item.year           = item.year.or(existing.year);
                item.genre          = item.genre.clone().or_else(|| existing.genre.clone());
                item.track_number   = item.track_number.or(existing.track_number);
                item.width          = item.width.or(existing.width);
                item.height         = item.height.or(existing.height);
                item.thumbnail_path = item.thumbnail_path.clone().or_else(|| existing.thumbnail_path.clone());
                item.date_added     = existing.date_added; // keep original add date
                item.last_played    = existing.last_played;
                item.play_count     = existing.play_count;
            }
        }

        // Clear thumbnail paths that no longer exist on disk so they get
        // re-extracted on the next probe pass.
        for item in &mut fresh {
            if let Some(p) = &item.thumbnail_path {
                if !p.exists() {
                    item.thumbnail_path = None;
                }
            }
        }

        self.items = fresh;
    }

    // ── Playlist management ───────────────────────────────────────────────────

    /// Create a new empty playlist with the given name and return its ID.
    pub fn create_playlist(&mut self, name: &str) -> String {
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| self.playlists.len().to_string());
        self.playlists.push(Playlist {
            id: id.clone(),
            name: name.to_string(),
            paths: Vec::new(),
        });
        id
    }

    /// Add a path to a playlist. Returns false if already present or playlist not found.
    pub fn add_to_playlist(&mut self, id: &str, path: PathBuf) -> bool {
        if let Some(pl) = self.playlists.iter_mut().find(|p| p.id == id) {
            if !pl.paths.contains(&path) {
                pl.paths.push(path);
                return true;
            }
        }
        false
    }

    /// Delete a playlist by ID.
    pub fn delete_playlist(&mut self, id: &str) {
        self.playlists.retain(|p| p.id != id);
    }

    /// Record a play event for the given path.
    pub fn record_play(&mut self, path: &Path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        if let Some(item) = self.items.iter_mut().find(|i| i.path == path) {
            item.last_played = now;
            item.play_count += 1;
        }
    }
}
