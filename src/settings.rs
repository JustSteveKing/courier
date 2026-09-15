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
    /// Seconds to wait for a server to start responding (streams may then run indefinitely).
    pub request_timeout_secs: u64,
    /// Sidebar label colours per kind of request.
    pub request_colors: RequestColors,
}

/// A colour from the active theme's palette, so labels follow theme switches.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LabelColor {
    /// HTTP only: a colour per method (GET green, POST yellow, PUT/PATCH blue, DELETE red).
    Method,
    Red,
    Yellow,
    Green,
    Cyan,
    Blue,
    Magenta,
    Grey,
}

impl LabelColor {
    pub const CHOICES: [LabelColor; 7] = [
        Self::Red,
        Self::Yellow,
        Self::Green,
        Self::Cyan,
        Self::Blue,
        Self::Magenta,
        Self::Grey,
    ];
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RequestColors {
    pub http: LabelColor,
    pub graphql: LabelColor,
    pub websocket: LabelColor,
    pub sse: LabelColor,
}

impl Default for RequestColors {
    fn default() -> Self {
        Self {
            http: LabelColor::Method,
            graphql: LabelColor::Magenta,
            websocket: LabelColor::Cyan,
            sse: LabelColor::Blue,
        }
    }
}

impl RequestColors {
    pub fn get(&self, kind: crate::model::RequestKind) -> LabelColor {
        use crate::model::RequestKind::*;
        match kind {
            Http => self.http,
            Graphql => self.graphql,
            WebSocket => self.websocket,
            EventStream => self.sse,
        }
    }

    pub fn set(&mut self, kind: crate::model::RequestKind, color: LabelColor) {
        use crate::model::RequestKind::*;
        match kind {
            Http => self.http = color,
            Graphql => self.graphql = color,
            WebSocket => self.websocket = color,
            EventStream => self.sse = color,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: None,
            follow_omarchy_theme: true,
            remember_responses: true,
            request_timeout_secs: 30,
            request_colors: RequestColors::default(),
        }
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
