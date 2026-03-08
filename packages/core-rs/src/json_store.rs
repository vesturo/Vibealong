use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app_store::{
    now_ms, AppProfile, LoadedProfile, ProfileStore, ProfileSummary, SecretStore, StoreError,
};
use crate::config::{
    normalize_in_app_config, ConfigError, DebugConfig, DiscoveryConfig, ForwardTarget, InAppConfig,
    OscListen, RelayPaths,
};
use crate::smoothing::OutputTuning;

const JSON_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    JSON_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonDocument {
    #[serde(default = "default_schema_version")]
    version: u32,
    #[serde(default)]
    last_selected_profile_id: Option<String>,
    #[serde(default)]
    close_to_background: Option<bool>,
    #[serde(default)]
    profiles: Vec<JsonProfileRecord>,
}

impl Default for JsonDocument {
    fn default() -> Self {
        Self {
            version: JSON_SCHEMA_VERSION,
            last_selected_profile_id: None,
            close_to_background: None,
            profiles: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonProfileRecord {
    id: String,
    name: String,
    config: InAppConfig,
    created_at_ms: i64,
    updated_at_ms: i64,
}

pub struct JsonConfigStore<S: SecretStore> {
    path: PathBuf,
    secrets: S,
    document: JsonDocument,
}

impl<S: SecretStore> JsonConfigStore<S> {
    pub fn open(path: impl AsRef<Path>, secrets: S) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let document = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            if raw.trim().is_empty() {
                JsonDocument::default()
            } else {
                serde_json::from_str::<JsonDocument>(&raw)?
            }
        } else {
            JsonDocument::default()
        };

        let mut store = Self {
            path,
            secrets,
            document,
        };
        if store.document.version == 0 {
            store.document.version = JSON_SCHEMA_VERSION;
        }
        if !store.path.exists() {
            store.persist()?;
        }
        Ok(store)
    }

    fn persist(&self) -> Result<(), StoreError> {
        let serialized = serde_json::to_vec_pretty(&self.document)?;
        let mut tmp_path = self.path.clone();
        tmp_path.set_extension("tmp");
        fs::write(&tmp_path, serialized)?;

        match fs::rename(&tmp_path, &self.path) {
            Ok(()) => Ok(()),
            Err(rename_err) => {
                if self.path.exists() {
                    let _ = fs::remove_file(&self.path);
                    fs::rename(&tmp_path, &self.path)?;
                    Ok(())
                } else {
                    Err(StoreError::Io(rename_err.to_string()))
                }
            }
        }
    }

    fn build_sanitized_profile(
        profile: &AppProfile,
        stream_key: &str,
    ) -> Result<(AppProfile, i64), StoreError> {
        let normalized = normalize_in_app_config(&profile.config, stream_key)?;
        let profile_id = if profile.id.trim().is_empty() {
            Uuid::new_v4().to_string()
        } else {
            profile.id.trim().to_string()
        };

        let sanitized = AppProfile {
            id: profile_id,
            name: profile.name.trim().to_string(),
            config: InAppConfig {
                website_base_url: normalized.base_url.to_string(),
                creator_username: normalized.creator_username,
                allow_insecure_http: profile.config.allow_insecure_http,
                osc_listen: OscListen {
                    host: normalized.osc_listen.host,
                    port: normalized.osc_listen.port,
                },
                allow_network_osc: normalized.allow_network_osc,
                osc_allowed_senders: normalized
                    .osc_allowed_senders
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                relay: RelayPaths {
                    session_path: normalized.relay.session_path,
                    ingest_path: normalized.relay.ingest_path,
                },
                debug: DebugConfig {
                    log_osc: normalized.debug.log_osc,
                    log_unmapped_only: normalized.debug.log_unmapped_only,
                    log_configured_only: normalized.debug.log_configured_only,
                    log_relay: normalized.debug.log_relay,
                },
                discovery: DiscoveryConfig {
                    enabled: normalized.discovery.enabled,
                    file_path: normalized.discovery.file_path,
                    include_arg_types: normalized.discovery.include_arg_types,
                },
                forward_targets: normalized
                    .forward_targets
                    .into_iter()
                    .map(|target| ForwardTarget {
                        host: target.host,
                        port: target.port,
                    })
                    .collect(),
                mappings: normalized.mappings,
                output: OutputTuning {
                    emit_hz: normalized.output.emit_hz,
                    attack_ms: normalized.output.attack_ms,
                    release_ms: normalized.output.release_ms,
                    ema_alpha: normalized.output.ema_alpha,
                    min_delta: normalized.output.min_delta,
                    heartbeat_ms: normalized.output.heartbeat_ms,
                },
            },
        };
        Ok((sanitized, now_ms()?))
    }
}

impl<S: SecretStore> ProfileStore for JsonConfigStore<S> {
    fn upsert_profile(
        &mut self,
        profile: &AppProfile,
        stream_key: &str,
    ) -> Result<String, StoreError> {
        let (sanitized_profile, now) = Self::build_sanitized_profile(profile, stream_key)?;
        let id = sanitized_profile.id.clone();

        if let Some(record) = self.document.profiles.iter_mut().find(|item| item.id == id) {
            record.name = sanitized_profile.name;
            record.config = sanitized_profile.config;
            record.updated_at_ms = now;
        } else {
            self.document.profiles.push(JsonProfileRecord {
                id: id.clone(),
                name: sanitized_profile.name,
                config: sanitized_profile.config,
                created_at_ms: now,
                updated_at_ms: now,
            });
        }
        self.persist()?;
        self.secrets
            .set_stream_key(&id, stream_key)
            .map_err(StoreError::Secret)?;
        Ok(id)
    }

