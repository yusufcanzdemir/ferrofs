//! `%APPDATA%\ferrofs\config.toml` (SPEC §1.5).

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::hotkey::Hotkey;
use crate::link::{LinkMode, Modes};

/// Written when no config file exists, so users can find and edit it.
/// Must parse to `Config::default()` (checked by a test).
pub const DEFAULT_TEXT: &str = r#"# ferro-link configuration. Use "Reload config" in the tray menu after editing.

[hotkeys]
# Remember the files/folders selected in Explorer.
pick = "Ctrl+B"
# Create links in the current Explorer folder. Add Shift to choose the link type.
drop = "Ctrl+G"

[link]
file_mode = "symlink"   # symlink | hardlink
dir_mode  = "junction"  # junction | symlink
# Symlinks store a path relative to the link's folder (survives moving both together).
relative_symlinks = false
"#;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub link: LinkConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hotkeys {
    #[serde(deserialize_with = "hotkey_from_str")]
    pub pick: Hotkey,
    #[serde(deserialize_with = "hotkey_from_str")]
    pub drop: Hotkey,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            pick: Hotkey {
                vk: b'B',
                mods: crate::hotkey::CTRL,
            },
            drop: Hotkey {
                vk: b'G',
                mods: crate::hotkey::CTRL,
            },
        }
    }
}

fn hotkey_from_str<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Hotkey, D::Error> {
    let text = String::deserialize(de)?;
    text.parse().map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinkConfig {
    pub file_mode: FileMode,
    pub dir_mode: DirMode,
    pub relative_symlinks: bool,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            file_mode: FileMode::Symlink,
            dir_mode: DirMode::Junction,
            relative_symlinks: false,
        }
    }
}

impl LinkConfig {
    pub fn modes(self) -> Modes {
        Modes {
            file: match self.file_mode {
                FileMode::Symlink => LinkMode::Symlink,
                FileMode::Hardlink => LinkMode::Hardlink,
            },
            dir: match self.dir_mode {
                DirMode::Junction => LinkMode::Junction,
                DirMode::Symlink => LinkMode::Symlink,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    Symlink,
    Hardlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DirMode {
    Junction,
    Symlink,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        anyhow::ensure!(
            config.hotkeys.pick != config.hotkeys.drop,
            "pick and drop hotkeys must differ"
        );
        Ok(config)
    }
}

/// Loads the config, creating the file with defaults if it does not exist.
pub fn load_or_create(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            std::fs::write(path, DEFAULT_TEXT)
                .with_context(|| format!("cannot write {}", path.display()))?;
            tracing::info!(path = %path.display(), "created default config");
            Ok(Config::default())
        }
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{ALT, CTRL};

    #[test]
    fn default_text_matches_default_config() {
        assert_eq!(Config::parse(DEFAULT_TEXT).unwrap(), Config::default());
    }

    #[test]
    fn empty_file_uses_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn parses_custom_values() {
        let config = Config::parse(
            r#"
            [hotkeys]
            pick = "Ctrl+Alt+P"
            [link]
            file_mode = "hardlink"
            dir_mode = "symlink"
            relative_symlinks = true
            "#,
        )
        .unwrap();
        assert_eq!(
            config.hotkeys.pick,
            Hotkey {
                vk: b'P',
                mods: CTRL | ALT
            }
        );
        assert_eq!(config.hotkeys.drop, Hotkeys::default().drop);
        assert_eq!(
            config.link.modes(),
            Modes {
                file: LinkMode::Hardlink,
                dir: LinkMode::Symlink
            }
        );
        assert!(config.link.relative_symlinks);
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(Config::parse("[link]\nfile_mode = \"junction\"").is_err());
        assert!(Config::parse("[hotkeys]\npick = \"B\"").is_err());
        assert!(Config::parse("[hotkeys]\npick = \"Ctrl+G\"").is_err()); // same as drop
        assert!(Config::parse("[unknown]\nx = 1").is_err());
    }

    #[test]
    fn creates_default_file_when_missing() {
        let dir = std::env::temp_dir().join(format!("ferrofs-config-{}", std::process::id()));
        let path = dir.join("nested").join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_or_create(&path).unwrap(), Config::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_TEXT);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
