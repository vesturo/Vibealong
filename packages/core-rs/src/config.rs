use std::env;
use std::fmt::{Display, Formatter};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use url::Url;

use crate::mapping::{clamp, Curve, Mapping};
use crate::smoothing::OutputTuning;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliOptions {
    pub debug_osc: bool,
    pub debug_unmapped_only: bool,
    pub debug_configured_only: bool,
    pub debug_relay: bool,
    pub discovery_path: Option<String>,
    pub discovery_include_arg_types: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OscListen {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPaths {
    pub session_path: String,
    pub ingest_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugConfig {
    pub log_osc: bool,
    pub log_unmapped_only: bool,
    pub log_configured_only: bool,
    pub log_relay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub file_path: PathBuf,
    pub include_arg_types: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardTarget {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedConfig {
    pub base_url: Url,
    pub creator_username: String,
    pub stream_key: String,
    pub osc_listen: OscListen,
    pub allow_network_osc: bool,
    pub osc_allowed_senders: Vec<IpAddr>,
    pub relay: RelayPaths,
    pub debug: DebugConfig,
    pub discovery: DiscoveryConfig,
    pub forward_targets: Vec<ForwardTarget>,
    pub mappings: Vec<Mapping>,
    pub output: OutputTuning,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InAppConfig {
    pub website_base_url: String,
    pub creator_username: String,
    pub allow_insecure_http: bool,
    pub osc_listen: OscListen,
    #[serde(default)]
    pub allow_network_osc: bool,
    #[serde(default)]
    pub osc_allowed_senders: Vec<String>,
    pub relay: RelayPaths,
    pub debug: DebugConfig,
    pub discovery: DiscoveryConfig,
    pub forward_targets: Vec<ForwardTarget>,
    pub mappings: Vec<Mapping>,
    pub output: OutputTuning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub full_path: PathBuf,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

fn default_config_value() -> Value {
    json!({
        "websiteBaseUrl": "https://dev.vrlewds.com",
        "creatorUsername": "",
        "streamKey": "",
        "allowInsecureHttp": false,
        "allowNetworkOsc": false,
        "oscListen": {
            "host": "127.0.0.1",
            "port": 9001
        },
        "oscAllowedSenders": [],
        "forwardTargets": [],
        "inputs": [
            {
                "address": "/avatar/parameters/SPS_Contact",
                "weight": 1,
                "curve": "linear",
                "invert": false,
                "deadzone": 0.02,
                "min": 0,
                "max": 1
            }
        ],
        "output": {
            "emitHz": 20,
            "attackMs": 55,
            "releaseMs": 220,
            "emaAlpha": 0.35,
            "minDelta": 0.015,
            "heartbeatMs": 1000
        },
        "relay": {
            "sessionPath": "/api/sps/session",
            "ingestPath": "/api/sps/ingest"
        },
        "debug": {
            "logOsc": false,
            "logUnmappedOnly": false,
            "logConfiguredOnly": false,
            "logRelay": false
        },
        "discovery": {
            "enabled": false,
            "filePath": "scripts/vrl-osc-discovered.txt",
            "includeArgTypes": false
        }
    })
}

fn resolve_path(path: &str) -> Result<PathBuf, ConfigError> {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let cwd = env::current_dir()
        .map_err(|e| ConfigError::new(format!("Failed to resolve current directory: {e}")))?;
    Ok(cwd.join(candidate))
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(v) => !v.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn js_to_number(value: Option<&Value>) -> f64 {
    let Some(value) = value else {
        return f64::NAN;
    };
    match value {
        Value::Null => 0.0,
        Value::Bool(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(v) => v.as_f64().unwrap_or(f64::NAN),
        Value::String(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn number_with_nullish_default(value: Option<&Value>, default: f64) -> f64 {
    if value.is_none() || value.is_some_and(Value::is_null) {
        default
    } else {
        js_to_number(value)
    }
}

fn is_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object()?.get(key)
}

fn get_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for key in path {
        cursor = get(cursor, key)?;
    }
    Some(cursor)
}

fn object_or_empty(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn string_or_truthy_fallback(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(v) if js_truthy(v) => value_to_string(v),
        _ => fallback.to_string(),
    }
}

pub fn deep_merge(base: &Value, incoming: &Value) -> Value {
    if !incoming.is_object() || incoming.is_array() {
        return base.clone();
    }
    let mut next = base.as_object().cloned().unwrap_or_default();
    let incoming_obj = incoming.as_object().expect("incoming object checked above");

    for (key, value) in incoming_obj {
        if value.is_array() {
            next.insert(key.clone(), value.clone());
            continue;
        }
        if value.is_object() {
            let base_child = next
                .get(key)
                .filter(|entry| entry.is_object() && !entry.is_array())
                .cloned()
                .unwrap_or_else(|| json!({}));
            next.insert(key.clone(), deep_merge(&base_child, value));
            continue;
        }
        next.insert(key.clone(), value.clone());
    }
    Value::Object(next)
}

pub fn merge_with_defaults(user_config: &Value) -> Value {
    deep_merge(&default_config_value(), user_config)
}

#[deprecated(
    note = "Legacy migration path only. Use app_store::AppConfigStore with normalize_in_app_config."
)]
pub fn load_config(config_path: impl AsRef<Path>) -> Result<LoadedConfig, ConfigError> {
    let full_path = resolve_path(
        config_path
            .as_ref()
            .to_str()
            .ok_or_else(|| ConfigError::new("Config path must be valid UTF-8"))?,
    )?;
    let raw = std::fs::read_to_string(&full_path)
        .map_err(|e| ConfigError::new(format!("Failed to read config file: {e}")))?;
    let user_config: Value = serde_json::from_str(&raw)
        .map_err(|e| ConfigError::new(format!("Failed to parse config JSON: {e}")))?;
    Ok(LoadedConfig {
        full_path,
        value: merge_with_defaults(&user_config),
    })
}

fn normalize_url_config(config: &Value) -> Result<Url, ConfigError> {
    let raw_base = string_or_truthy_fallback(get(config, "websiteBaseUrl"), "");
    let raw_base = raw_base.trim().to_string();
    if raw_base.is_empty() {
        return Err(ConfigError::new("config.websiteBaseUrl is required"));
    }

    let base = Url::parse(&raw_base)
        .map_err(|_| ConfigError::new("config.websiteBaseUrl must be a valid URL"))?;
    let is_local_host = base
        .host_str()
        .is_some_and(|host| host == "127.0.0.1" || host == "localhost");
    let allow_insecure_http = matches!(get(config, "allowInsecureHttp"), Some(Value::Bool(true)));
    if base.scheme() != "https" && !(is_local_host || allow_insecure_http) {
        return Err(ConfigError::new(
            "websiteBaseUrl must use HTTPS unless allowInsecureHttp=true for local testing",
        ));
    }
    Ok(base)
}

fn normalize_mappings(config: &Value) -> Result<Vec<Mapping>, ConfigError> {
    let Some(inputs) = get(config, "inputs").and_then(Value::as_array) else {
        return Err(ConfigError::new(
            "config.inputs must contain at least one OSC mapping",
        ));
    };
    if inputs.is_empty() {
        return Err(ConfigError::new(
            "config.inputs must contain at least one OSC mapping",
        ));
    }

    let mut normalized = Vec::new();
    for entry in inputs {
        let address = entry
            .as_object()
            .and_then(|obj| obj.get("address"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if address.is_empty() || !address.starts_with('/') {
            continue;
        }

        let weight_raw = js_to_number(entry.as_object().and_then(|obj| obj.get("weight")));
        let weight = if weight_raw.is_finite() {
            weight_raw
        } else {
            1.0
        };
        let deadzone = clamp(
            js_to_number(entry.as_object().and_then(|obj| obj.get("deadzone"))),
            0.0,
            1.0,
        );
        let invert = entry
            .as_object()
            .and_then(|obj| obj.get("invert"))
            .is_some_and(js_truthy);
        let curve = entry
            .as_object()
            .and_then(|obj| obj.get("curve"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("linear");
        let min_raw = js_to_number(entry.as_object().and_then(|obj| obj.get("min")));
        let min = if min_raw.is_finite() { min_raw } else { 0.0 };
        let max_raw = js_to_number(entry.as_object().and_then(|obj| obj.get("max")));
        let max = if max_raw.is_finite() { max_raw } else { 1.0 };

        let Some(mapping) = Mapping::new(
            address,
            weight,
            deadzone,
            invert,
            Curve::from_name(curve),
            min,
            max,
        ) else {
            continue;
        };
        normalized.push(mapping);
    }
    Ok(normalized)
}

fn normalize_forward_targets(config: &Value) -> Vec<ForwardTarget> {
    let Some(targets) = get(config, "forwardTargets").and_then(Value::as_array) else {
        return Vec::new();
    };
    targets
        .iter()
        .filter_map(|entry| {
            let host = entry
                .as_object()
                .and_then(|obj| obj.get("host"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let port = js_to_number(entry.as_object().and_then(|obj| obj.get("port")));
            if host.is_empty() || !is_integer(port) || !(1.0..=65_535.0).contains(&port) {
                return None;
            }
            Some(ForwardTarget {
                host,
                port: port as u16,
            })
        })
        .collect()
}

fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim();
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    trimmed.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn parse_allowed_sender_items(raw_items: &[String]) -> Result<Vec<IpAddr>, ConfigError> {
    let mut senders = Vec::<IpAddr>::new();
    for raw in raw_items {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = trimmed
            .parse::<IpAddr>()
            .map_err(|_| ConfigError::new(format!("Invalid sender IP '{trimmed}'")))?;
        if !senders.contains(&parsed) {
            senders.push(parsed);
        }
    }
    Ok(senders)
}

fn normalize_allowed_sender_list_from_value(config: &Value) -> Result<Vec<IpAddr>, ConfigError> {
    let Some(raw) = get(config, "oscAllowedSenders") else {
        return Ok(Vec::new());
    };

    let items = match raw {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        Value::String(value) => value
            .split([',', '\n', '\r', ';'])
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        _ => {
            return Err(ConfigError::new(
                "config.oscAllowedSenders must be an array of IP strings",
            ))
        }
    };
    parse_allowed_sender_items(&items)
}

fn normalize_allowed_sender_list_from_in_app(
    config: &InAppConfig,
) -> Result<Vec<IpAddr>, ConfigError> {
    parse_allowed_sender_items(&config.osc_allowed_senders)
}

fn normalize_output_config(config: &Value) -> OutputTuning {
    let output = object_or_empty(get(config, "output"));
    let emit_hz = clamp(
        number_with_nullish_default(output.get("emitHz"), 20.0),
        2.0,
        60.0,
    );
    let attack_ms = clamp(
        number_with_nullish_default(output.get("attackMs"), 55.0),
        10.0,
        2_000.0,
    );
    let release_ms = clamp(
        number_with_nullish_default(output.get("releaseMs"), 220.0),
        10.0,
        5_000.0,
    );
    let ema_alpha = clamp(
        number_with_nullish_default(output.get("emaAlpha"), 0.35),
        0.01,
        1.0,
    );
    let min_delta = clamp(
        number_with_nullish_default(output.get("minDelta"), 0.015),
        0.0,
        1.0,
    );
    let heartbeat_ms = clamp(
        number_with_nullish_default(output.get("heartbeatMs"), 1_000.0),
        200.0,
        10_000.0,
    );

    OutputTuning {
        emit_hz,
        attack_ms,
        release_ms,
        ema_alpha,
        min_delta,
        heartbeat_ms: heartbeat_ms.trunc() as u64,
    }
}

pub fn normalize_config(
    config: &Value,
    cli_options: &CliOptions,
) -> Result<NormalizedConfig, ConfigError> {
    let base_url = normalize_url_config(config)?;

    let creator_username = string_or_truthy_fallback(get(config, "creatorUsername"), "");
    let creator_username = creator_username.trim().to_string();
    let stream_key = string_or_truthy_fallback(get(config, "streamKey"), "");
    let stream_key = stream_key.trim().to_string();
    if creator_username.is_empty() {
        return Err(ConfigError::new("config.creatorUsername is required"));
    }
    if stream_key.is_empty() {
        return Err(ConfigError::new("config.streamKey is required"));
    }

    let osc_host = string_or_truthy_fallback(get_path(config, &["oscListen", "host"]), "127.0.0.1");
    let osc_host = osc_host.trim().to_string();
    let osc_port_number = {
        let osc_port_raw = get_path(config, &["oscListen", "port"]);
        if osc_port_raw.is_none() || osc_port_raw.is_some_and(Value::is_null) {
            9001.0
        } else {
            js_to_number(osc_port_raw)
        }
    };
    if osc_host.is_empty() {
        return Err(ConfigError::new("config.oscListen.host is required"));
    }
    if !is_integer(osc_port_number) || !(1.0..=65_535.0).contains(&osc_port_number) {
        return Err(ConfigError::new(
            "config.oscListen.port must be a valid UDP port",
        ));
    }
    let osc_listen = OscListen {
        host: osc_host,
        port: osc_port_number as u16,
    };
    let allow_network_osc = get(config, "allowNetworkOsc").is_some_and(js_truthy);
    if !allow_network_osc && !is_loopback_host(&osc_listen.host) {
        return Err(ConfigError::new(
            "config.oscListen.host must be loopback unless allowNetworkOsc=true",
        ));
    }
    let osc_allowed_senders = normalize_allowed_sender_list_from_value(config)?;
    if !allow_network_osc && !osc_allowed_senders.is_empty() {
        return Err(ConfigError::new(
            "config.oscAllowedSenders requires allowNetworkOsc=true",
        ));
    }

    let debug_config = object_or_empty(get(config, "debug"));
    let debug_osc_from_config = debug_config.get("logOsc").is_some_and(js_truthy);
    let debug_unmapped_from_config = debug_config.get("logUnmappedOnly").is_some_and(js_truthy);
    let debug_configured_from_config = debug_config.get("logConfiguredOnly").is_some_and(js_truthy);
    let debug_relay_from_config = debug_config.get("logRelay").is_some_and(js_truthy);

    let requested_unmapped_only = cli_options.debug_unmapped_only || debug_unmapped_from_config;
    let requested_configured_only =
        cli_options.debug_configured_only || debug_configured_from_config;
    let log_configured_only = requested_configured_only;
    let log_unmapped_only = requested_unmapped_only && !requested_configured_only;
    let log_osc =
        cli_options.debug_osc || debug_osc_from_config || log_unmapped_only || log_configured_only;
    let log_relay = cli_options.debug_relay || debug_relay_from_config;
    let debug = DebugConfig {
        log_osc,
        log_unmapped_only,
        log_configured_only,
        log_relay,
    };

    let discovery_config = object_or_empty(get(config, "discovery"));
    let discovery_path_cli = cli_options
        .discovery_path
        .as_deref()
        .map(str::trim)
        .unwrap_or("");
    let discovery_path_raw = if !discovery_path_cli.is_empty() {
        discovery_path_cli.to_string()
    } else {
        discovery_config
            .get("filePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let discovery_path_raw = if discovery_path_raw.is_empty() {
        "scripts/vrl-osc-discovered.txt".to_string()
    } else {
        discovery_path_raw
    };
    let discovery_enabled =
        !discovery_path_cli.is_empty() || discovery_config.get("enabled").is_some_and(js_truthy);
    let discovery_include_arg_types = cli_options.discovery_include_arg_types
        || discovery_config
            .get("includeArgTypes")
            .is_some_and(js_truthy);
    let discovery = DiscoveryConfig {
        enabled: discovery_enabled,
        file_path: resolve_path(&discovery_path_raw)?,
        include_arg_types: discovery_include_arg_types,
    };

    let relay = RelayPaths {
        session_path: string_or_truthy_fallback(
            get_path(config, &["relay", "sessionPath"]),
            "/api/sps/session",
        ),
        ingest_path: string_or_truthy_fallback(
            get_path(config, &["relay", "ingestPath"]),
            "/api/sps/ingest",
        ),
    };

    Ok(NormalizedConfig {
        base_url,
        creator_username,
        stream_key,
        osc_listen,
        allow_network_osc,
        osc_allowed_senders,
        relay,
        debug,
        discovery,
        forward_targets: normalize_forward_targets(config),
        mappings: normalize_mappings(config)?,
        output: normalize_output_config(config),
    })
}

pub fn normalize_in_app_config(
    config: &InAppConfig,
    stream_key: &str,
) -> Result<NormalizedConfig, ConfigError> {
    let raw_base = config.website_base_url.trim();
    if raw_base.is_empty() {
        return Err(ConfigError::new("config.websiteBaseUrl is required"));
    }
    let base_url = Url::parse(raw_base)
        .map_err(|_| ConfigError::new("config.websiteBaseUrl must be a valid URL"))?;
    let is_local_host = base_url
        .host_str()
        .is_some_and(|host| host == "127.0.0.1" || host == "localhost");
    if base_url.scheme() != "https" && !(is_local_host || config.allow_insecure_http) {
        return Err(ConfigError::new(
            "websiteBaseUrl must use HTTPS unless allowInsecureHttp=true for local testing",
        ));
    }

    let creator_username = config.creator_username.trim().to_string();
    let stream_key = stream_key.trim().to_string();
    if creator_username.is_empty() {
        return Err(ConfigError::new("config.creatorUsername is required"));
    }
    if stream_key.is_empty() {
        return Err(ConfigError::new("config.streamKey is required"));
    }

    let osc_host = config.osc_listen.host.trim().to_string();
    if osc_host.is_empty() {
        return Err(ConfigError::new("config.oscListen.host is required"));
    }
    if config.osc_listen.port == 0 {
        return Err(ConfigError::new(
            "config.oscListen.port must be a valid UDP port",
        ));
    }
    if !config.allow_network_osc && !is_loopback_host(&osc_host) {
        return Err(ConfigError::new(
            "config.oscListen.host must be loopback unless allowNetworkOsc=true",
        ));
    }
    let osc_allowed_senders = normalize_allowed_sender_list_from_in_app(config)?;
    if !config.allow_network_osc && !osc_allowed_senders.is_empty() {
        return Err(ConfigError::new(
            "config.oscAllowedSenders requires allowNetworkOsc=true",
        ));
    }

    let mappings = config
        .mappings
        .iter()
        .filter_map(|mapping| {
            let address = mapping.address.trim().to_string();
            if address.is_empty() || !address.starts_with('/') {
                return None;
            }
            let weight = if mapping.weight.is_finite() {
                mapping.weight
            } else {
                1.0
            };
            let deadzone = clamp(mapping.deadzone, 0.0, 1.0);
            let min = if mapping.min.is_finite() {
                mapping.min
            } else {
                0.0
            };
            let max = if mapping.max.is_finite() {
                mapping.max
            } else {
                1.0
            };
            Mapping::new(
                address,
                weight,
                deadzone,
                mapping.invert,
                mapping.curve,
                min,
                max,
            )
        })
        .collect::<Vec<_>>();

    let forward_targets = config
        .forward_targets
        .iter()
        .filter_map(|target| {
            let host = target.host.trim().to_string();
            if host.is_empty() || target.port == 0 {
                return None;
            }
            Some(ForwardTarget {
                host,
                port: target.port,
            })
        })
        .collect::<Vec<_>>();

    let requested_unmapped_only = config.debug.log_unmapped_only;
    let requested_configured_only = config.debug.log_configured_only;
    let log_configured_only = requested_configured_only;
    let log_unmapped_only = requested_unmapped_only && !requested_configured_only;
    let log_osc = config.debug.log_osc || log_unmapped_only || log_configured_only;

    let discovery_path_raw = if config.discovery.file_path.as_os_str().is_empty() {
        "scripts/vrl-osc-discovered.txt".to_string()
    } else {
        config.discovery.file_path.to_string_lossy().to_string()
    };

    let session_path = {
        let value = config.relay.session_path.trim();
        if value.is_empty() {
            "/api/sps/session".to_string()
        } else {
            value.to_string()
        }
    };
    let ingest_path = {
        let value = config.relay.ingest_path.trim();
        if value.is_empty() {
            "/api/sps/ingest".to_string()
        } else {
            value.to_string()
        }
    };

    Ok(NormalizedConfig {
        base_url,
        creator_username,
        stream_key,
        osc_listen: OscListen {
            host: osc_host,
            port: config.osc_listen.port,
        },
        allow_network_osc: config.allow_network_osc,
        osc_allowed_senders,
        relay: RelayPaths {
            session_path,
            ingest_path,
        },
        debug: DebugConfig {
            log_osc,
            log_unmapped_only,
            log_configured_only,
            log_relay: config.debug.log_relay,
        },
        discovery: DiscoveryConfig {
            enabled: config.discovery.enabled,
            file_path: resolve_path(&discovery_path_raw)?,
            include_arg_types: config.discovery.include_arg_types,
        },
        forward_targets,
        mappings,
        output: config.output.sanitize(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::Curve;

    fn merged_minimal_valid() -> Value {
        merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "key_123"
        }))
    }

    fn sample_in_app_config() -> InAppConfig {
        InAppConfig {
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
                file_path: PathBuf::from("scripts/vrl-osc-discovered.txt"),
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
        }
    }

    #[test]
    fn deep_merge_merges_objects_and_replaces_arrays() {
        let merged = merge_with_defaults(&json!({
            "oscListen": { "port": 6010 },
            "inputs": [{ "address": "/avatar/parameters/X", "weight": 2 }]
        }));
        assert_eq!(
            get_path(&merged, &["oscListen", "host"]).and_then(Value::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(
            js_to_number(get_path(&merged, &["oscListen", "port"])),
            6010.0
        );
        let inputs_len = get(&merged, "inputs")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or_default();
        assert_eq!(inputs_len, 1);
    }

    #[test]
    fn requires_creator_and_stream_key() {
        let merged = merge_with_defaults(&json!({}));
        let error = normalize_config(&merged, &CliOptions::default()).expect_err("must fail");
        assert_eq!(error.message, "config.creatorUsername is required");
    }

    #[test]
    fn enforces_https_unless_explicitly_allowed() {
        let merged = merge_with_defaults(&json!({
            "websiteBaseUrl": "http://example.com",
            "creatorUsername": "Vee",
            "streamKey": "k"
        }));
        let error = normalize_config(&merged, &CliOptions::default()).expect_err("must fail");
        assert_eq!(
            error.message,
            "websiteBaseUrl must use HTTPS unless allowInsecureHttp=true for local testing"
        );

        let local_ok = merge_with_defaults(&json!({
            "websiteBaseUrl": "http://localhost:3000",
            "creatorUsername": "Vee",
            "streamKey": "k"
        }));
        assert!(normalize_config(&local_ok, &CliOptions::default()).is_ok());
    }

    #[test]
    fn rejects_non_loopback_osc_host_without_explicit_network_flag() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "oscListen": { "host": "0.0.0.0", "port": 9001 }
        }));
        let error = normalize_config(&merged, &CliOptions::default()).expect_err("must fail");
        assert_eq!(
            error.message,
            "config.oscListen.host must be loopback unless allowNetworkOsc=true"
        );
    }

    #[test]
    fn parses_sender_allowlist_when_network_mode_enabled() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "allowNetworkOsc": true,
            "oscListen": { "host": "0.0.0.0", "port": 9001 },
            "oscAllowedSenders": ["127.0.0.1", "192.168.1.20"]
        }));
        let normalized = normalize_config(&merged, &CliOptions::default()).expect("valid config");
        assert!(normalized.allow_network_osc);
        assert_eq!(normalized.osc_allowed_senders.len(), 2);
    }

    #[test]
    fn validates_osc_port() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "oscListen": { "port": 0 }
        }));
        let error = normalize_config(&merged, &CliOptions::default()).expect_err("must fail");
        assert_eq!(
            error.message,
            "config.oscListen.port must be a valid UDP port"
        );
    }

    #[test]
    fn debug_precedence_matches_cli_rules() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "debug": {
                "logUnmappedOnly": true,
                "logConfiguredOnly": false
            }
        }));
        let cli = CliOptions {
            debug_configured_only: true,
            ..CliOptions::default()
        };
        let normalized = normalize_config(&merged, &cli).expect("valid config");
        assert!(normalized.debug.log_osc);
        assert!(normalized.debug.log_configured_only);
        assert!(!normalized.debug.log_unmapped_only);
    }

    #[test]
    fn discovery_cli_path_enables_and_resolves() {
        let merged = merged_minimal_valid();
        let cli = CliOptions {
            discovery_path: Some("tmp/discovered.txt".to_string()),
            discovery_include_arg_types: true,
            ..CliOptions::default()
        };
        let normalized = normalize_config(&merged, &cli).expect("valid config");
        assert!(normalized.discovery.enabled);
        assert!(normalized.discovery.include_arg_types);
        assert!(normalized
            .discovery
            .file_path
            .ends_with(Path::new("tmp").join("discovered.txt")));
    }

    #[test]
    fn mappings_are_normalized_and_invalid_addresses_dropped() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "inputs": [
                { "address": "invalid", "weight": 1 },
                {
                    "address": "/avatar/parameters/Valid",
                    "weight": "2",
                    "curve": "easeOutQuad",
                    "invert": true,
                    "deadzone": 1.2,
                    "min": 4,
                    "max": 4
                }
            ]
        }));
        let normalized = normalize_config(&merged, &CliOptions::default()).expect("valid config");
        assert_eq!(normalized.mappings.len(), 1);
        let mapping = &normalized.mappings[0];
        assert_eq!(mapping.address, "/avatar/parameters/Valid");
        assert_eq!(mapping.weight, 2.0);
        assert_eq!(mapping.deadzone, 1.0);
        assert!(mapping.invert);
        assert_eq!(mapping.curve, Curve::EaseOutQuad);
        assert_eq!(mapping.min, 4.0);
        assert_eq!(mapping.max, 5.0);
    }

    #[test]
    fn forward_targets_filter_invalid_entries() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "forwardTargets": [
                { "host": "127.0.0.1", "port": 9000 },
                { "host": "", "port": 9001 },
                { "host": "localhost", "port": 70000 }
            ]
        }));
        let normalized = normalize_config(&merged, &CliOptions::default()).expect("valid config");
        assert_eq!(normalized.forward_targets.len(), 1);
        assert_eq!(normalized.forward_targets[0].port, 9000);
    }

    #[test]
    fn output_clamps_match_js_limits() {
        let merged = merge_with_defaults(&json!({
            "creatorUsername": "Vee",
            "streamKey": "k",
            "output": {
                "emitHz": 500,
                "attackMs": 1,
                "releaseMs": 9000,
                "emaAlpha": 0,
                "minDelta": -5,
                "heartbeatMs": 50
            }
        }));
        let normalized = normalize_config(&merged, &CliOptions::default()).expect("valid config");
        assert_eq!(normalized.output.emit_hz, 60.0);
        assert_eq!(normalized.output.attack_ms, 10.0);
        assert_eq!(normalized.output.release_ms, 5000.0);
        assert_eq!(normalized.output.ema_alpha, 0.01);
        assert_eq!(normalized.output.min_delta, 0.0);
        assert_eq!(normalized.output.heartbeat_ms, 200);
    }

    #[test]
    fn output_defaults_match_js_defaults() {
        let merged = merged_minimal_valid();
        let normalized = normalize_config(&merged, &CliOptions::default()).expect("valid config");
        assert_eq!(normalized.output.emit_hz, 20.0);
        assert_eq!(normalized.output.attack_ms, 55.0);
        assert_eq!(normalized.output.release_ms, 220.0);
        assert_eq!(normalized.output.ema_alpha, 0.35);
        assert_eq!(normalized.output.min_delta, 0.015);
        assert_eq!(normalized.output.heartbeat_ms, 1000);
    }

    #[test]
    fn in_app_normalization_validates_and_sanitizes() {
        let mut in_app = sample_in_app_config();
        in_app.output.emit_hz = 500.0;
        in_app.output.attack_ms = 1.0;
        in_app.debug.log_configured_only = true;
        in_app.debug.log_unmapped_only = true;
        in_app.discovery.file_path = PathBuf::new();

        let normalized =
            normalize_in_app_config(&in_app, "stream_key_123").expect("valid in-app config");
        assert_eq!(normalized.output.emit_hz, 60.0);
        assert_eq!(normalized.output.attack_ms, 10.0);
        assert!(normalized.debug.log_configured_only);
        assert!(!normalized.debug.log_unmapped_only);
        assert!(normalized
            .discovery
            .file_path
            .ends_with("scripts/vrl-osc-discovered.txt"));
    }

    #[test]
    fn in_app_normalization_rejects_missing_stream_key() {
        let in_app = sample_in_app_config();
        let error = normalize_in_app_config(&in_app, "").expect_err("must fail");
        assert_eq!(error.message, "config.streamKey is required");
    }

    #[test]
    fn in_app_normalization_allows_empty_mappings() {
        let mut in_app = sample_in_app_config();
        in_app.mappings.clear();
        let normalized =
            normalize_in_app_config(&in_app, "stream_key_123").expect("valid in-app config");
        assert!(normalized.mappings.is_empty());
    }
}