    fn list_profiles(&mut self) -> Result<Vec<ProfileSummary>, StoreError> {
        let mut rows = self.document.profiles.clone();
        rows.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            let has_stream_key = self
                .secrets
                .get_stream_key(&row.id)
                .map_err(StoreError::Secret)?
                .is_some();
            summaries.push(ProfileSummary {
                id: row.id,
                name: row.name,
                creator_username: row.config.creator_username,
                updated_at_ms: row.updated_at_ms,
                has_stream_key,
            });
        }
        Ok(summaries)
    }

    fn load_profile(&mut self, profile_id: &str) -> Result<Option<LoadedProfile>, StoreError> {
        let Some(row) = self
            .document
            .profiles
            .iter()
            .find(|item| item.id == profile_id)
        else {
            return Ok(None);
        };
        let stream_key = self
            .secrets
            .get_stream_key(profile_id)
            .map_err(StoreError::Secret)?;
        Ok(Some(LoadedProfile {
            profile: AppProfile {
                id: row.id.clone(),
                name: row.name.clone(),
                config: row.config.clone(),
            },
            stream_key,
        }))
    }

    fn last_selected_profile_id(&mut self) -> Result<Option<String>, StoreError> {
        Ok(self.document.last_selected_profile_id.clone())
    }

    fn set_last_selected_profile_id(&mut self, profile_id: Option<&str>) -> Result<(), StoreError> {
        let value = profile_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        self.document.last_selected_profile_id = value;
        self.persist()
    }

    fn close_to_background_preference(&mut self) -> Result<Option<bool>, StoreError> {
        Ok(self.document.close_to_background)
    }

    fn set_close_to_background_preference(&mut self, enabled: bool) -> Result<(), StoreError> {
        self.document.close_to_background = Some(enabled);
        self.persist()
    }

    fn set_stream_key(&mut self, profile_id: &str, stream_key: &str) -> Result<(), StoreError> {
        if stream_key.trim().is_empty() {
            return Err(StoreError::Validation(ConfigError {
                message: "config.streamKey is required".to_string(),
            }));
        }
        self.secrets
            .set_stream_key(profile_id, stream_key)
            .map_err(StoreError::Secret)
    }

    fn clear_stream_key(&mut self, profile_id: &str) -> Result<(), StoreError> {
        self.secrets
            .delete_stream_key(profile_id)
            .map_err(StoreError::Secret)
    }

    fn delete_profile(&mut self, profile_id: &str) -> Result<(), StoreError> {
        self.document.profiles.retain(|item| item.id != profile_id);
        if self.document.last_selected_profile_id.as_deref() == Some(profile_id) {
            self.document.last_selected_profile_id = None;
        }
        self.persist()?;
        self.secrets
            .delete_stream_key(profile_id)
            .map_err(StoreError::Secret)
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::app_store::InMemorySecretStore;
    use crate::config::{DebugConfig, DiscoveryConfig, ForwardTarget};
    use crate::mapping::{Curve, Mapping};

    fn tmp_json_path() -> PathBuf {
        env::temp_dir().join(format!("vibealong-json-store-{}.json", Uuid::new_v4()))
    }

    fn sample_profile() -> AppProfile {
        AppProfile {
            id: String::new(),
            name: "Default".to_string(),
            config: InAppConfig {
                website_base_url: "https://dev.vrlewds.com".to_string(),
                creator_username: "Vee".to_string(),
                allow_insecure_http: false,
                osc_listen: OscListen {
                    host: "127.0.0.1".to_string(),
                    port: 9001,
                },
                allow_network_osc: false,
                osc_allowed_senders: Vec::new(),
                relay: RelayPaths {
                    session_path: "/api/sps/session".to_string(),
                    ingest_path: "/api/sps/ingest".to_string(),
                },
                debug: DebugConfig {
                    log_osc: false,
                    log_unmapped_only: false,
                    log_configured_only: false,
                    log_relay: false,
                },
                discovery: DiscoveryConfig {
                    enabled: true,
                    file_path: "data/osc-discovered.txt".into(),
                    include_arg_types: false,
                },
                forward_targets: vec![ForwardTarget {
                    host: "127.0.0.1".to_string(),
                    port: 9000,
                }],
                mappings: vec![Mapping::new(
                    "/avatar/parameters/SPS_Contact",
                    1.0,
                    0.02,
                    false,
                    Curve::Linear,
                    0.0,
                    1.0,
                )
                .expect("valid mapping")],
                output: OutputTuning::default(),
            },
        }
    }

    #[test]
    fn json_store_round_trip_and_no_stream_key_in_file() {
        let path = tmp_json_path();
        let mut store =
            JsonConfigStore::open(&path, InMemorySecretStore::default()).expect("open store");
        let id = store
            .upsert_profile(&sample_profile(), "super_secret_value")
            .expect("upsert");
        let loaded = store.load_profile(&id).expect("load").expect("profile");
        assert_eq!(loaded.profile.name, "Default");
        assert_eq!(loaded.stream_key.as_deref(), Some("super_secret_value"));

        let content = fs::read_to_string(&path).expect("read json");
        assert!(!content.contains("super_secret_value"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn json_store_close_to_background_round_trip() {
        let path = tmp_json_path();
        let mut store =
            JsonConfigStore::open(&path, InMemorySecretStore::default()).expect("open store");
        assert_eq!(
            store
                .close_to_background_preference()
                .expect("read default"),
            None
        );
        store
            .set_close_to_background_preference(true)
            .expect("set pref");
        assert_eq!(
            store.close_to_background_preference().expect("read pref"),
            Some(true)
        );

        let _ = fs::remove_file(path);
    }
}
