use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModifierMask(u8);

impl ModifierMask {
    pub const CTRL: Self = Self(1 << 0);
    pub const ALT: Self = Self(1 << 1);
    pub const SHIFT: Self = Self(1 << 2);
    pub const META: Self = Self(1 << 3);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn from_bits(bits: u8) -> Self {
        Self(bits & 0x0f)
    }
}

impl std::ops::BitOrAssign for ModifierMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HotkeyKey {
    Hid(u16),
}

impl HotkeyKey {
    pub fn usage(self) -> u16 {
        match self {
            Self::Hid(usage) => usage,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: ModifierMask,
    pub key: HotkeyKey,
}

impl Hotkey {
    pub const DEFAULT: &'static str = "ctrl+alt+shift+esc";

    pub fn matches(self, usage: u16, modifiers: ModifierMask) -> bool {
        self.key.usage() == usage && self.modifiers == modifiers
    }
}

impl FromStr for Hotkey {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut modifiers = ModifierMask::default();
        let mut key = None;
        for raw in value.split('+') {
            let part = raw.trim().to_ascii_lowercase();
            if part.is_empty() {
                bail!("热键包含空白按键");
            }
            let modifier = match part.as_str() {
                "ctrl" | "control" => Some(ModifierMask::CTRL),
                "alt" | "option" => Some(ModifierMask::ALT),
                "shift" => Some(ModifierMask::SHIFT),
                "meta" | "cmd" | "command" | "win" | "super" => {
                    Some(ModifierMask::META)
                }
                _ => None,
            };
            if let Some(modifier) = modifier {
                if modifiers.contains(modifier) {
                    bail!("热键包含重复修饰键 `{part}`");
                }
                modifiers |= modifier;
                continue;
            }
            if key.is_some() {
                bail!("热键只能包含一个主键");
            }
            key = Some(HotkeyKey::Hid(parse_named_key(&part)?));
        }
        if modifiers.bits() == 0 {
            bail!("热键至少需要一个修饰键");
        }
        Ok(Self {
            modifiers,
            key: key.ok_or_else(|| anyhow::anyhow!("热键缺少主键"))?,
        })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.modifiers.contains(ModifierMask::CTRL) {
            parts.push("ctrl".to_string());
        }
        if self.modifiers.contains(ModifierMask::ALT) {
            parts.push("alt".to_string());
        }
        if self.modifiers.contains(ModifierMask::SHIFT) {
            parts.push("shift".to_string());
        }
        if self.modifiers.contains(ModifierMask::META) {
            parts.push("meta".to_string());
        }
        parts.push(key_name(self.key.usage()));
        write!(f, "{}", parts.join("+"))
    }
}

fn parse_named_key(value: &str) -> Result<u16> {
    if value.len() == 1 {
        let byte = value.as_bytes()[0];
        if byte.is_ascii_alphabetic() {
            return Ok(0x04 + u16::from(byte.to_ascii_lowercase() - b'a'));
        }
        if byte.is_ascii_digit() {
            return Ok(if byte == b'0' {
                0x27
            } else {
                0x1e + u16::from(byte - b'1')
            });
        }
    }
    let usage = match value {
        "esc" | "escape" => 0x29,
        "enter" | "return" => 0x28,
        "tab" => 0x2b,
        "space" => 0x2c,
        "backspace" => 0x2a,
        "delete" => 0x4c,
        "insert" => 0x49,
        "home" => 0x4a,
        "end" => 0x4d,
        "pageup" => 0x4b,
        "pagedown" => 0x4e,
        "right" => 0x4f,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        name if name.starts_with('f') => {
            let number = name[1..].parse::<u16>().map_err(|_| {
                anyhow::anyhow!("不支持的热键主键 `{value}`")
            })?;
            if !(1..=24).contains(&number) {
                bail!("功能键范围必须是 F1 到 F24");
            }
            if number <= 12 {
                0x3a + number - 1
            } else {
                0x68 + number - 13
            }
        }
        _ => bail!("不支持的热键主键 `{value}`"),
    };
    Ok(usage)
}

pub(super) fn key_name(usage: u16) -> String {
    match usage {
        0x04..=0x1d => char::from(b'a' + (usage - 0x04) as u8).to_string(),
        0x1e..=0x26 => char::from(b'1' + (usage - 0x1e) as u8).to_string(),
        0x27 => "0".to_string(),
        0x28 => "enter".to_string(),
        0x29 => "esc".to_string(),
        0x2a => "backspace".to_string(),
        0x2b => "tab".to_string(),
        0x2c => "space".to_string(),
        0x2d => "minus".to_string(),
        0x2e => "equal".to_string(),
        0x2f => "left_bracket".to_string(),
        0x30 => "right_bracket".to_string(),
        0x31 => "backslash".to_string(),
        0x33 => "semicolon".to_string(),
        0x34 => "apostrophe".to_string(),
        0x35 => "grave".to_string(),
        0x36 => "comma".to_string(),
        0x37 => "period".to_string(),
        0x38 => "slash".to_string(),
        0x39 => "caps_lock".to_string(),
        0x3a..=0x45 => format!("f{}", usage - 0x3a + 1),
        0x49 => "insert".to_string(),
        0x4a => "home".to_string(),
        0x4b => "pageup".to_string(),
        0x4c => "delete".to_string(),
        0x4d => "end".to_string(),
        0x4e => "pagedown".to_string(),
        0x4f => "right".to_string(),
        0x50 => "left".to_string(),
        0x51 => "down".to_string(),
        0x52 => "up".to_string(),
        0xe0 => "left_ctrl".to_string(),
        0xe1 => "left_shift".to_string(),
        0xe2 => "left_option".to_string(),
        0xe3 => "left_command".to_string(),
        0xe4 => "right_ctrl".to_string(),
        0xe5 => "right_shift".to_string(),
        0xe6 => "right_option".to_string(),
        0xe7 => "right_command".to_string(),
        0x68..=0x73 => format!("f{}", usage - 0x68 + 13),
        _ => format!("hid-{usage}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Hotkey, HotkeyKey, ModifierMask};
    use std::str::FromStr;

    #[test]
    fn parses_and_normalizes_hotkey() {
        let hotkey = Hotkey::from_str("Shift+CTRL+Alt+Escape").unwrap();
        assert_eq!(hotkey.key, HotkeyKey::Hid(0x29));
        assert_eq!(hotkey.to_string(), "ctrl+alt+shift+esc");
        assert!(hotkey.matches(0x29, ModifierMask::from_bits(0x07)));
    }

    #[test]
    fn rejects_ambiguous_hotkeys() {
        assert!(Hotkey::from_str("esc").is_err());
        assert!(Hotkey::from_str("ctrl+alt").is_err());
        assert!(Hotkey::from_str("ctrl+a+b").is_err());
        assert!(Hotkey::from_str("ctrl+ctrl+a").is_err());
    }
}
