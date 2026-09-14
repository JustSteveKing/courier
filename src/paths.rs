//! XDG base directories and the small amount of app state kept between runs.
//!
//! - `$XDG_CONFIG_HOME/<app>/` — user settings (reserved; nothing written yet)
//! - `$XDG_DATA_HOME/<app>/collections/` — default home for new collections
//! - `$XDG_STATE_HOME/<app>/state.yaml` — open collections, active environments, last request

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use etcetera::BaseStrategy as _;
use serde::{Deserialize, Serialize};

use crate::storage::{read_yaml, write_yaml};

pub const APP_NAME: &str = "gpui-playground";
pub const APP_ID: &str = "dev.steve.gpui-playground";

#[derive(Clone, Debug)]
pub struct AppPaths {
    #[expect(dead_code, reason = "no user settings yet")]
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl AppPaths {
    pub fn from_env() -> Result<Self> {
        let xdg = etcetera::choose_base_strategy()?;
        Ok(Self {
            config_dir: xdg.config_dir().join(APP_NAME),
            data_dir: xdg.data_dir().join(APP_NAME),
            state_dir: xdg.state_dir().unwrap_or_else(|| xdg.data_dir()).join(APP_NAME),
        })
    }

    pub fn collections_dir(&self) -> PathBuf {
        self.data_dir.join("collections")
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.yaml")
    }

    /// Missing or unreadable state is not an error; the app just starts fresh.
    pub fn load_state(&self) -> AppState {
        read_yaml(&self.state_file()).unwrap_or_default()
    }

    pub fn save_state(&self, state: &AppState) -> Result<()> {
        write_yaml(&self.state_file(), state)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AppState {
    #[serde(default)]
    pub open_collections: Vec<PathBuf>,
    /// Collection root to environment file.
    #[serde(default)]
    pub active_environments: BTreeMap<PathBuf, PathBuf>,
    #[serde(default)]
    pub last_request: Option<PathBuf>,
}
