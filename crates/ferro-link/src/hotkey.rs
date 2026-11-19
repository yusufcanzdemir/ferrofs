//! Hotkey definitions such as `"Ctrl+B"` or `"Ctrl+Shift+F5"`.

use std::fmt;
use std::str::FromStr;

/// Modifier bits.
pub const CTRL: u8 = 1;
pub const SHIFT: u8 = 2;
pub const ALT: u8 = 4;
pub const WIN: u8 = 8;

/// A key plus an exact set of modifiers. Packs into a `u32` so the keyboard hook can
/// read it from an atomic without locking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    /// Win32 virtual-key code.
    pub vk: u8,
    /// Combination of [`CTRL`], [`SHIFT`], [`ALT`], [`WIN`].
    pub mods: u8,
}

impl Hotkey {
    pub const fn pack(self) -> u32 {
        ((self.mods as u32) << 8) | self.vk as u32
    }

    pub const fn unpack(packed: u32) -> Self {
        Self {
            vk: (packed & 0xFF) as u8,
            mods: ((packed >> 8) & 0xFF) as u8,
        }
    }
}

/// Error for a hotkey string that cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseHotkeyError(String);

impl fmt::Display for ParseHotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseHotkeyError {}

impl FromStr for Hotkey {
    type Err = ParseHotkeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |msg: &str| ParseHotkeyError(format!("invalid hotkey {s:?}: {msg}"));
        let mut mods = 0u8;
        let mut vk = None;
        for token in s.split('+').map(str::trim) {
            let upper = token.to_ascii_uppercase();
            let bit = match upper.as_str() {
                "CTRL" | "CONTROL" => CTRL,
                "SHIFT" => SHIFT,
                "ALT" => ALT,
                "WIN" | "WINDOWS" => WIN,
                _ => {
                    if vk.is_some() {
                        return Err(err("more than one key"));
                    }
                    vk = Some(key_code(&upper).ok_or_else(|| err("unknown key"))?);
                    continue;
                }
            };
            if mods & bit != 0 {
                return Err(err("repeated modifier"));
            }
            mods |= bit;
        }
        let vk = vk.ok_or_else(|| err("missing key"))?;
        // A bare key or Shift+key would hijack normal typing in Explorer.
        if mods & (CTRL | ALT | WIN) == 0 {
            return Err(err("needs Ctrl, Alt or Win"));
        }
        Ok(Self { vk, mods })
    }
}

/// `A`–`Z`, `0`–`9`, `F1`–`F24`.
fn key_code(key: &str) -> Option<u8> {
    let bytes = key.as_bytes();
    match bytes {
        [c] if c.is_ascii_uppercase() || c.is_ascii_digit() => Some(*c),
        [b'F', rest @ ..] if !rest.is_empty() => {
            let n: u8 = key[1..].parse().ok()?;
            (1..=24).contains(&n).then(|| 0x70 + n - 1)
        }
        _ => None,
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, name) in [
            (CTRL, "Ctrl+"),
            (SHIFT, "Shift+"),
            (ALT, "Alt+"),
            (WIN, "Win+"),
        ] {
            if self.mods & bit != 0 {
                f.write_str(name)?;
            }
        }
        match self.vk {
            vk @ 0x70..=0x87 => write!(f, "F{}", vk - 0x70 + 1),
            vk => write!(f, "{}", char::from(vk)),
        }
    }
}

impl TryFrom<String> for Hotkey {
    type Error = ParseHotkeyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Hotkey> for String {
    fn from(hotkey: Hotkey) -> Self {
        hotkey.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays() {
        let key: Hotkey = "ctrl + shift + b".parse().unwrap();
        assert_eq!(
            key,
            Hotkey {
                vk: b'B',
                mods: CTRL | SHIFT
            }
        );
        assert_eq!(key.to_string(), "Ctrl+Shift+B");
        assert_eq!("Win+F5".parse::<Hotkey>().unwrap().to_string(), "Win+F5");
        assert_eq!(
            "Alt+7".parse::<Hotkey>().unwrap(),
            Hotkey {
                vk: b'7',
                mods: ALT
            }
        );
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "B",
            "Shift+B",
            "Ctrl",
            "Ctrl+B+C",
            "Ctrl+Ctrl+B",
            "Ctrl+F25",
            "Ctrl+Space",
        ] {
            assert!(bad.parse::<Hotkey>().is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn pack_roundtrip() {
        let key = Hotkey {
            vk: 0x87,
            mods: CTRL | ALT | WIN,
        };
        assert_eq!(Hotkey::unpack(key.pack()), key);
    }
}
