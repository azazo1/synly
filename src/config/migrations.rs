use super::schema::GuiState;
use anyhow::{Context, Result, bail};

pub(super) const MAIN_CONFIG_VERSION: u32 = 3;
pub(super) const GUI_STATE_VERSION: u32 = 1;
pub(super) const IDENTITY_VERSION: u32 = 1;
pub(super) const TRUSTED_DEVICES_VERSION: u32 = 1;

pub(super) struct MigrationDocument {
    pub(super) document: toml::Value,
    pub(super) migrated: bool,
}

pub(super) struct MainMigration {
    pub(super) document: toml::Value,
    pub(super) migrated: bool,
    pub(super) legacy_gui_state: Option<GuiState>,
}

pub(super) fn migrate_main_config(raw: &str) -> Result<MainMigration> {
    let mut document = parse_document(raw, "config.toml")?;
    let mut version = read_version(&document, true, "config.toml")?;
    let mut migrated = false;
    let mut legacy_gui_state = None;
    while version < MAIN_CONFIG_VERSION {
        match version {
            0 => {
                legacy_gui_state = Some(extract_legacy_gui_state(&mut document)?);
                version = 1;
                set_version(&mut document, version, "config.toml")?;
                migrated = true;
            }
            1 => {
                insert_main_v2_scroll_fields(&mut document)?;
                version = 2;
                set_version(&mut document, version, "config.toml")?;
                migrated = true;
            }
            2 => {
                insert_main_v3_filter_app_events(&mut document)?;
                version = 3;
                set_version(&mut document, version, "config.toml")?;
                migrated = true;
            }
            version => bail!("missing config.toml migration from version {version}"),
        }
    }
    if version > MAIN_CONFIG_VERSION {
        bail!(
            "unsupported config.toml version {version}, current version is {MAIN_CONFIG_VERSION}"
        );
    }
    Ok(MainMigration {
        document,
        migrated,
        legacy_gui_state,
    })
}

pub(super) fn migrate_identity(raw: &str) -> Result<MigrationDocument> {
    migrate_unversioned_document(raw, IDENTITY_VERSION, "identity.toml")
}

pub(super) fn migrate_trusted_devices(raw: &str) -> Result<MigrationDocument> {
    migrate_unversioned_document(raw, TRUSTED_DEVICES_VERSION, "trusted-devices.toml")
}

pub(super) fn migrate_gui_state(raw: &str) -> Result<MigrationDocument> {
    let document = parse_document(raw, "gui-state.toml")?;
    let version = read_version(&document, false, "gui-state.toml")?;
    if version < GUI_STATE_VERSION {
        bail!("missing gui-state.toml migration from version {version}");
    }
    if version > GUI_STATE_VERSION {
        bail!(
            "unsupported gui-state.toml version {version}, current version is {GUI_STATE_VERSION}"
        );
    }
    Ok(MigrationDocument {
        document,
        migrated: false,
    })
}

fn migrate_unversioned_document(
    raw: &str,
    current_version: u32,
    file_name: &str,
) -> Result<MigrationDocument> {
    let mut document = parse_document(raw, file_name)?;
    let mut version = read_version(&document, true, file_name)?;
    let mut migrated = false;
    while version < current_version {
        match version {
            0 => {
                version = 1;
                set_version(&mut document, version, file_name)?;
                migrated = true;
            }
            version => bail!("missing {file_name} migration from version {version}"),
        }
    }
    if version > current_version {
        bail!(
            "unsupported {file_name} version {version}, current version is {current_version}"
        );
    }
    Ok(MigrationDocument { document, migrated })
}

fn parse_document(raw: &str, file_name: &str) -> Result<toml::Value> {
    toml::from_str(raw).with_context(|| format!("failed to parse {file_name}"))
}

fn read_version(document: &toml::Value, allow_missing: bool, file_name: &str) -> Result<u32> {
    let table = document
        .as_table()
        .with_context(|| format!("{file_name} must contain a TOML table"))?;
    let Some(value) = table.get("version") else {
        if allow_missing {
            return Ok(0);
        }
        bail!("{file_name} is missing the version field");
    };
    let version = value
        .as_integer()
        .with_context(|| format!("{file_name} version must be an integer"))?;
    u32::try_from(version).with_context(|| format!("{file_name} version is out of range"))
}

fn set_version(document: &mut toml::Value, version: u32, file_name: &str) -> Result<()> {
    let table = document
        .as_table_mut()
        .with_context(|| format!("{file_name} must contain a TOML table"))?;
    table.insert("version".to_string(), toml::Value::Integer(i64::from(version)));
    Ok(())
}

fn extract_legacy_gui_state(document: &mut toml::Value) -> Result<GuiState> {
    let table = document
        .as_table_mut()
        .context("config.toml must contain a ui table")?;
    let ui = table
        .get_mut("ui")
        .and_then(toml::Value::as_table_mut)
        .context("config.toml must contain a ui table")?;
    Ok(GuiState {
        first_run_completed: take_value(ui, "first_run_completed")?
            .context("config.toml ui.first_run_completed is missing")?
            .try_into()
            .context("config.toml ui.first_run_completed is invalid")?,
        window_width: take_value(ui, "window_width")?
            .context("config.toml ui.window_width is missing")?
            .try_into()
            .context("config.toml ui.window_width is invalid")?,
        window_height: take_value(ui, "window_height")?
            .context("config.toml ui.window_height is missing")?
            .try_into()
            .context("config.toml ui.window_height is invalid")?,
    })
}

