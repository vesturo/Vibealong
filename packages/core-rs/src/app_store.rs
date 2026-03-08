use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{
    normalize_in_app_config, ConfigError, DebugConfig, DiscoveryConfig, ForwardTarget, InAppConfig,
    OscListen, RelayPaths,
};
use crate::mapping::{Curve, Mapping};
use crate::smoothing::OutputTuning;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppProfile {
    pub id: String,
    pub name: String,
    pub config: InAppConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadedProfile {
    pub profile: AppProfile,
    pub stream_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub creator_username: String,
    pub updated_at_ms: i64,
    pub has_stream_key: bool,
}

pub trait SecretStore {
    fn set_stream_key(&self, profile_id: &str, stream_key: &str) -> Result<(), String>;
    fn get_stream_key(&self, profile_id: &str) -> Result<Option<String>, String>;
    fn delete_stream_key(&self, profile_id: &str) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    entries: Mutex<HashMap<String, String>>,
}

impl SecretStore for InMemorySecretStore {
    fn set_stream_key(&self, profile_id: &str, stream_key: &str) -> Result<(), String> {
        self.entries
            .lock()
            .map_err(|_| "Secret store mutex poisoned".to_string())?
            .insert(profile_id.to_string(), stream_key.to_string());
        Ok(())
    }

    fn get_stream_key(&self, profile_id: &str) -> Result<Option<String>, String> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| "Secret store mutex poisoned".to_string())?
            .get(profile_id)
            .cloned())
    }

    fn delete_stream_key(&self, profile_id: &str) -> Result<(), String> {
        self.entries
            .lock()
            .map_err(|_| "Secret store mutex poisoned".to_string())?
            .remove(profile_id);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyringSecretStore {
    service_name: String,
    legacy_service_names: Vec<String>,
}

impl KeyringSecretStore {
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            legacy_service_names: Vec::new(),
        }
    }

    pub fn with_legacy_service_names(mut self, legacy_service_names: Vec<String>) -> Self {
        self.legacy_service_names = legacy_service_names;
        self
    }

    fn account_name(profile_id: &str) -> String {
        format!("profile_{}_stream_key", profile_id.trim())
    }

    fn legacy_account_names(profile_id: &str) -> Vec<String> {
        let id = profile_id.trim();
        vec![
            format!("profile:{id}:stream_key"),
            format!("profile-{id}-stream_key"),
            id.to_string(),
        ]
    }

    fn entry_for_service_and_account(
        &self,
        service_name: &str,
        account_name: &str,
    ) -> Result<keyring::Entry, String> {
        keyring::Entry::new(service_name, account_name).map_err(|e| e.to_string())
    }

    fn entry(&self, profile_id: &str) -> Result<keyring::Entry, String> {
        self.entry_for_service_and_account(&self.service_name, &Self::account_name(profile_id))
    }

    fn read_service_accounts(
        &self,
        service_name: &str,
        profile_id: &str,
    ) -> Result<Option<(String, String)>, String> {
        let mut accounts = vec![Self::account_name(profile_id)];
        accounts.extend(Self::legacy_account_names(profile_id));

        for account_name in accounts {
            match self
                .entry_for_service_and_account(service_name, &account_name)?
                .get_password()
            {
                Ok(value) => return Ok(Some((account_name, value))),
                Err(keyring::Error::NoEntry) => {}
                Err(err) => return Err(err.to_string()),
            }
        }
        Ok(None)
    }

    fn delete_service_accounts(&self, service_name: &str, profile_id: &str) -> Result<(), String> {
        let mut accounts = vec![Self::account_name(profile_id)];
        accounts.extend(Self::legacy_account_names(profile_id));

        for account_name in accounts {
            match self
                .entry_for_service_and_account(service_name, &account_name)?
                .delete_credential()
            {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(err) => return Err(err.to_string()),
            }
        }
        Ok(())
    }
}

impl SecretStore for KeyringSecretStore {
    fn set_stream_key(&self, profile_id: &str, stream_key: &str) -> Result<(), String> {
        self.entry(profile_id)?
            .set_password(stream_key)
            .map_err(|e| e.to_string())
    }

