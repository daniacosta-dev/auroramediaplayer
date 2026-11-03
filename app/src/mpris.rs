use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

// ── Public state snapshot ────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct MprisState {
    pub playback_status: String, // "Playing" | "Paused" | "Stopped"
    pub loop_status: String,     // "None" | "Track" | "Playlist"
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: String, // https:// or file:// URI for cover art
    pub position_us: i64, // microseconds
    pub duration_us: i64, // microseconds
    pub volume: f64,      // 0.0 – 1.0
    pub can_go_next: bool,
    pub can_go_previous: bool,
}

impl MprisState {
    pub fn stopped() -> Self {
        Self {
            playback_status: "Stopped".into(),
            loop_status: "None".into(),
            volume: 1.0,
            ..Default::default()
        }
    }
}

// ── Commands coming back from D-Bus to the GTK main thread ──────────────────

#[derive(Debug)]
pub enum MprisCommand {
    PlayPause,
    Next,
    Previous,
    Seek(i64),      // offset in microseconds
    SetVolume(f64), // 0.0 – 1.0
}

// ── Public handle given to the GTK thread ───────────────────────────────────

pub struct MprisHandle {
    state: Arc<Mutex<MprisState>>,
    changed_tx: tokio::sync::watch::Sender<()>,
}

impl MprisHandle {
    /// Push a new state snapshot; the background thread emits PropertiesChanged
    /// for any fields that actually changed.
    pub fn update(&self, new: MprisState) {
        *self.state.lock().unwrap() = new;
        self.changed_tx.send(()).ok();
    }
}

/// Spawn the MPRIS server in a background thread.
///
/// Returns:
/// - `MprisHandle` to push state updates from the GTK main thread.
/// - `std::sync::mpsc::Receiver<MprisCommand>` to be polled on the GTK main
///   thread (e.g. inside the 200 ms timer) to process incoming D-Bus commands.
pub fn spawn() -> (MprisHandle, std::sync::mpsc::Receiver<MprisCommand>) {
    let state = Arc::new(Mutex::new(MprisState::stopped()));
    let (changed_tx, changed_rx) = tokio::sync::watch::channel(());
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();

    let handle = MprisHandle {
        state: state.clone(),
        changed_tx,
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt for MPRIS");
        rt.block_on(run_server(state, changed_rx, cmd_tx));
    });

    (handle, cmd_rx)
}

// ── D-Bus server ─────────────────────────────────────────────────────────────

/// Well-known MPRIS bus name for this build.
///
/// The Flatpak sandbox only lets an app own the MPRIS name derived from its
/// app ID, so any other name is rejected without an extra `--own-name` permission.
fn bus_name(flatpak_id: Option<&str>) -> String {
    let suffix = flatpak_id.unwrap_or("aurora-media-player");
    format!("org.mpris.MediaPlayer2.{suffix}")
}

async fn run_server(
    state: Arc<Mutex<MprisState>>,
    mut changed_rx: tokio::sync::watch::Receiver<()>,
    cmd_tx: std::sync::mpsc::Sender<MprisCommand>,
) {
    let name = bus_name(std::env::var("FLATPAK_ID").ok().as_deref());
    let conn = match zbus::connection::Builder::session()
        .and_then(|b| b.name(name.as_str()))
        .and_then(|b| b.serve_at("/org/mpris/MediaPlayer2", MediaPlayer2))
        .and_then(|b| {
            b.serve_at(
                "/org/mpris/MediaPlayer2",
                MediaPlayer2Player {
                    state: state.clone(),
                    cmd_tx,
                },
            )
        }) {
        Ok(builder) => match builder.build().await {
            Ok(c) => c,
            Err(e) => {
                log::warn!("MPRIS: could not connect to D-Bus session bus: {e}");
                return;
            }
        },
        Err(e) => {
            log::warn!("MPRIS: failed to configure D-Bus connection: {e}");
            return;
        }
    };

    let mut prev = MprisState::stopped();

    loop {
        if changed_rx.changed().await.is_err() {
            break; // handle dropped → app is exiting
        }

        let current = state.lock().unwrap().clone();

        let iface = match conn
            .object_server()
            .interface::<_, MediaPlayer2Player>("/org/mpris/MediaPlayer2")
            .await
        {
            Ok(i) => i,
            Err(_) => continue,
        };

        if current.playback_status != prev.playback_status {
            iface.get().await.playback_status_changed(iface.signal_context()).await.ok();
        }
        if current.title != prev.title
            || current.artist != prev.artist
            || current.album != prev.album
            || current.art_url != prev.art_url
            || current.duration_us != prev.duration_us
        {
            iface.get().await.metadata_changed(iface.signal_context()).await.ok();
        }
        if current.can_go_next != prev.can_go_next {
            iface.get().await.can_go_next_changed(iface.signal_context()).await.ok();
        }
        if current.can_go_previous != prev.can_go_previous {
            iface.get().await.can_go_previous_changed(iface.signal_context()).await.ok();
        }
        if current.loop_status != prev.loop_status {
            iface.get().await.loop_status_changed(iface.signal_context()).await.ok();
        }
        if (current.volume - prev.volume).abs() > 0.005 {
            iface.get().await.volume_changed(iface.signal_context()).await.ok();
        }

        prev = current;
    }
}

// ── org.mpris.MediaPlayer2 ────────────────────────────────────────────────────

struct MediaPlayer2;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MediaPlayer2 {
    async fn raise(&self) {}
    async fn quit(&self) {}

