//! What the agent last reported about its selectable options and commands.
//!
//! ACP only reveals models, modes, thought levels and slash commands inside a session. The
//! adapter never creates throwaway sessions to learn them; instead it records what real
//! sessions reported (in `<state_dir>/session-options.json`) and `probe()` / `commands()`
//! answer from that record. Defaults are the values a `session/new` response reported as
//! current.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::mapping::{SessionOptions, SettingKind};
use crate::wire::AvailableCommand;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheData {
    #[serde(default)]
    pub options: SessionOptions,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub commands: Vec<AvailableCommand>,
}

/// Shared, persisted cache. Cheap to clone.
#[derive(Clone)]
pub struct OptionsCache {
    path: Option<PathBuf>,
    data: Arc<std::sync::Mutex<CacheData>>,
}

impl OptionsCache {
    /// Loads the cache from `path`. A missing file yields an empty cache; so does a file that
    /// cannot be read or parsed, which is logged (the cache is refilled by the next session,
    /// and the next write replaces the file).
    pub fn load(path: PathBuf) -> Self {
        let data = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<CacheData>(&bytes) {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "the ACP session options cache is not valid; starting empty");
                    CacheData::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CacheData::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read the ACP session options cache; starting empty");
                CacheData::default()
            }
        };
        Self {
            path: Some(path),
            data: Arc::new(std::sync::Mutex::new(data)),
        }
    }

    /// An in-memory cache (tests).
    pub fn memory() -> Self {
        Self {
            path: None,
            data: Arc::new(std::sync::Mutex::new(CacheData::default())),
        }
    }

    pub fn get(&self) -> CacheData {
        self.data.lock().expect("cache lock").clone()
    }

    /// Applies `f`; persists and returns `true` when the data changed.
    fn update(&self, f: impl FnOnce(&mut CacheData)) -> bool {
        let mut data = self.data.lock().expect("cache lock");
        let before = data.clone();
        f(&mut data);
        let changed = *data != before;
        if changed {
            self.persist(&data);
        }
        changed
    }

    /// Records the selectable options; `true` when the model/mode/effort lists changed.
    pub fn record_options(&self, options: &SessionOptions) -> bool {
        if options.config_options.is_empty() && options.modes.is_none() {
            return false;
        }
        let before = self.get();
        self.update(|d| d.options = options.clone());
        let after = self.get();
        crate::mapping::option_lists_differ(&before.options, &after.options)
    }

    /// Records the defaults a `session/new` reported; `true` when they changed.
    pub fn record_defaults(&self, options: &SessionOptions) -> bool {
        let defaults = Defaults {
            model: options.current(SettingKind::Model),
            mode: options.current(SettingKind::Mode),
            effort: options.current(SettingKind::Effort),
        };
        self.update(|d| d.defaults = defaults)
    }

    pub fn record_commands(&self, commands: &[AvailableCommand]) {
        self.update(|d| d.commands = commands.to_vec());
    }

    fn persist(&self, data: &CacheData) {
        let Some(path) = &self.path else { return };
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec_pretty(data).map_err(std::io::Error::other)?,
            )?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = result {
            tracing::warn!(path = %path.display(), error = %e, "could not persist ACP session options cache");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("session-options.json");
        let cache = OptionsCache::load(path.clone());
        let options: SessionOptions = serde_json::from_value(serde_json::json!({
            "config_options": [{"id": "model", "name": "Model", "category": "model", "type": "select",
                                "currentValue": "m1", "options": [{"value": "m1", "name": "M1"}]}]
        }))
        .unwrap();
        cache.record_options(&options);
        cache.record_defaults(&options);
        cache.record_commands(&[AvailableCommand {
            name: "plan".into(),
            description: None,
            input: None,
        }]);
        let reloaded = OptionsCache::load(path).get();
        assert_eq!(reloaded.defaults.model.as_deref(), Some("m1"));
        assert_eq!(reloaded.commands.len(), 1);
        assert_eq!(reloaded.options, options);
    }
}
