use super::{KeySnapshot, ModifierMask};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputPlatform {
    Macos,
    Windows,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyMappingConfig {
    pub macos_to_windows: BTreeMap<String, String>,
    pub windows_to_macos: BTreeMap<String, String>,
}

impl Default for KeyMappingConfig {
    fn default() -> Self {
        Self {
            macos_to_windows: BTreeMap::from([
                ("left_option".to_string(), "left_win".to_string()),
                ("right_option".to_string(), "right_win".to_string()),
                ("left_command".to_string(), "left_alt".to_string()),
                ("right_command".to_string(), "right_alt".to_string()),
            ]),
            windows_to_macos: BTreeMap::from([
                ("left_win".to_string(), "left_option".to_string()),
                ("right_win".to_string(), "right_option".to_string()),
                ("left_alt".to_string(), "left_command".to_string()),
                ("right_alt".to_string(), "right_command".to_string()),
            ]),
        }
    }
}

impl InputPlatform {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(windows)]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            Self::Macos
        }
    }
}

#[derive(Debug)]
pub(super) struct KeyMapper {
    mapping: BTreeMap<u16, u16>,
    pressed_sources: BTreeMap<u16, u16>,
    target_counts: BTreeMap<u16, usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MappedKey {
    pub usage: u16,
    pub modifiers: ModifierMask,
    pub down: bool,
    pub repeat: bool,
}

impl KeyMapper {
    pub fn new(
        config: &KeyMappingConfig,
        local: InputPlatform,
        remote: InputPlatform,
    ) -> Result<Self> {
        let mapping = if local == remote {
            BTreeMap::new()
        } else {
            compile_direction(config, local, remote)?
        };
        Ok(Self {
            mapping,
            pressed_sources: BTreeMap::new(),
            target_counts: BTreeMap::new(),
        })
    }

    pub fn map_snapshot(&mut self, snapshot: &KeySnapshot) -> KeySnapshot {
        self.clear();
        for source in &snapshot.usages {
            let target = self.map_usage(*source);
            self.pressed_sources.insert(*source, target);
            *self.target_counts.entry(target).or_default() += 1;
        }
        KeySnapshot {
            usages: self.target_counts.keys().copied().collect(),
            modifiers: modifiers_from_usages(self.target_counts.keys().copied()),
            buttons: snapshot.buttons.clone(),
        }
    }

    pub fn map_key(
        &mut self,
        usage: u16,
        down: bool,
        repeat: bool,
    ) -> Option<MappedKey> {
        if repeat {
            let target = self
                .pressed_sources
                .get(&usage)
                .copied()
                .unwrap_or_else(|| self.map_usage(usage));
            return Some(MappedKey {
                usage: target,
                modifiers: self.modifiers(),
                down,
                repeat,
            });
        }

        let target = if down {
            if self.pressed_sources.contains_key(&usage) {
                return None;
            }
            let target = self.map_usage(usage);
            self.pressed_sources.insert(usage, target);
            let count = self.target_counts.entry(target).or_default();
            *count += 1;
            if *count > 1 {
                return None;
            }
            target
        } else {
            let target = self.pressed_sources.remove(&usage)?;
            let count = self.target_counts.get_mut(&target)?;
            *count = count.saturating_sub(1);
            if *count > 0 {
                return None;
            }
            self.target_counts.remove(&target);
            target
        };

        Some(MappedKey {
            usage: target,
            modifiers: self.modifiers(),
            down,
            repeat,
        })
    }

    pub fn clear(&mut self) {
        self.pressed_sources.clear();
        self.target_counts.clear();
    }

    fn map_usage(&self, usage: u16) -> u16 {
        self.mapping.get(&usage).copied().unwrap_or(usage)
    }

