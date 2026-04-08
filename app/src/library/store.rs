use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use super::scanner::{scan_directory, MediaItem, MediaKind};

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
        // Stream items (URLs) are kept — they don't correspond to local files.
        store.items.retain(|item| item.kind == MediaKind::Stream || item.path.is_file());
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

    /// Remove a folder, all items sourced from it, and any playlist entries
    /// that pointed to files inside it.
    pub fn remove_folder(&mut self, folder: &Path) {
        self.watched_folders.retain(|f| f != folder);
        self.items.retain(|item| !item.path.starts_with(folder));
        // Keep only playlist paths that are still tracked in the library.
        let valid: std::collections::HashSet<&std::path::Path> =
            self.items.iter().map(|i| i.path.as_path()).collect();
        for pl in &mut self.playlists {
            pl.paths.retain(|p| valid.contains(p.as_path()));
        }
    }

    /// Scan all watched folders and merge results.
    /// Preserves existing metadata (play_count, last_played, thumbnail_path)
    /// for items that are already in the store.
    pub fn rescan(&mut self) {
        // Stream items (URL playlists) live outside watched folders — preserve them.
        let stream_items: Vec<MediaItem> = self.items.iter()
            .filter(|i| i.kind == MediaKind::Stream)
            .cloned()
            .collect();

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
        // Re-attach stream items after rescan.
        self.items.extend(stream_items);
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

    /// Delete a playlist by ID. Also removes stream items that are no longer
    /// referenced by any URL playlist.
    pub fn delete_playlist(&mut self, id: &str) {
        self.playlists.retain(|p| p.id != id);
        // Clean up orphaned stream items — those not in any remaining playlist.
        let referenced: std::collections::HashSet<&Path> = self.playlists.iter()
            .flat_map(|p| p.paths.iter().map(|p| p.as_path()))
            .collect();
        self.items.retain(|item| {
            item.kind != MediaKind::Stream || referenced.contains(item.path.as_path())
        });
    }

    /// Add items to the "Recent URLs" singleton playlist (create it if needed).
    /// Returns newly created MediaItem::Stream entries (already in `self.items`).
    pub fn append_to_recent_urls(&mut self, items: Vec<(String, String)>) -> Vec<MediaItem> {
        const RECENT_ID: &str = "recent-urls";
        const RECENT_NAME: &str = "Recent URLs";

        // Find or create the "Recent URLs" playlist.
        let pl_idx = if let Some(i) = self.playlists.iter().position(|p| p.id == RECENT_ID) {
            i
        } else {
            self.playlists.push(Playlist {
                id:    RECENT_ID.to_string(),
                name:  RECENT_NAME.to_string(),
                paths: Vec::new(),
            });
            self.playlists.len() - 1
        };

        let mut new_items = Vec::new();
        for (title, url) in &items {
            let url_path = PathBuf::from(url);
            // Skip if already in the playlist.
            if self.playlists[pl_idx].paths.contains(&url_path) { continue; }
            self.playlists[pl_idx].paths.push(url_path.clone());
            // Add as stream item if not already tracked.
            if !self.items.iter().any(|i| i.path == url_path) {
                let item = MediaItem::new_stream(url.clone(), title.clone());
                new_items.push(item.clone());
                self.items.push(item);
            }
        }
        new_items
    }

    /// Save a URL playlist (items from the "Open URL" dialog) to the store.
    /// Returns the playlist ID (existing if an identical one already exists).
    /// Items are `(display_title, url)` pairs.
    pub fn add_url_playlist(&mut self, name: String, items: Vec<(String, String)>) -> Option<String> {
        let paths: Vec<PathBuf> = items.iter().map(|(_, u)| PathBuf::from(u)).collect();

        // Skip if a playlist with the exact same URLs already exists.
        if let Some(existing) = self.playlists.iter().find(|p| p.paths == paths) {
            return None; // already saved, no new items to add
        }

        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| self.playlists.len().to_string());

        for (title, url) in &items {
            let url_path = PathBuf::from(url);
            // Add as stream item if not already tracked.
            if !self.items.iter().any(|i| i.path == url_path) {
                self.items.push(MediaItem::new_stream(url.clone(), title.clone()));
            }
        }

        self.playlists.push(Playlist { id: id.clone(), name, paths });
        Some(id)
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
