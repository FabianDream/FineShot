use std::fs;
use std::path::PathBuf;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use winit::keyboard::KeyCode;

/// Supported image export formats.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveFormat {
    Png,
    Jpeg,
    Webp,
    Bmp,
    Tiff,
    Qoi,
    Tga,
    Gif,
}

impl SaveFormat {
    pub const ALL: [SaveFormat; 8] = [
        SaveFormat::Png,
        SaveFormat::Jpeg,
        SaveFormat::Webp,
        SaveFormat::Bmp,
        SaveFormat::Tiff,
        SaveFormat::Qoi,
        SaveFormat::Tga,
        SaveFormat::Gif,
    ];

    pub fn ext(self) -> &'static str {
        match self {
            SaveFormat::Png => "png",
            SaveFormat::Jpeg => "jpg",
            SaveFormat::Webp => "webp",
            SaveFormat::Bmp => "bmp",
            SaveFormat::Tiff => "tiff",
            SaveFormat::Qoi => "qoi",
            SaveFormat::Tga => "tga",
            SaveFormat::Gif => "gif",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            SaveFormat::Png => "PNG",
            SaveFormat::Jpeg => "JPG",
            SaveFormat::Webp => "WEBP",
            SaveFormat::Bmp => "BMP",
            SaveFormat::Tiff => "TIFF",
            SaveFormat::Qoi => "QOI",
            SaveFormat::Tga => "TGA",
            SaveFormat::Gif => "GIF",
        }
    }

    pub fn filter_name(self) -> &'static str {
        match self {
            SaveFormat::Png => "PNG Image (*.png)",
            SaveFormat::Jpeg => "JPEG Image (*.jpg, *.jpeg)",
            SaveFormat::Webp => "WebP Image (*.webp)",
            SaveFormat::Bmp => "BMP Bitmap (*.bmp)",
            SaveFormat::Tiff => "TIFF Image (*.tiff, *.tif)",
            SaveFormat::Qoi => "QOI Image (*.qoi)",
            SaveFormat::Tga => "TGA Image (*.tga)",
            SaveFormat::Gif => "GIF Image (*.gif)",
        }
    }

    pub fn filter_exts(self) -> &'static [&'static str] {
        match self {
            SaveFormat::Png => &["png"],
            SaveFormat::Jpeg => &["jpg", "jpeg"],
            SaveFormat::Webp => &["webp"],
            SaveFormat::Bmp => &["bmp"],
            SaveFormat::Tiff => &["tiff", "tif"],
            SaveFormat::Qoi => &["qoi"],
            SaveFormat::Tga => &["tga"],
            SaveFormat::Gif => &["gif"],
        }
    }

    pub fn from_ext(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "png" => Some(SaveFormat::Png),
            "jpg" | "jpeg" => Some(SaveFormat::Jpeg),
            "webp" => Some(SaveFormat::Webp),
            "bmp" => Some(SaveFormat::Bmp),
            "tif" | "tiff" => Some(SaveFormat::Tiff),
            "qoi" => Some(SaveFormat::Qoi),
            "tga" => Some(SaveFormat::Tga),
            "gif" => Some(SaveFormat::Gif),
            _ => None,
        }
    }
}

/// Persistent user configuration stored in `%APPDATA%\FineShot\config.ini`.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub hotkey: String,
    pub default_format: SaveFormat,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "PrintScreen".to_string(),
            default_format: SaveFormat::Png,
        }
    }
}

impl AppConfig {
    pub fn config_path() -> Option<PathBuf> {
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(target_os = "windows"))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));

        base.map(|dir| dir.join("FineShot").join("config.ini"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Self {
        let mut cfg = Self::default();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                match key {
                    "hotkey" if !val.is_empty() && val.parse::<HotKey>().is_ok() => {
                        cfg.hotkey = val.to_string();
                    }
                    "default_format" => {
                        if let Some(fmt) = SaveFormat::from_ext(val) {
                            cfg.default_format = fmt;
                        }
                    }
                    _ => {}
                }
            }
        }
        cfg
    }

    pub fn serialize(&self) -> String {
        format!(
            "hotkey={}\ndefault_format={}\n",
            self.hotkey,
            self.default_format.ext()
        )
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(path) = Self::config_path() else {
            return Err("Could not locate user config directory".to_string());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        fs::write(&path, self.serialize())
            .map_err(|e| format!("Failed to write config: {e}"))
    }

    pub fn parsed_hotkey(&self) -> HotKey {
        self.hotkey
            .parse::<HotKey>()
            .unwrap_or_else(|_| HotKey::new(None, Code::PrintScreen))
    }
}

/// Formats a `HotKey` into a clean human-readable label (e.g. `Ctrl + Shift + S`).
pub fn format_hotkey_label(hk: HotKey) -> String {
    let mut parts = Vec::new();
    if hk.mods.contains(Modifiers::CONTROL) {
        parts.push("Ctrl".to_string());
    }
    if hk.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift".to_string());
    }
    if hk.mods.contains(Modifiers::ALT) {
        parts.push("Alt".to_string());
    }
    if hk.mods.contains(Modifiers::META) {
        parts.push("Win".to_string());
    }
    let raw_key = hk.key.to_string();
    let nice_key = raw_key
        .strip_prefix("Key")
        .or_else(|| raw_key.strip_prefix("Digit"))
        .unwrap_or(&raw_key)
        .to_string();
    parts.push(nice_key);
    parts.join(" + ")
}

