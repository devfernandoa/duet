//! User preferences that belong to the person, not to any workspace — today
//! just the color theme. Kept in their own small file
//! (`$XDG_DATA_HOME/duet/settings.json`) rather than in `store.json`, so a
//! preference never needs a workspace schema migration and a damaged
//! preferences file can never cost a workspace.
//!
//! GTK-free: `main.rs` maps a [`ThemePreference`] onto libadwaita's
//! `StyleManager`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Which color scheme the app uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    /// Whatever the desktop prefers (libadwaita's default).
    #[default]
    System,
    /// Always the regular light (white) theme.
    Light,
    /// Always the regular dark (grey) theme.
    Dark,
}

impl ThemePreference {
    pub const ALL: [ThemePreference; 3] = [
        ThemePreference::System,
        ThemePreference::Light,
        ThemePreference::Dark,
    ];

    /// The stable id used in the settings file and the `app.theme` action.
    pub fn id(self) -> &'static str {
        match self {
            ThemePreference::System => "system",
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
        }
    }

    pub fn from_id(id: &str) -> Option<ThemePreference> {
        ThemePreference::ALL.into_iter().find(|t| t.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemePreference::System => "Follow System",
            ThemePreference::Light => "Light",
            ThemePreference::Dark => "Dark",
        }
    }
}

pub const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "settings_version")]
    pub version: u32,
    /// An unknown value (say, from a newer duet) falls back to `System`.
    #[serde(default, deserialize_with = "lenient_theme")]
    pub theme: ThemePreference,
}

fn settings_version() -> u32 {
    SETTINGS_VERSION
}

fn lenient_theme<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ThemePreference, D::Error> {
    let raw = String::deserialize(d)?;
    Ok(ThemePreference::from_id(&raw).unwrap_or_default())
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: SETTINGS_VERSION,
            theme: ThemePreference::System,
        }
    }
}

impl Settings {
    /// Reads `path`. A missing file is the defaults; an unreadable one is
    /// the defaults plus a warning — preferences are never worth refusing
    /// to start over, and the file is only rewritten when a preference
    /// actually changes.
    pub fn load(path: &Path) -> (Settings, Option<String>) {
        match std::fs::read_to_string(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Settings::default(), None)
            }
            Err(error) => (
                Settings::default(),
                Some(format!("couldn't read {}: {error}", path.display())),
            ),
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(settings) => (settings, None),
                Err(error) => (
                    Settings::default(),
                    Some(format!(
                        "ignored unreadable preferences in {}: {error}",
                        path.display()
                    )),
                ),
            },
        }
    }

    /// Atomic write (temp file, fsync, rename), like `store.json`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&Settings {
            version: SETTINGS_VERSION,
            ..self.clone()
        })
        .expect("Settings always serializes");
        let temporary = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temporary, path)
    }
}

pub fn default_settings_path() -> anyhow::Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet")
        .join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn theme_round_trips_and_defaults_to_system() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let (missing, warning) = Settings::load(&path);
        assert_eq!(missing.theme, ThemePreference::System);
        assert!(warning.is_none());

        for theme in ThemePreference::ALL {
            Settings {
                theme,
                ..Settings::default()
            }
            .save(&path)
            .unwrap();
            let (loaded, warning) = Settings::load(&path);
            assert!(warning.is_none());
            assert_eq!(loaded.theme, theme);
            assert_eq!(ThemePreference::from_id(theme.id()), Some(theme));
        }
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"theme\": \"dark\"")
        );
    }

    #[test]
    fn unknown_or_damaged_preferences_fall_back_without_failing() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"version": 7, "theme": "solarized"}"#).unwrap();
        let (settings, warning) = Settings::load(&path);
        assert_eq!(settings.theme, ThemePreference::System);
        assert!(warning.is_none());

        std::fs::write(&path, "{not json").unwrap();
        let (settings, warning) = Settings::load(&path);
        assert_eq!(settings.theme, ThemePreference::System);
        assert!(warning.unwrap().contains("ignored"));
        // Loading never rewrites the damaged file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }
}