fn insert_main_v2_scroll_fields(document: &mut toml::Value) -> Result<()> {
    let table = document
        .as_table_mut()
        .context("config.toml must contain a TOML table")?;
    if !table.contains_key("input") {
        table.insert("input".to_string(), toml::Value::Table(Default::default()));
    }
    let input = table
        .get_mut("input")
        .and_then(toml::Value::as_table_mut)
        .context("config.toml input must be a TOML table")?;
    for key in [
        "native_scroll_macos_to_windows",
        "native_scroll_windows_to_macos",
    ] {
        if !input.contains_key(key) {
            input.insert(key.to_string(), toml::Value::Boolean(false));
        }
    }
    Ok(())
}

fn insert_main_v3_filter_app_events(document: &mut toml::Value) -> Result<()> {
    let table = document
        .as_table_mut()
        .context("config.toml must contain a TOML table")?;
    if !table.contains_key("input") {
        table.insert("input".to_string(), toml::Value::Table(Default::default()));
    }
    let input = table
        .get_mut("input")
        .and_then(toml::Value::as_table_mut)
        .context("config.toml input must be a TOML table")?;
    if !input.contains_key("filter_app_events") {
        input.insert("filter_app_events".to_string(), toml::Value::Boolean(true));
    }
    Ok(())
}

fn take_value(
    table: &mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<toml::Value>> {
    Ok(table.remove(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_v0_extracts_gui_state_and_adds_version() {
        let migration = migrate_main_config(
            "[ui]\nfirst_run_completed = true\nwindow_width = 900\nwindow_height = 600\n",
        )
        .unwrap();
        assert!(migration.migrated);
        assert_eq!(
            migration.legacy_gui_state,
            Some(GuiState {
                first_run_completed: true,
                window_width: 900,
                window_height: 600,
            })
        );
        let table = migration.document.as_table().unwrap();
        assert_eq!(table.get("version").and_then(toml::Value::as_integer), Some(3));
        let ui = table.get("ui").and_then(toml::Value::as_table).unwrap();
        assert!(!ui.contains_key("first_run_completed"));
        assert!(!ui.contains_key("window_width"));
        assert!(!ui.contains_key("window_height"));
        let input = table.get("input").and_then(toml::Value::as_table).unwrap();
        assert_eq!(
            input.get("native_scroll_macos_to_windows"),
            Some(&toml::Value::Boolean(false))
        );
        assert_eq!(
            input.get("native_scroll_windows_to_macos"),
            Some(&toml::Value::Boolean(false))
        );
        assert_eq!(
            input.get("filter_app_events"),
            Some(&toml::Value::Boolean(true))
        );
    }

    #[test]
    fn current_main_config_is_not_migrated() {
        let migration = migrate_main_config("version = 3\n").unwrap();
        assert!(!migration.migrated);
        assert!(migration.legacy_gui_state.is_none());
    }

    #[test]
    fn main_v1_adds_native_scroll_fields_and_version_three() {
        let migration =
            migrate_main_config("version = 1\n[input]\nreverse_mouse_wheel = true\n").unwrap();
        assert!(migration.migrated);
        let table = migration.document.as_table().unwrap();
        assert_eq!(table.get("version").and_then(toml::Value::as_integer), Some(3));
        let input = table.get("input").and_then(toml::Value::as_table).unwrap();
        assert_eq!(
            input.get("native_scroll_macos_to_windows"),
            Some(&toml::Value::Boolean(false))
        );
        assert_eq!(
            input.get("native_scroll_windows_to_macos"),
            Some(&toml::Value::Boolean(false))
        );
        assert_eq!(
            input.get("reverse_mouse_wheel"),
            Some(&toml::Value::Boolean(true))
        );
        assert_eq!(
            input.get("filter_app_events"),
            Some(&toml::Value::Boolean(true))
        );
    }

    #[test]
    fn main_v2_adds_filter_app_events_and_version_three() {
        let migration =
            migrate_main_config("version = 2\n[input]\nblock_switch_on_press = true\n").unwrap();
        assert!(migration.migrated);
        let table = migration.document.as_table().unwrap();
        assert_eq!(table.get("version").and_then(toml::Value::as_integer), Some(3));
        let input = table.get("input").and_then(toml::Value::as_table).unwrap();
        assert_eq!(
            input.get("filter_app_events"),
            Some(&toml::Value::Boolean(true))
        );
        assert_eq!(
            input.get("block_switch_on_press"),
            Some(&toml::Value::Boolean(true))
        );
    }

    #[test]
    fn gui_state_requires_a_version() {
        assert!(migrate_gui_state("first_run_completed = false\n").is_err());
    }

    #[test]
    fn future_versions_are_rejected() {
        assert!(migrate_identity("version = 2\n").is_err());
        assert!(migrate_trusted_devices("version = 2\n").is_err());
        assert!(migrate_main_config("version = 4\n").is_err());
    }
}