/// Converts a `winit` `KeyCode` into a `global_hotkey::hotkey::Code`,
/// returning `None` for pure modifier keys or unmapped keys.
pub fn winit_keycode_to_hotkey_code(key: KeyCode) -> Option<Code> {
    use KeyCode as K;
    Some(match key {
        K::PrintScreen => Code::PrintScreen,
        K::ScrollLock => Code::ScrollLock,
        K::Pause => Code::Pause,
        K::Insert => Code::Insert,
        K::Delete => Code::Delete,
        K::Home => Code::Home,
        K::End => Code::End,
        K::PageUp => Code::PageUp,
        K::PageDown => Code::PageDown,
        K::Space => Code::Space,
        K::Tab => Code::Tab,
        K::Enter => Code::Enter,
        K::Backspace => Code::Backspace,
        K::ArrowUp => Code::ArrowUp,
        K::ArrowDown => Code::ArrowDown,
        K::ArrowLeft => Code::ArrowLeft,
        K::ArrowRight => Code::ArrowRight,
        K::Backquote => Code::Backquote,
        K::Minus => Code::Minus,
        K::Equal => Code::Equal,
        K::BracketLeft => Code::BracketLeft,
        K::BracketRight => Code::BracketRight,
        K::Backslash => Code::Backslash,
        K::Semicolon => Code::Semicolon,
        K::Quote => Code::Quote,
        K::Comma => Code::Comma,
        K::Period => Code::Period,
        K::Slash => Code::Slash,
        K::Digit0 => Code::Digit0,
        K::Digit1 => Code::Digit1,
        K::Digit2 => Code::Digit2,
        K::Digit3 => Code::Digit3,
        K::Digit4 => Code::Digit4,
        K::Digit5 => Code::Digit5,
        K::Digit6 => Code::Digit6,
        K::Digit7 => Code::Digit7,
        K::Digit8 => Code::Digit8,
        K::Digit9 => Code::Digit9,
        K::KeyA => Code::KeyA,
        K::KeyB => Code::KeyB,
        K::KeyC => Code::KeyC,
        K::KeyD => Code::KeyD,
        K::KeyE => Code::KeyE,
        K::KeyF => Code::KeyF,
        K::KeyG => Code::KeyG,
        K::KeyH => Code::KeyH,
        K::KeyI => Code::KeyI,
        K::KeyJ => Code::KeyJ,
        K::KeyK => Code::KeyK,
        K::KeyL => Code::KeyL,
        K::KeyM => Code::KeyM,
        K::KeyN => Code::KeyN,
        K::KeyO => Code::KeyO,
        K::KeyP => Code::KeyP,
        K::KeyQ => Code::KeyQ,
        K::KeyR => Code::KeyR,
        K::KeyS => Code::KeyS,
        K::KeyT => Code::KeyT,
        K::KeyU => Code::KeyU,
        K::KeyV => Code::KeyV,
        K::KeyW => Code::KeyW,
        K::KeyX => Code::KeyX,
        K::KeyY => Code::KeyY,
        K::KeyZ => Code::KeyZ,
        K::F1 => Code::F1,
        K::F2 => Code::F2,
        K::F3 => Code::F3,
        K::F4 => Code::F4,
        K::F5 => Code::F5,
        K::F6 => Code::F6,
        K::F7 => Code::F7,
        K::F8 => Code::F8,
        K::F9 => Code::F9,
        K::F10 => Code::F10,
        K::F11 => Code::F11,
        K::F12 => Code::F12,
        K::F13 => Code::F13,
        K::F14 => Code::F14,
        K::F15 => Code::F15,
        K::F16 => Code::F16,
        K::F17 => Code::F17,
        K::F18 => Code::F18,
        K::F19 => Code::F19,
        K::F20 => Code::F20,
        K::F21 => Code::F21,
        K::F22 => Code::F22,
        K::F23 => Code::F23,
        K::F24 => Code::F24,
        K::Numpad0 => Code::Numpad0,
        K::Numpad1 => Code::Numpad1,
        K::Numpad2 => Code::Numpad2,
        K::Numpad3 => Code::Numpad3,
        K::Numpad4 => Code::Numpad4,
        K::Numpad5 => Code::Numpad5,
        K::Numpad6 => Code::Numpad6,
        K::Numpad7 => Code::Numpad7,
        K::Numpad8 => Code::Numpad8,
        K::Numpad9 => Code::Numpad9,
        K::NumpadAdd => Code::NumpadAdd,
        K::NumpadSubtract => Code::NumpadSubtract,
        K::NumpadMultiply => Code::NumpadMultiply,
        K::NumpadDivide => Code::NumpadDivide,
        K::NumpadDecimal => Code::NumpadDecimal,
        K::NumpadEnter => Code::NumpadEnter,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_preserves_hotkey_and_format() {
        let cfg = AppConfig {
            hotkey: "control+shift+KeyS".to_string(),
            default_format: SaveFormat::Jpeg,
        };
        let parsed = AppConfig::parse(&cfg.serialize());
        assert_eq!(parsed.hotkey, "control+shift+KeyS");
        assert_eq!(parsed.default_format, SaveFormat::Jpeg);
    }

    #[test]
    fn invalid_config_values_fall_back_safely() {
        let parsed = AppConfig::parse("hotkey=NotARealKey\ndefault_format=xyz\n");
        assert_eq!(parsed.hotkey, "PrintScreen");
        assert_eq!(parsed.default_format, SaveFormat::Png);
    }

    #[test]
    fn hotkey_label_formats_nicely() {
        let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS);
        assert_eq!(format_hotkey_label(hk), "Ctrl + Shift + S");
    }
}