    fn modifiers(&self) -> ModifierMask {
        modifiers_from_usages(self.target_counts.keys().copied())
    }
}

pub fn validate_key_mapping(config: &KeyMappingConfig) -> Result<()> {
    compile_direction(
        config,
        InputPlatform::Macos,
        InputPlatform::Windows,
    )?;
    compile_direction(
        config,
        InputPlatform::Windows,
        InputPlatform::Macos,
    )?;
    Ok(())
}

fn compile_direction(
    config: &KeyMappingConfig,
    source_platform: InputPlatform,
    target_platform: InputPlatform,
) -> Result<BTreeMap<u16, u16>> {
    let entries = match (source_platform, target_platform) {
        (InputPlatform::Macos, InputPlatform::Windows) => &config.macos_to_windows,
        (InputPlatform::Windows, InputPlatform::Macos) => &config.windows_to_macos,
        _ => return Ok(BTreeMap::new()),
    };
    let mut compiled = BTreeMap::new();
    for (source_name, target_name) in entries {
        let source = parse_key_name(source_platform, source_name).ok_or_else(|| {
            anyhow::anyhow!(
                "input key mapping source `{source_name}` is not supported on {source_platform:?}"
            )
        })?;
        let target = parse_key_name(target_platform, target_name).ok_or_else(|| {
            anyhow::anyhow!(
                "input key mapping target `{target_name}` is not supported on {target_platform:?}"
            )
        })?;
        compiled.insert(source, target);
    }
    Ok(compiled)
}

fn parse_key_name(platform: InputPlatform, name: &str) -> Option<u16> {
    let usage = parse_common_key_name(name).or_else(|| parse_modifier_name(platform, name))?;
    platform_supports_usage(platform, usage).then_some(usage)
}

fn parse_common_key_name(name: &str) -> Option<u16> {
    if name.len() == 1 {
        let byte = name.as_bytes()[0];
        if byte.is_ascii_lowercase() {
            return Some(0x04 + u16::from(byte - b'a'));
        }
        if (b'1'..=b'9').contains(&byte) {
            return Some(0x1e + u16::from(byte - b'1'));
        }
        if byte == b'0' {
            return Some(0x27);
        }
    }
    if let Some(number) = name.strip_prefix('f').and_then(|value| value.parse::<u16>().ok())
        && (1..=15).contains(&number)
    {
        return Some(0x3a + number - 1);
    }
    Some(match name {
        "enter" => 0x28,
        "escape" => 0x29,
        "backspace" => 0x2a,
        "tab" => 0x2b,
        "space" => 0x2c,
        "minus" => 0x2d,
        "equal" => 0x2e,
        "left_bracket" => 0x2f,
        "right_bracket" => 0x30,
        "backslash" => 0x31,
        "semicolon" => 0x33,
        "apostrophe" => 0x34,
        "grave" => 0x35,
        "comma" => 0x36,
        "period" => 0x37,
        "slash" => 0x38,
        "caps_lock" => 0x39,
        "insert" => 0x49,
        "home" => 0x4a,
        "page_up" => 0x4b,
        "delete" => 0x4c,
        "end" => 0x4d,
        "page_down" => 0x4e,
        "right" => 0x4f,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        _ => return None,
    })
}

fn parse_modifier_name(platform: InputPlatform, name: &str) -> Option<u16> {
    Some(match (platform, name) {
        (_, "left_ctrl") => 0xe0,
        (_, "left_shift") => 0xe1,
        (InputPlatform::Macos, "left_option")
        | (InputPlatform::Windows, "left_alt") => 0xe2,
        (InputPlatform::Macos, "left_command")
        | (InputPlatform::Windows, "left_win") => 0xe3,
        (_, "right_ctrl") => 0xe4,
        (_, "right_shift") => 0xe5,
        (InputPlatform::Macos, "right_option")
        | (InputPlatform::Windows, "right_alt") => 0xe6,
        (InputPlatform::Macos, "right_command")
        | (InputPlatform::Windows, "right_win") => 0xe7,
        _ => return None,
    })
}

fn platform_supports_usage(platform: InputPlatform, usage: u16) -> bool {
    match platform {
        InputPlatform::Macos => matches!(
            usage,
            0x04..=0x31 | 0x33..=0x34 | 0x36..=0x52 | 0xe0..=0xe7
        ),
        InputPlatform::Windows => matches!(
            usage,
            0x04..=0x31 | 0x33..=0x45 | 0x49..=0x52 | 0xe0..=0xe7
        ),
    }
}

fn modifiers_from_usages(usages: impl Iterator<Item = u16>) -> ModifierMask {
    let mut bits = 0;
    for usage in usages {
        match usage {
            0xe0 | 0xe4 => bits |= ModifierMask::CTRL.bits(),
            0xe1 | 0xe5 => bits |= ModifierMask::SHIFT.bits(),
            0xe2 | 0xe6 => bits |= ModifierMask::ALT.bits(),
            0xe3 | 0xe7 => bits |= ModifierMask::META.bits(),
            _ => {}
        }
    }
    ModifierMask::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mapping_swaps_option_win_and_command_alt() {
        let config = KeyMappingConfig::default();
        let mut mac = KeyMapper::new(
            &config,
            InputPlatform::Macos,
            InputPlatform::Windows,
        )
        .unwrap();
        let mapped = mac.map_snapshot(&KeySnapshot {
            usages: vec![0xe2, 0xe3],
            modifiers: ModifierMask::from_bits(
                ModifierMask::ALT.bits() | ModifierMask::META.bits(),
            ),
            buttons: Vec::new(),
        });
        assert_eq!(mapped.usages, vec![0xe2, 0xe3]);
        assert_eq!(
            mapped.modifiers,
            ModifierMask::from_bits(ModifierMask::ALT.bits() | ModifierMask::META.bits())
        );

        let mut windows = KeyMapper::new(
            &config,
            InputPlatform::Windows,
            InputPlatform::Macos,
        )
        .unwrap();
        assert_eq!(windows.map_key(0xe3, true, false).unwrap().usage, 0xe2);
        assert_eq!(windows.map_key(0xe3, false, false).unwrap().usage, 0xe2);
        assert_eq!(windows.map_key(0xe2, true, false).unwrap().usage, 0xe3);
    }

    #[test]
    fn same_platform_keeps_native_usages() {
        let config = KeyMappingConfig::default();
        let mut mapper = KeyMapper::new(
            &config,
            InputPlatform::Macos,
            InputPlatform::Macos,
        )
        .unwrap();
        assert_eq!(mapper.map_key(0xe3, true, false).unwrap().usage, 0xe3);
    }

    #[test]
    fn custom_ordinary_key_mapping_is_applied_once() {
        let mut config = KeyMappingConfig::default();
        config.macos_to_windows.clear();
        config
            .macos_to_windows
            .insert("a".to_string(), "b".to_string());
        let mut mapper = KeyMapper::new(
            &config,
            InputPlatform::Macos,
            InputPlatform::Windows,
        )
        .unwrap();
        assert_eq!(mapper.map_key(0x04, true, false).unwrap().usage, 0x05);
        assert!(mapper.map_key(0x05, true, false).is_none());
        assert!(mapper.map_key(0x04, false, false).is_none());
        assert_eq!(mapper.map_key(0x05, false, false).unwrap().usage, 0x05);
    }

    #[test]
    fn invalid_names_are_rejected() {
        let mut config = KeyMappingConfig::default();
        config
            .macos_to_windows
            .insert("unknown".to_string(), "a".to_string());
        assert!(validate_key_mapping(&config).is_err());
    }

    #[test]
    fn duplicate_targets_are_allowed_and_collapsed() {
        let mut config = KeyMappingConfig::default();
        config.macos_to_windows.clear();
        config
            .macos_to_windows
            .insert("a".to_string(), "b".to_string());
        config
            .macos_to_windows
            .insert("c".to_string(), "b".to_string());
        assert!(validate_key_mapping(&config).is_ok());

        let mut mapper = KeyMapper::new(
            &config,
            InputPlatform::Macos,
            InputPlatform::Windows,
        )
        .unwrap();
        // 先按下的来源键发出目标键, 第二个来源键在目标已按下时被压制.
        assert_eq!(mapper.map_key(0x04, true, false).unwrap().usage, 0x05);
        assert!(mapper.map_key(0x06, true, false).is_none());
        // 仅松开其中一个来源键不会释放目标键, 全部松开后才释放.
        assert!(mapper.map_key(0x04, false, false).is_none());
        assert_eq!(mapper.map_key(0x06, false, false).unwrap().usage, 0x05);
        // 快照按目标键去重.
        let snapshot = mapper.map_snapshot(&KeySnapshot {
            usages: vec![0x04, 0x06],
            modifiers: ModifierMask::default(),
            buttons: Vec::new(),
        });
        assert_eq!(snapshot.usages, vec![0x05]);
    }

    #[test]
    fn mapped_modifiers_follow_press_and_release_state() {
        let config = KeyMappingConfig::default();
        let mut mapper = KeyMapper::new(
            &config,
            InputPlatform::Macos,
            InputPlatform::Windows,
        )
        .unwrap();
        let down = mapper.map_key(0xe3, true, false).unwrap();
        assert_eq!(down.usage, 0xe2);
        assert_eq!(down.modifiers, ModifierMask::ALT);
        let key = mapper.map_key(0x06, true, false).unwrap();
        assert_eq!(key.modifiers, ModifierMask::ALT);
        let up = mapper.map_key(0xe3, false, false).unwrap();
        assert_eq!(up.modifiers, ModifierMask::default());
    }
}
