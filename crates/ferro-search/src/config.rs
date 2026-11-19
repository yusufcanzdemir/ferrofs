//! The `[search]` section of the shared `%APPDATA%\ferrofs\config.toml`.
//! Other sections belong to other ferrofs apps and are ignored here.

use std::path::PathBuf;

use serde::Deserialize;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};

pub const DEFAULT_HOTKEY: &str = "Ctrl+Alt+Space";

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    search: Section,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Section {
    hotkey: String,
}

impl Default for Section {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.into(),
        }
    }
}

/// A global hotkey in `RegisterHotKey` terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

impl Hotkey {
    /// Parses `"Ctrl+Alt+Space"`, `"Win+Shift+F"`, `"Alt+F12"`…
    pub fn parse(text: &str) -> Result<Self, String> {
        let err = |msg: &str| format!("invalid hotkey {text:?}: {msg}");
        let mut modifiers = HOT_KEY_MODIFIERS(0);
        let mut vk = None;
        for token in text.split('+').map(str::trim) {
            let upper = token.to_ascii_uppercase();
            let modifier = match upper.as_str() {
                "CTRL" | "CONTROL" => MOD_CONTROL,
                "ALT" => MOD_ALT,
                "SHIFT" => MOD_SHIFT,
                "WIN" | "WINDOWS" => MOD_WIN,
                _ => {
                    if vk
                        .replace(key_code(&upper).ok_or_else(|| err("unknown key"))?)
                        .is_some()
                    {
                        return Err(err("more than one key"));
                    }
                    continue;
                }
            };
            modifiers |= modifier;
        }
        let vk = vk.ok_or_else(|| err("missing key"))?;
        if (modifiers & (MOD_CONTROL | MOD_ALT | MOD_WIN)).0 == 0 {
            return Err(err("needs Ctrl, Alt or Win"));
        }
        Ok(Self { modifiers, vk })
    }
}

fn key_code(key: &str) -> Option<u32> {
    match key.as_bytes() {
        [c] if c.is_ascii_uppercase() || c.is_ascii_digit() => Some(u32::from(*c)),
        _ if key == "SPACE" => Some(0x20),
        [b'F', ..] => {
            let n: u32 = key[1..].parse().ok()?;
            (1..=24).contains(&n).then(|| 0x70 + n - 1)
        }
        _ => None,
    }
}

fn config_file() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("ferrofs").join("config.toml"))
}

/// The configured hotkey, or the default (with a logged reason) when the file is missing
/// or invalid.
pub fn load_hotkey() -> Hotkey {
    let text = config_file().and_then(|f| std::fs::read_to_string(f).ok());
    let configured = match text.as_deref().map(toml::from_str::<File>) {
        None => DEFAULT_HOTKEY.to_owned(),
        Some(Ok(file)) => file.search.hotkey,
        Some(Err(err)) => {
            tracing::warn!("invalid config.toml, using the default search hotkey: {err}");
            DEFAULT_HOTKEY.to_owned()
        }
    };
    Hotkey::parse(&configured).unwrap_or_else(|err| {
        tracing::warn!("{err}; using {DEFAULT_HOTKEY}");
        Hotkey::parse(DEFAULT_HOTKEY).unwrap_or(Hotkey {
            modifiers: MOD_CONTROL | MOD_ALT,
            vk: 0x20,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        assert_eq!(
            Hotkey::parse("ctrl + alt + space").unwrap(),
            Hotkey {
                modifiers: MOD_CONTROL | MOD_ALT,
                vk: 0x20
            }
        );
        assert_eq!(Hotkey::parse("Win+Shift+F").unwrap().vk, u32::from(b'F'));
        assert_eq!(Hotkey::parse("Alt+F12").unwrap().vk, 0x7B);
        for bad in ["Space", "Shift+A", "Ctrl", "Ctrl+A+B", "Ctrl+Enter"] {
            assert!(Hotkey::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_only_the_search_section() {
        let file: File = toml::from_str(
            "[hotkeys]\npick = \"Ctrl+B\"\n[link]\nfile_mode = \"symlink\"\n[search]\nhotkey = \"Win+S\"\n",
        )
        .unwrap();
        assert_eq!(file.search.hotkey, "Win+S");
        let empty: File = toml::from_str("[hotkeys]\npick = \"Ctrl+B\"\n").unwrap();
        assert_eq!(empty.search.hotkey, DEFAULT_HOTKEY);
    }
}
