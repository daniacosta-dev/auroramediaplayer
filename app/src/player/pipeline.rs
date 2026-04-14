#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum RepeatMode {
    #[default]
    None,
    Playlist,
    One,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackState {
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum PlayerCommand {
    Open(std::path::PathBuf),
    OpenUrl(String),
    Play,
    Pause,
    TogglePause,
    Stop,
    Seek(f64),           // seconds
    SetVolume(f64),      // 0.0 - 100.0
    Mute(bool),
    SetSpeed(f64),       // 0.25 - 4.0
    NextFrame,
    PrevFrame,
    Screenshot,
    ScreenshotToFile(std::path::PathBuf),
    SetRepeat(RepeatMode),
    SetAudioTrack(i64),
    SetSubtitleTrack(i64),   // 0 means disable
    AddSubtitle(std::path::PathBuf),
    SetVideoEnabled(bool),
    /// Change streaming quality: set ytdl-format and reload the URL from start_pos.
    SetQuality { format: String, url: String, start_pos: f64 },
}