    #[zbus(property)]
    async fn can_quit(&self) -> bool { false }
    #[zbus(property)]
    async fn can_raise(&self) -> bool { false }
    #[zbus(property)]
    async fn has_track_list(&self) -> bool { false }
    #[zbus(property)]
    async fn identity(&self) -> &str { "Aurora Media Player" }
    #[zbus(property)]
    async fn desktop_entry(&self) -> String {
        // Snaps install the .desktop file as "<snap-instance-name>_<desktop-file-name>".
        // MPRIS clients use this value to look up the app icon, so it must match exactly.
        if let Ok(snap_name) = std::env::var("SNAP_INSTANCE_NAME") {
            format!("{}_aurora-media-player", snap_name)
        } else {
            "io.github.daniacosta_dev.AuroraMediaPlayer".into()
        }
    }
    #[zbus(property)]
    async fn supported_uri_schemes(&self) -> Vec<String> { vec!["file".into()] }
    #[zbus(property)]
    async fn supported_mime_types(&self) -> Vec<String> { vec![] }
}

// ── org.mpris.MediaPlayer2.Player ────────────────────────────────────────────

struct MediaPlayer2Player {
    state: Arc<Mutex<MprisState>>,
    cmd_tx: std::sync::mpsc::Sender<MprisCommand>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MediaPlayer2Player {
    async fn play(&self) {
        self.cmd_tx.send(MprisCommand::PlayPause).ok();
    }
    async fn pause(&self) {
        self.cmd_tx.send(MprisCommand::PlayPause).ok();
    }
    async fn play_pause(&self) {
        self.cmd_tx.send(MprisCommand::PlayPause).ok();
    }
    async fn stop(&self) {
        self.cmd_tx.send(MprisCommand::PlayPause).ok();
    }
    async fn next(&self) {
        self.cmd_tx.send(MprisCommand::Next).ok();
    }
    async fn previous(&self) {
        self.cmd_tx.send(MprisCommand::Previous).ok();
    }
    async fn seek(&self, offset_us: i64) {
        self.cmd_tx.send(MprisCommand::Seek(offset_us)).ok();
    }

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.state.lock().unwrap().playback_status.clone()
    }
    #[zbus(property)]
    async fn loop_status(&self) -> String {
        self.state.lock().unwrap().loop_status.clone()
    }
    #[zbus(property)]
    async fn rate(&self) -> f64 { 1.0 }
    #[zbus(property)]
    async fn shuffle(&self) -> bool { false }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        let s = self.state.lock().unwrap();
        build_metadata(&s.title, &s.artist, &s.album, &s.art_url, s.duration_us)
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.state.lock().unwrap().volume
    }
    #[zbus(property)]
    async fn position(&self) -> i64 {
        self.state.lock().unwrap().position_us
    }
    #[zbus(property)]
    async fn minimum_rate(&self) -> f64 { 1.0 }
    #[zbus(property)]
    async fn maximum_rate(&self) -> f64 { 1.0 }
    #[zbus(property)]
    async fn can_go_next(&self) -> bool {
        self.state.lock().unwrap().can_go_next
    }
    #[zbus(property)]
    async fn can_go_previous(&self) -> bool {
        self.state.lock().unwrap().can_go_previous
    }
    #[zbus(property)]
    async fn can_play(&self) -> bool { true }
    #[zbus(property)]
    async fn can_pause(&self) -> bool { true }
    #[zbus(property)]
    async fn can_seek(&self) -> bool { true }
    #[zbus(property)]
    async fn can_control(&self) -> bool { true }
}

// ── Metadata builder ─────────────────────────────────────────────────────────

fn build_metadata(
    title: &str,
    artist: &str,
    album: &str,
    art_url: &str,
    duration_us: i64,
) -> HashMap<String, OwnedValue> {
    let mut map: HashMap<String, OwnedValue> = HashMap::new();

    let trackid = zbus::zvariant::ObjectPath::try_from(
        "/org/mpris/MediaPlayer2/TrackList/NoTrack",
    )
    .unwrap();
    map.insert(
        "mpris:trackid".into(),
        Value::from(trackid).try_into().unwrap(),
    );
    map.insert(
        "xesam:title".into(),
        Value::from(title).try_into().unwrap(),
    );

    if !artist.is_empty() {
        // xesam:artist is as (array of strings) per the MPRIS spec.
        let sig = zbus::zvariant::Signature::try_from("s").unwrap();
        let mut arr = zbus::zvariant::Array::new(sig);
        arr.append(Value::from(artist)).unwrap();
        map.insert(
            "xesam:artist".into(),
            Value::from(arr).try_into().unwrap(),
        );
    }

    if !album.is_empty() {
        map.insert(
            "xesam:album".into(),
            Value::from(album).try_into().unwrap(),
        );
    }

    if !art_url.is_empty() {
        map.insert(
            "mpris:artUrl".into(),
            Value::from(art_url).try_into().unwrap(),
        );
    }

    if duration_us > 0 {
        map.insert(
            "mpris:length".into(),
            Value::from(duration_us).try_into().unwrap(),
        );
    }

    map
}

#[cfg(test)]
mod tests {
    use super::bus_name;

    #[test]
    fn bus_name_outside_flatpak_keeps_default() {
        assert_eq!(bus_name(None), "org.mpris.MediaPlayer2.aurora-media-player");
    }

    #[test]
    fn bus_name_in_flatpak_uses_app_id() {
        assert_eq!(
            bus_name(Some("io.github.daniacosta_dev.AuroraMediaPlayer")),
            "org.mpris.MediaPlayer2.io.github.daniacosta_dev.AuroraMediaPlayer"
        );
    }
}
