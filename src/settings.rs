//! User settings, stored in `$XDG_CONFIG_HOME/<app>/settings.yaml`.
//!
//! Held in a GPUI global so views can observe changes. Every field has a default, so a
//! missing or partial file is fine and unknown keys from newer versions are ignored.

use std::path::PathBuf;

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;
use crate::storage::{read_yaml, write_yaml};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Interface language code (`en`, `es`, `de`, `fr`), or None to follow the system locale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Use the active Omarchy theme when available; otherwise GPUI Kit's default themes.
    pub follow_omarchy_theme: bool,
    /// Save each request's last response to the cache directory so it survives restarts.
    pub remember_responses: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { language: None, follow_omarchy_theme: true, remember_responses: true }
    }
}

pub struct AppSettings {
    path: PathBuf,
    settings: Settings,
}

impl Global for AppSettings {}

impl AppSettings {
    pub fn file_path(paths: &AppPaths) -> PathBuf {
        paths.config_dir.join("settings.yaml")
    }

    /// Loads settings; a missing file gives defaults, a broken one is reported and ignored.
    pub fn load(paths: &AppPaths) -> Self {
        let path = Self::file_path(paths);
        let settings = if path.exists() {
            read_yaml(&path).unwrap_or_else(|e| {
                eprintln!("ignoring invalid settings: {e:#}");
                Settings::default()
            })
        } else {
            Settings::default()
        };
        Self { path, settings }
    }

    pub fn get(cx: &App) -> &Settings {
        &cx.global::<Self>().settings
    }

    pub fn path(cx: &App) -> PathBuf {
        cx.global::<Self>().path.clone()
    }

    /// Writes the current settings, e.g. so there is a file to open for hand-editing.
    pub fn save_now(cx: &App) {
        let this = cx.global::<Self>();
        if let Err(e) = write_yaml(&this.path, &this.settings) {
            eprintln!("could not save settings: {e:#}");
        }
    }

    /// Changes settings, saves them, and notifies `observe_global::<AppSettings>` observers.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        let this = cx.global_mut::<Self>();
        let before = this.settings.clone();
        change(&mut this.settings);
        if this.settings == before {
            return;
        }
        if let Err(e) = write_yaml(&this.path, &this.settings) {
            eprintln!("could not save settings: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_missing_or_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(tmp.path());
        assert_eq!(AppSettings::load(&paths).settings, Settings::default());

        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(AppSettings::file_path(&paths), "language: de\nfuture_option: 1\n").unwrap();
        let loaded = AppSettings::load(&paths).settings;
        assert_eq!(loaded.language.as_deref(), Some("de"));
        assert!(loaded.follow_omarchy_theme);
    }
}
