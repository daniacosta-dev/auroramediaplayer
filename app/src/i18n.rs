/// Minimal runtime i18n — returns `&'static str` for every key so results
/// can be passed directly to GTK widget builders.
///
/// English is the default; each additional language is a match arm in its own
/// function.  Unknown keys fall back to the English string (the key itself).
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Lang {
    #[default]
    En,
    Es,
}

impl Lang {
    pub fn from_code(code: &str) -> Self {
        match code {
            "es" => Self::Es,
            _    => Self::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Es => "es",
        }
    }
}

thread_local! {
    static LANG: Cell<Lang> = Cell::new(Lang::En);
}

pub fn set(lang: Lang) {
    LANG.with(|l| l.set(lang));
}

pub fn current() -> Lang {
    LANG.with(|l| l.get())
}

/// Translate `key` (which is the English string) into the current language.
/// Falls back to the key itself when a translation is missing.
pub fn t(key: &'static str) -> &'static str {
    match current() {
        Lang::En => key,
        Lang::Es => es(key).unwrap_or(key),
    }
}

fn es(key: &'static str) -> Option<&'static str> {
    Some(match key {
        // ── Settings dialog ──────────────────────────────────────────────
        "Settings"                    => "Ajustes",
        "Interface language"          => "Idioma de la interfaz",
        "If you like Aurora Media Player, consider"
                                      => "Si te gusta Aurora Media Player, considera",
        "⭐ starring it on GitHub"    => "⭐ darle una estrella en GitHub",
        "Appearance"                  => "Apariencia",
        "Theme"                       => "Tema",
        "System"                      => "Sistema",
        "Light"                       => "Claro",
        "Dark"                        => "Oscuro",
        "Keyboard Shortcuts"          => "Atajos de teclado",
        "Playback"                    => "Reproducción",
        "Play / Pause"                => "Reproducir / Pausar",
        "Next track"                  => "Pista siguiente",
        "Previous track"              => "Pista anterior",
        "Mute"                        => "Silenciar",
        "Screenshot"                  => "Captura de pantalla",
        "Seek & Volume"               => "Navegación y volumen",
        "Seek −5 s"                   => "Retroceder 5 s",
        "Seek +5 s"                   => "Avanzar 5 s",
        "Seek −30 s"                  => "Retroceder 30 s",
        "Seek +30 s"                  => "Avanzar 30 s",
        "Volume up"                   => "Subir volumen",
        "Volume down"                 => "Bajar volumen",
        "Speed & Video"               => "Velocidad y vídeo",
        "Speed up"                    => "Aumentar velocidad",
        "Speed down"                  => "Reducir velocidad",
        "Reset speed"                 => "Restablecer velocidad",
        "Fullscreen"                  => "Pantalla completa",
        "Exit fullscreen"             => "Salir de pantalla completa",
        "App"                         => "App",
        "Open file"                   => "Abrir archivo",
        "Open URL"                    => "Abrir URL",
        "Load subtitle"               => "Cargar subtítulo",
        "Control bar"                 => "Barra de controles",
        "Control bar style"           => "Estilo de barra",
        "Floating"                    => "Flotante",
        "Fixed"                       => "Fija",
        "Modern"                      => "Moderno",
        "Language"                    => "Idioma",
        "English"                     => "Inglés",
        "Spanish"                     => "Español",
        "Restart to apply"            => "Reiniciar para aplicar",
        "Custom"                      => "Personalizado",
        "Accent color"                => "Color de acento",
        "Accent Color"                => "Color de acento",
        "Background color"            => "Color de fondo",
        "Background Color"            => "Color de fondo",
        "Text/Icon"                   => "Texto/Icono",
        "Text/Icon Color"             => "Color de texto e icono",
        "Reset to system default"     => "Restablecer al predeterminado",
        "System default"              => "Predeterminado del sistema",
        // ── File menu ────────────────────────────────────────────────────
        "File"                        => "Archivo",
        "Open File…"                  => "Abrir archivo…",
        "Open URL or Playlist…"       => "Abrir URL o lista…",
        "Load Subtitle File…"         => "Cargar subtítulos…",
        "Recent Files"                => "Archivos recientes",
        "No recent files"             => "Sin archivos recientes",
        "Open Screenshot Folder"      => "Abrir carpeta de capturas",
        "Report Issue"                => "Reportar problema",
        "Close"                       => "Cerrar",
        "Remove from recents"         => "Quitar de recientes",
        "Open Media File"             => "Abrir archivo multimedia",
        "Open Subtitle File"          => "Abrir archivo de subtítulos",
        // ── Video area ────────────────────────────────────────────────────
        "Open a file to start playing" => "Abre un archivo para reproducir",
        // ── Header / window ───────────────────────────────────────────────
        "Loading…"                    => "Cargando…",
        // ── Controls tooltips ─────────────────────────────────────────────
        "Podcast mode — audio only (saves bandwidth)"
                                      => "Modo podcast — solo audio (ahorra datos)",
        "Take screenshot"             => "Tomar captura",
        "Audio & Subtitle tracks"     => "Pistas de audio y subtítulos",
        "Playback speed"              => "Velocidad de reproducción",
        "Shuffle"                     => "Aleatorio",
        // ── Tracks popover ────────────────────────────────────────────────
        "Audio"                       => "Audio",
        "Subtitles"                   => "Subtítulos",
        "Disabled"                    => "Desactivado",
        // ── Playlist panel ────────────────────────────────────────────────
        "Playlist"                    => "Lista de reproducción",
        "Drop files or folders here"  => "Suelta archivos o carpetas aquí",
        // ── URL playlist dialog ───────────────────────────────────────────
        "URL Playlist"                => "Lista de URLs",
        "Play"                        => "Reproducir",
        "Add URL"                     => "Agregar URL",
        "Save as playlist…"           => "Guardar como lista…",
        "Save"                        => "Guardar",
        "Save playlist"               => "Guardar lista",
        "Edit"                        => "Editar",
        "Delete"                      => "Eliminar",
        "Remove"                      => "Quitar",
        // ── Toast messages ────────────────────────────────────────────────
        "Screenshot saved"            => "Captura guardada",
        _ => return None,
    })
}