    fn get_stream_key(&self, profile_id: &str) -> Result<Option<String>, String> {
        if let Some((account_name, value)) =
            self.read_service_accounts(&self.service_name, profile_id)?
        {
            if account_name != Self::account_name(profile_id) {
                // Opportunistic migration to the primary account namespace.
                let _ = self.entry(profile_id)?.set_password(&value);
            }
            return Ok(Some(value));
        }

        for legacy in &self.legacy_service_names {
            if let Some((_, value)) = self.read_service_accounts(legacy, profile_id)? {
                // Opportunistic migration to the primary service namespace.
                let _ = self.entry(profile_id)?.set_password(&value);
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn delete_stream_key(&self, profile_id: &str) -> Result<(), String> {
        self.delete_service_accounts(&self.service_name, profile_id)?;
        for legacy in &self.legacy_service_names {
            self.delete_service_accounts(legacy, profile_id)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(String),
    Json(String),
    Validation(ConfigError),
    Secret(String),
    Time(String),
}

impl Display for StoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(err) => write!(f, "SQLite error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::Validation(err) => write!(f, "Validation error: {err}"),
            Self::Secret(err) => write!(f, "Secret storage error: {err}"),
            Self::Time(err) => write!(f, "Time error: {err}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<ConfigError> for StoreError {
    fn from(value: ConfigError) -> Self {
        Self::Validation(value)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value.to_string())
    }
}

pub trait ProfileStore {
    fn upsert_profile(
        &mut self,
        profile: &AppProfile,
        stream_key: &str,
    ) -> Result<String, StoreError>;
    fn list_profiles(&mut self) -> Result<Vec<ProfileSummary>, StoreError>;
    fn load_profile(&mut self, profile_id: &str) -> Result<Option<LoadedProfile>, StoreError>;
    fn last_selected_profile_id(&mut self) -> Result<Option<String>, StoreError>;
    fn set_last_selected_profile_id(&mut self, profile_id: Option<&str>) -> Result<(), StoreError>;
    fn close_to_background_preference(&mut self) -> Result<Option<bool>, StoreError>;
    fn set_close_to_background_preference(&mut self, enabled: bool) -> Result<(), StoreError>;
    fn set_stream_key(&mut self, profile_id: &str, stream_key: &str) -> Result<(), StoreError>;
    fn clear_stream_key(&mut self, profile_id: &str) -> Result<(), StoreError>;
    fn delete_profile(&mut self, profile_id: &str) -> Result<(), StoreError>;
}

fn bool_to_i64(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn i64_to_bool(value: i64) -> bool {
    value != 0
}

fn parse_osc_sender_lines(raw: &str) -> Vec<String> {
    raw.split(['\n', ',', ';'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter_map(|value| value.parse::<IpAddr>().ok())
        .map(|ip| ip.to_string())
        .collect()
}

fn format_osc_sender_lines(senders: &[IpAddr]) -> String {
    senders
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

const META_SCHEMA_VERSION: &str = "schema_version";
const META_LAST_SELECTED_PROFILE_ID: &str = "last_selected_profile_id";
const META_CLOSE_TO_BACKGROUND: &str = "close_to_background";

pub(crate) fn now_ms() -> Result<i64, StoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| StoreError::Time(e.to_string()))?;
    Ok(duration.as_millis() as i64)
}

fn add_profiles_column_if_missing(conn: &Connection, clause: &str) -> Result<(), StoreError> {
    let sql = format!("ALTER TABLE profiles ADD COLUMN {clause}");
    match conn.execute(&sql, []) {
        Ok(_) => Ok(()),
        Err(err) => match &err {
            rusqlite::Error::SqliteFailure(_, Some(message))
                if message
                    .to_ascii_lowercase()
                    .contains("duplicate column name") =>
            {
                Ok(())
            }
            _ => Err(StoreError::Sqlite(err)),
        },
    }
}

fn curve_name(curve: Curve) -> &'static str {
    match curve {
        Curve::Linear => "linear",
        Curve::EaseOutQuad => "easeOutQuad",
        Curve::EaseInQuad => "easeInQuad",
        Curve::EaseInOutQuad => "easeInOutQuad",
    }
}

pub struct AppConfigStore<S: SecretStore> {
    conn: Connection,
    secrets: S,
}

impl<S: SecretStore> AppConfigStore<S> {
    pub fn open(path: impl AsRef<Path>, secrets: S) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        let mut store = Self { conn, secrets };
        store.init_schema()?;
        Ok(store)
    }

    pub fn in_memory(secrets: S) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        let mut store = Self { conn, secrets };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&mut self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS app_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                website_base_url TEXT NOT NULL,
                creator_username TEXT NOT NULL,
                allow_insecure_http INTEGER NOT NULL,
                osc_host TEXT NOT NULL,
                osc_port INTEGER NOT NULL,
                osc_allow_network INTEGER NOT NULL DEFAULT 0,
                osc_allowed_senders TEXT NOT NULL DEFAULT '',
                relay_session_path TEXT NOT NULL,
                relay_ingest_path TEXT NOT NULL,
                debug_log_osc INTEGER NOT NULL,
                debug_log_unmapped_only INTEGER NOT NULL,
                debug_log_configured_only INTEGER NOT NULL,
                debug_log_relay INTEGER NOT NULL,
                discovery_enabled INTEGER NOT NULL,
                discovery_file_path TEXT NOT NULL,
                discovery_include_arg_types INTEGER NOT NULL,
                output_emit_hz REAL NOT NULL,
                output_attack_ms REAL NOT NULL,
                output_release_ms REAL NOT NULL,
                output_ema_alpha REAL NOT NULL,
                output_min_delta REAL NOT NULL,
                output_heartbeat_ms INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS profile_mappings (
                profile_id TEXT NOT NULL,
                sort_index INTEGER NOT NULL,
                address TEXT NOT NULL,
                weight REAL NOT NULL,
                deadzone REAL NOT NULL,
                invert INTEGER NOT NULL,
                curve TEXT NOT NULL,
                min_value REAL NOT NULL,
                max_value REAL NOT NULL,
                PRIMARY KEY(profile_id, sort_index),
                FOREIGN KEY(profile_id) REFERENCES profiles(id) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS profile_forward_targets (
                profile_id TEXT NOT NULL,
                sort_index INTEGER NOT NULL,
                host TEXT NOT NULL,
                port INTEGER NOT NULL,
                PRIMARY KEY(profile_id, sort_index),
                FOREIGN KEY(profile_id) REFERENCES profiles(id) ON DELETE CASCADE
            );
            ",
        )?;
        add_profiles_column_if_missing(&self.conn, "osc_allow_network INTEGER NOT NULL DEFAULT 0")?;
        add_profiles_column_if_missing(&self.conn, "osc_allowed_senders TEXT NOT NULL DEFAULT ''")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO app_meta(key, value) VALUES(?1, '1')",
            [META_SCHEMA_VERSION],
        )?;
        Ok(())
    }

    pub fn last_selected_profile_id(&self) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT value FROM app_meta WHERE key = ?1",
                [META_LAST_SELECTED_PROFILE_ID],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn set_last_selected_profile_id(&self, profile_id: Option<&str>) -> Result<(), StoreError> {
        let profile_id = profile_id.map(|value| value.trim().to_string());
        if profile_id.as_ref().is_some_and(|value| value.is_empty()) || profile_id.is_none() {
            self.conn.execute(
                "DELETE FROM app_meta WHERE key = ?1",
                [META_LAST_SELECTED_PROFILE_ID],
            )?;
            return Ok(());
        }

        self.conn.execute(
            "
            INSERT INTO app_meta(key, value) VALUES(?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            ",
            params![
                META_LAST_SELECTED_PROFILE_ID,
                profile_id.expect("profile id present")
            ],
        )?;
        Ok(())
    }

    pub fn close_to_background_preference(&self) -> Result<Option<bool>, StoreError> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM app_meta WHERE key = ?1",
                [META_CLOSE_TO_BACKGROUND],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(match value.as_deref() {
            Some("1") | Some("true") => Some(true),
            Some("0") | Some("false") => Some(false),
            Some(_) | None => None,
        })
    }

    pub fn set_close_to_background_preference(&self, enabled: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "
            INSERT INTO app_meta(key, value) VALUES(?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            ",
            params![META_CLOSE_TO_BACKGROUND, if enabled { "1" } else { "0" }],
        )?;
        Ok(())
    }

    pub fn upsert_profile(
        &mut self,
        profile: &AppProfile,
        stream_key: &str,
    ) -> Result<String, StoreError> {
        let normalized = normalize_in_app_config(&profile.config, stream_key)?;
        let profile_id = if profile.id.trim().is_empty() {
            Uuid::new_v4().to_string()
        } else {
            profile.id.trim().to_string()
        };
        let now = now_ms()?;

        let created_at_ms = self
            .conn
            .query_row(
                "SELECT created_at_ms FROM profiles WHERE id = ?1",
                [profile_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(now);

        let tx = self.conn.transaction()?;
        tx.execute(
            "
            INSERT INTO profiles (
                id, name, website_base_url, creator_username, allow_insecure_http,
                osc_host, osc_port, osc_allow_network, osc_allowed_senders, relay_session_path, relay_ingest_path,
                debug_log_osc, debug_log_unmapped_only, debug_log_configured_only, debug_log_relay,
                discovery_enabled, discovery_file_path, discovery_include_arg_types,
                output_emit_hz, output_attack_ms, output_release_ms, output_ema_alpha, output_min_delta, output_heartbeat_ms,
                created_at_ms, updated_at_ms
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15,
                ?16, ?17, ?18,
                ?19, ?20, ?21, ?22, ?23, ?24,
                ?25, ?26
            )
            ON CONFLICT(id) DO UPDATE SET
                name=excluded.name,
                website_base_url=excluded.website_base_url,
                creator_username=excluded.creator_username,
                allow_insecure_http=excluded.allow_insecure_http,
                osc_host=excluded.osc_host,
                osc_port=excluded.osc_port,
                osc_allow_network=excluded.osc_allow_network,
                osc_allowed_senders=excluded.osc_allowed_senders,
                relay_session_path=excluded.relay_session_path,
                relay_ingest_path=excluded.relay_ingest_path,
                debug_log_osc=excluded.debug_log_osc,
                debug_log_unmapped_only=excluded.debug_log_unmapped_only,
                debug_log_configured_only=excluded.debug_log_configured_only,
                debug_log_relay=excluded.debug_log_relay,
                discovery_enabled=excluded.discovery_enabled,
                discovery_file_path=excluded.discovery_file_path,
                discovery_include_arg_types=excluded.discovery_include_arg_types,
                output_emit_hz=excluded.output_emit_hz,
                output_attack_ms=excluded.output_attack_ms,
                output_release_ms=excluded.output_release_ms,
                output_ema_alpha=excluded.output_ema_alpha,
                output_min_delta=excluded.output_min_delta,
                output_heartbeat_ms=excluded.output_heartbeat_ms,
                created_at_ms=excluded.created_at_ms,
                updated_at_ms=excluded.updated_at_ms
            ",
            params![
                profile_id,
                profile.name.trim(),
                normalized.base_url.to_string(),
                normalized.creator_username,
                bool_to_i64(profile.config.allow_insecure_http),
                normalized.osc_listen.host,
                i64::from(normalized.osc_listen.port),
                bool_to_i64(normalized.allow_network_osc),
                format_osc_sender_lines(&normalized.osc_allowed_senders),
                normalized.relay.session_path,
                normalized.relay.ingest_path,
                bool_to_i64(normalized.debug.log_osc),
                bool_to_i64(normalized.debug.log_unmapped_only),
                bool_to_i64(normalized.debug.log_configured_only),
                bool_to_i64(normalized.debug.log_relay),
                bool_to_i64(normalized.discovery.enabled),
                normalized.discovery.file_path.to_string_lossy(),
                bool_to_i64(normalized.discovery.include_arg_types),
                normalized.output.emit_hz,
                normalized.output.attack_ms,
                normalized.output.release_ms,
                normalized.output.ema_alpha,
                normalized.output.min_delta,
                normalized.output.heartbeat_ms as i64,
                created_at_ms,
                now,
            ],
        )?;

        tx.execute(
            "DELETE FROM profile_mappings WHERE profile_id = ?1",
            [profile_id.as_str()],
        )?;
        for (idx, mapping) in normalized.mappings.iter().enumerate() {
            tx.execute(
                "
                INSERT INTO profile_mappings (
                    profile_id, sort_index, address, weight, deadzone, invert, curve, min_value, max_value
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ",
                params![
                    profile_id,
                    idx as i64,
                    mapping.address,
                    mapping.weight,
                    mapping.deadzone,
                    bool_to_i64(mapping.invert),
                    curve_name(mapping.curve),
                    mapping.min,
                    mapping.max,
                ],
            )?;
        }

        tx.execute(
            "DELETE FROM profile_forward_targets WHERE profile_id = ?1",
            [profile_id.as_str()],
        )?;
        for (idx, target) in normalized.forward_targets.iter().enumerate() {
            tx.execute(
                "
                INSERT INTO profile_forward_targets (
                    profile_id, sort_index, host, port
                ) VALUES (?1, ?2, ?3, ?4)
                ",
                params![profile_id, idx as i64, target.host, i64::from(target.port)],
            )?;
        }

        tx.commit()?;
        self.secrets
            .set_stream_key(&profile_id, stream_key)
            .map_err(StoreError::Secret)?;
        Ok(profile_id)
    }

    pub fn load_profile(&self, profile_id: &str) -> Result<Option<LoadedProfile>, StoreError> {
        let profile_row = self
            .conn
            .query_row(
                "
                SELECT
                    id, name, website_base_url, creator_username, allow_insecure_http,
                    osc_host, osc_port, osc_allow_network, osc_allowed_senders, relay_session_path, relay_ingest_path,
                    debug_log_osc, debug_log_unmapped_only, debug_log_configured_only, debug_log_relay,
                    discovery_enabled, discovery_file_path, discovery_include_arg_types,
                    output_emit_hz, output_attack_ms, output_release_ms, output_ema_alpha, output_min_delta, output_heartbeat_ms
                FROM profiles WHERE id = ?1
                ",
                [profile_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                        row.get::<_, i64>(14)?,
                        row.get::<_, i64>(15)?,
                        row.get::<_, String>(16)?,
                        row.get::<_, i64>(17)?,
                        row.get::<_, f64>(18)?,
                        row.get::<_, f64>(19)?,
                        row.get::<_, f64>(20)?,
                        row.get::<_, f64>(21)?,
                        row.get::<_, f64>(22)?,
                        row.get::<_, i64>(23)?,
                    ))
                },
            )
            .optional()?;

        let Some((
            id,
            name,
            website_base_url,
            creator_username,
            allow_insecure_http,
            osc_host,
            osc_port,
            osc_allow_network,
            osc_allowed_senders,
            relay_session_path,
            relay_ingest_path,
            debug_log_osc,
            debug_log_unmapped_only,
            debug_log_configured_only,
            debug_log_relay,
            discovery_enabled,
            discovery_file_path,
            discovery_include_arg_types,
            output_emit_hz,
            output_attack_ms,
            output_release_ms,
            output_ema_alpha,
            output_min_delta,
            output_heartbeat_ms,
        )) = profile_row
        else {
            return Ok(None);
        };

        let mut mappings_stmt = self.conn.prepare(
            "
            SELECT address, weight, deadzone, invert, curve, min_value, max_value
            FROM profile_mappings
            WHERE profile_id = ?1
            ORDER BY sort_index ASC
            ",
        )?;
        let mappings = mappings_stmt
            .query_map([profile_id], |row| {
                let address: String = row.get(0)?;
                let weight: f64 = row.get(1)?;
                let deadzone: f64 = row.get(2)?;
                let invert: i64 = row.get(3)?;
                let curve: String = row.get(4)?;
                let min: f64 = row.get(5)?;
                let max: f64 = row.get(6)?;
                Ok(Mapping::new(
                    address,
                    weight,
                    deadzone,
                    i64_to_bool(invert),
                    Curve::from_name(&curve),
                    min,
                    max,
                )
                .expect("database contains invalid mapping"))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut targets_stmt = self.conn.prepare(
            "
            SELECT host, port
            FROM profile_forward_targets
            WHERE profile_id = ?1
            ORDER BY sort_index ASC
            ",
        )?;
        let forward_targets = targets_stmt
            .query_map([profile_id], |row| {
                Ok(ForwardTarget {
                    host: row.get::<_, String>(0)?,
                    port: row.get::<_, i64>(1)? as u16,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let stream_key = self
            .secrets
            .get_stream_key(profile_id)
            .map_err(StoreError::Secret)?;

        Ok(Some(LoadedProfile {
            profile: AppProfile {
                id,
                name,
                config: InAppConfig {
                    website_base_url,
                    creator_username,
                    allow_insecure_http: i64_to_bool(allow_insecure_http),
                    osc_listen: OscListen {
                        host: osc_host,
                        port: osc_port as u16,
                    },
                    allow_network_osc: i64_to_bool(osc_allow_network),
                    osc_allowed_senders: parse_osc_sender_lines(&osc_allowed_senders),
                    relay: RelayPaths {
                        session_path: relay_session_path,
                        ingest_path: relay_ingest_path,
                    },
                    debug: DebugConfig {
                        log_osc: i64_to_bool(debug_log_osc),
                        log_unmapped_only: i64_to_bool(debug_log_unmapped_only),
                        log_configured_only: i64_to_bool(debug_log_configured_only),
                        log_relay: i64_to_bool(debug_log_relay),
                    },
                    discovery: DiscoveryConfig {
                        enabled: i64_to_bool(discovery_enabled),
                        file_path: discovery_file_path.into(),
                        include_arg_types: i64_to_bool(discovery_include_arg_types),
                    },
                    forward_targets,
                    mappings,
                    output: OutputTuning {
                        emit_hz: output_emit_hz,
                        attack_ms: output_attack_ms,
                        release_ms: output_release_ms,
                        ema_alpha: output_ema_alpha,
                        min_delta: output_min_delta,
                        heartbeat_ms: output_heartbeat_ms as u64,
                    },
                },
            },
            stream_key,
        }))
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileSummary>, StoreError> {
        let mut stmt = self.conn.prepare(
            "
            SELECT id, name, creator_username, updated_at_ms
            FROM profiles
            ORDER BY updated_at_ms DESC
            ",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut summaries = Vec::new();
        for (id, name, creator_username, updated_at_ms) in rows {
            let has_stream_key = self
                .secrets
                .get_stream_key(&id)
                .map_err(StoreError::Secret)?
                .is_some();
            summaries.push(ProfileSummary {
                id,
                name,
                creator_username,
                updated_at_ms,
                has_stream_key,
            });
        }
        Ok(summaries)
    }

    pub fn delete_profile(&mut self, profile_id: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM profiles WHERE id = ?1", [profile_id])?;
        tx.execute(
            "DELETE FROM app_meta WHERE key = ?1 AND value = ?2",
            params![META_LAST_SELECTED_PROFILE_ID, profile_id],
        )?;
        tx.commit()?;
        self.secrets
            .delete_stream_key(profile_id)
            .map_err(StoreError::Secret)?;
        Ok(())
    }

    pub fn set_stream_key(&self, profile_id: &str, stream_key: &str) -> Result<(), StoreError> {
        if stream_key.trim().is_empty() {
            return Err(StoreError::Validation(ConfigError {
                message: "config.streamKey is required".to_string(),
            }));
        }
        self.secrets
            .set_stream_key(profile_id, stream_key)
            .map_err(StoreError::Secret)
    }

    pub fn clear_stream_key(&self, profile_id: &str) -> Result<(), StoreError> {
        self.secrets
            .delete_stream_key(profile_id)
            .map_err(StoreError::Secret)
    }
}

impl<S: SecretStore> ProfileStore for AppConfigStore<S> {
    fn upsert_profile(
        &mut self,
        profile: &AppProfile,
        stream_key: &str,
    ) -> Result<String, StoreError> {
        AppConfigStore::upsert_profile(self, profile, stream_key)
    }

    fn list_profiles(&mut self) -> Result<Vec<ProfileSummary>, StoreError> {
        AppConfigStore::list_profiles(self)
    }

    fn load_profile(&mut self, profile_id: &str) -> Result<Option<LoadedProfile>, StoreError> {
        AppConfigStore::load_profile(self, profile_id)
    }

    fn last_selected_profile_id(&mut self) -> Result<Option<String>, StoreError> {
        AppConfigStore::last_selected_profile_id(self)
    }

    fn set_last_selected_profile_id(&mut self, profile_id: Option<&str>) -> Result<(), StoreError> {
        AppConfigStore::set_last_selected_profile_id(self, profile_id)
    }

    fn close_to_background_preference(&mut self) -> Result<Option<bool>, StoreError> {
        AppConfigStore::close_to_background_preference(self)
    }

    fn set_close_to_background_preference(&mut self, enabled: bool) -> Result<(), StoreError> {
        AppConfigStore::set_close_to_background_preference(self, enabled)
    }

    fn set_stream_key(&mut self, profile_id: &str, stream_key: &str) -> Result<(), StoreError> {
        AppConfigStore::set_stream_key(self, profile_id, stream_key)
    }

    fn clear_stream_key(&mut self, profile_id: &str) -> Result<(), StoreError> {
        AppConfigStore::clear_stream_key(self, profile_id)
    }

    fn delete_profile(&mut self, profile_id: &str) -> Result<(), StoreError> {
        AppConfigStore::delete_profile(self, profile_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    enabled: false,
                    file_path: "scripts/vrl-osc-discovered.txt".into(),
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
    fn upsert_and_load_profile_round_trip() {
        let secrets = InMemorySecretStore::default();
        let mut store = AppConfigStore::in_memory(secrets).expect("store");
        let profile = sample_profile();

        let id = store
            .upsert_profile(&profile, "stream_secret")
            .expect("upsert");
        let loaded = store.load_profile(&id).expect("load").expect("profile");

        assert_eq!(loaded.profile.name, "Default");
        assert_eq!(
            loaded.profile.config.creator_username,
            profile.config.creator_username
        );
        assert_eq!(loaded.profile.config.mappings.len(), 1);
        assert_eq!(loaded.stream_key.as_deref(), Some("stream_secret"));
    }

    #[test]
    fn sqlite_schema_does_not_store_stream_key() {
        let secrets = InMemorySecretStore::default();
        let mut store = AppConfigStore::in_memory(secrets).expect("store");
        let profile = sample_profile();
        let id = store
            .upsert_profile(&profile, "super_secret_value")
            .expect("upsert");
        let _ = store.load_profile(&id).expect("load");

        let mut stmt = store
            .conn
            .prepare("PRAGMA table_info(profiles)")
            .expect("pragma");
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        assert!(!columns.iter().any(|column| column == "stream_key"));
    }

    #[test]
    fn delete_profile_removes_secret() {
        let secrets = InMemorySecretStore::default();
        let mut store = AppConfigStore::in_memory(secrets).expect("store");
        let profile = sample_profile();
        let id = store
            .upsert_profile(&profile, "super_secret_value")
            .expect("upsert");
        store.delete_profile(&id).expect("delete");
        assert!(store.load_profile(&id).expect("load").is_none());
    }

    #[test]
    fn last_selected_profile_meta_round_trip() {
        let secrets = InMemorySecretStore::default();
        let store = AppConfigStore::in_memory(secrets).expect("store");
        assert_eq!(store.last_selected_profile_id().expect("read"), None);

        store
            .set_last_selected_profile_id(Some("abc-123"))
            .expect("set");
        assert_eq!(
            store.last_selected_profile_id().expect("read"),
            Some("abc-123".to_string())
        );

        store.set_last_selected_profile_id(None).expect("clear");
        assert_eq!(store.last_selected_profile_id().expect("read"), None);
    }

    #[test]
    fn deleting_selected_profile_clears_last_selected_meta() {
        let secrets = InMemorySecretStore::default();
        let mut store = AppConfigStore::in_memory(secrets).expect("store");
        let profile = sample_profile();
        let id = store
            .upsert_profile(&profile, "super_secret_value")
            .expect("upsert");
        store
            .set_last_selected_profile_id(Some(&id))
            .expect("set selected");
        assert_eq!(
            store.last_selected_profile_id().expect("read"),
            Some(id.clone())
        );

        store.delete_profile(&id).expect("delete");
        assert_eq!(store.last_selected_profile_id().expect("read"), None);
    }

    #[test]
    fn close_to_background_meta_round_trip() {
        let secrets = InMemorySecretStore::default();
        let store = AppConfigStore::in_memory(secrets).expect("store");
        assert_eq!(store.close_to_background_preference().expect("read"), None);

        store
            .set_close_to_background_preference(true)
            .expect("set true");
        assert_eq!(
            store.close_to_background_preference().expect("read"),
            Some(true)
        );

        store
            .set_close_to_background_preference(false)
            .expect("set false");
        assert_eq!(
            store.close_to_background_preference().expect("read"),
            Some(false)
        );
    }
}
