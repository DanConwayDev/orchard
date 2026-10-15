//! The user's settings: durable preferences in one JSON file under the
//! XDG config dir, shared by the client UI and the embedded indexd.
//! Every field defaults when absent, so the file only ever carries the
//! user's departures from the defaults, and a reset is a deletion.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Channel visibility defaults to on (Flathub is locked on in the UI
/// and re-clamped at load); the update cadence defaults to daily, with
/// the launch check always running on top of it.
impl Default for Settings {
    fn default() -> Self {
        Settings {
            flathub_source: true,
            orchard_source: true,
            default_channel: "all".into(),
            event_relays: Vec::new(),
            social_relays: Vec::new(),
            update_checks: "daily".into(),
            remote_images: true,
            sort: "updated".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Show the Flathub (reviewed) channel in the catalog. Locked on:
    /// the UI offers no switch and load re-clamps any stored false,
    /// because reviewed installs resolve their runtimes and
    /// dependencies from Flathub.
    pub flathub_source: bool,
    /// Show the Orchard (publisher) channel in the catalog.
    pub orchard_source: bool,
    /// The channel the catalog opens on: `all`, `flathub`, `orchard`,
    /// or `installed`.
    pub default_channel: String,
    /// Event relay URLs (the catalog). Empty: the rendezvous set.
    pub event_relays: Vec<String>,
    /// Social relay URLs (profiles, contact lists). Empty: the
    /// rendezvous set.
    pub social_relays: Vec<String>,
    /// How often to re-scan installed apps for updates: `hourly`,
    /// `daily`, or `off`. The launch check is not a cadence: it always
    /// runs once at startup, whatever this says.
    pub update_checks: String,
    /// Fetch publisher-hosted images (icons, screenshots) from their
    /// URLs. Off: placeholders only, no contact with publisher hosts.
    pub remote_images: bool,
    /// The catalog list order: `updated` (recency, arrived-only) or
    /// `name` (alphabetical).
    pub sort: String,
}

/// The settings plus where they persist. Cloneable; every holder sees
/// every update, so the client UI and the embedded indexd share one
/// handle and settings apply live.
#[derive(Clone)]
pub struct SettingsHandle {
    inner: Arc<RwLock<Settings>>,
    path: Arc<PathBuf>,
}

impl SettingsHandle {
    /// Load from the standard location: `$XDG_CONFIG_HOME/orchard/
    /// settings.json`, the home fallback when XDG is unset, or the
    /// working directory when even HOME is missing.
    pub fn load_default() -> Self {
        let path = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("orchard/settings.json"))
            .unwrap_or_else(|| PathBuf::from("orchard-settings.json"));
        Self::load(path)
    }

    /// Load from `path`, falling back to defaults for a missing or
    /// unparseable file (a corrupt file must never gate the store).
    /// Three values are clamped on the way in: both channels are
    /// required (reviewed installs resolve their runtimes from
    /// Flathub, publisher installs and the app's own updates come
    /// from the Orchard channel), so pre-lock files that turned
    /// either off must not hide them; and `launch` is a retired
    /// update cadence (the launch check now runs unconditionally,
    /// the cadence only picks the repeats), so a file saved before
    /// the change maps to the new default rather than pinning a
    /// removed option.
    pub fn load(path: PathBuf) -> Self {
        let mut settings = std::fs::read(&path)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<Settings>(&bytes).ok()
            })
            .unwrap_or_default();
        settings.flathub_source = true;
        settings.orchard_source = true;
        if !matches!(
            settings.update_checks.as_str(),
            "off" | "hourly" | "daily"
        ) {
            settings.update_checks = "daily".into();
        }
        Self {
            inner: Arc::new(RwLock::new(settings)),
            path: Arc::new(path),
        }
    }

    /// A snapshot of the current settings.
    pub fn get(&self) -> Settings {
        // poison recovery, house rule: a panicked reader must not take
        // settings (and with it the supervisor's tick) down forever
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Apply an edit and persist it. The file write is atomic (tmp
    /// plus rename), so a crash mid-save cannot half-write settings.
    pub fn update(&self, edit: impl FnOnce(&mut Settings)) {
        let settings = {
            let mut settings = self.inner.write().expect("settings lock poisoned");
            edit(&mut settings);
            settings.clone()
        };
        self.save(&settings);
    }

    fn save(&self, settings: &Settings) {
        let Ok(json) = serde_json::to_vec_pretty(settings) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, self.path.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_defaults() {
        let handle = SettingsHandle::load(std::env::temp_dir().join("orchard-settings-missing"));
        assert!(handle.get().flathub_source);
        assert_eq!(handle.get().default_channel, "all");
        assert_eq!(handle.get().update_checks, "daily");
        assert!(handle.get().event_relays.is_empty());
    }

    #[test]
    fn channels_stay_locked_and_launch_maps_to_daily() {
        // a pre-lock file: either channel turned off, the retired
        // launch cadence baked in by an older version's full-struct
        // save
        let path = std::env::temp_dir().join(format!("orchard-settings-lock-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            r#"{"flathub_source": false, "orchard_source": false, "default_channel": "all", "event_relays": [], "social_relays": [], "update_checks": "launch", "remote_images": true, "sort": "updated"}"#,
        )
        .unwrap();
        let reloaded = SettingsHandle::load(path.clone());
        assert!(reloaded.get().flathub_source);
        assert!(reloaded.get().orchard_source);
        assert_eq!(reloaded.get().update_checks, "daily");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_persists_and_round_trips() {
        let path = std::env::temp_dir().join(format!("orchard-settings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let handle = SettingsHandle::load(path.clone());
        handle.update(|s| {
            s.update_checks = "off".into();
            s.event_relays = vec!["ws://127.0.0.1:8008".into()];
        });
        let reloaded = SettingsHandle::load(path.clone());
        assert_eq!(reloaded.get().update_checks, "off");
        assert_eq!(
            reloaded.get().event_relays,
            vec!["ws://127.0.0.1:8008".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }
}
