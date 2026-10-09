use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::api::Network;

/// Member credentials as handed over by the website's extension-sync module.
/// These are the same fields the official extension keeps in `chrome.storage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub id: u64,
    pub session_key: String,
    pub audio_token: String,
    #[serde(default)]
    pub listen_key: String,
    #[serde(default)]
    pub api_key: String,
    /// The website binds sessions to the browser that created them, so API calls must
    /// present the login webview's User-Agent or they're rejected as "Invalid Session".
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub volume: f32,
    /// Last channel played on DI.FM (kept under its original name).
    pub last_channel: Option<u64>,
    /// Last channel played on the other networks, by network key.
    #[serde(default)]
    pub last_channels: BTreeMap<String, u64>,
    #[serde(default)]
    pub network: Network,
    #[serde(default = "enabled")]
    pub autostart: bool,
}

fn enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { volume: 1.0, last_channel: None, last_channels: BTreeMap::new(), network: Network::Di, autostart: true }
    }
}

fn dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("fm", "di", "difm-tray").context("cannot determine config directory")
}

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs()?.data_dir().to_path_buf();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn config_file(name: &str) -> Result<PathBuf> {
    let dir = dirs()?.config_dir().to_path_buf();
    fs::create_dir_all(&dir)?;
    Ok(dir.join(name))
}

fn load<T: for<'de> Deserialize<'de>>(name: &str) -> Option<T> {
    let path = config_file(name).ok()?;
    let data = fs::read(&path).ok()?;
    serde_json::from_slice(&data)
        .inspect_err(|e| log::warn!("ignoring malformed {}: {e}", path.display()))
        .ok()
}

fn store<T: Serialize>(name: &str, value: &T, private: bool) -> Result<()> {
    let path = config_file(name)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = private;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Sessions are per network; DI.FM's keeps the original file name.
fn credentials_file(network: Network) -> String {
    match network {
        Network::Di => "credentials.json".to_owned(),
        other => format!("credentials-{}.json", other.key()),
    }
}

impl Credentials {
    pub fn load(network: Network) -> Option<Self> {
        load(&credentials_file(network))
    }

    pub fn save(&self, network: Network) -> Result<()> {
        store(&credentials_file(network), self, true)
    }

    /// Forgets the sessions of all networks.
    pub fn clear() {
        for network in Network::ALL {
            if let Ok(path) = config_file(&credentials_file(network)) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        load("settings.json").unwrap_or_default()
    }

    pub fn last_channel(&self, network: Network) -> Option<u64> {
        match network {
            Network::Di => self.last_channel,
            other => self.last_channels.get(other.key()).copied(),
        }
    }

    pub fn set_last_channel(&mut self, network: Network, id: u64) {
        match network {
            Network::Di => self.last_channel = Some(id),
            other => {
                self.last_channels.insert(other.key().to_owned(), id);
            }
        }
    }

    pub fn save(&self) {
        if let Err(e) = store("settings.json", self, false) {
            log::warn!("saving settings: {e:#}");
        }
    }
}
