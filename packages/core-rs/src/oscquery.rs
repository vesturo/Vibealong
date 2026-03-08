use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::Value;

const OSCQUERY_SERVICE: &str = "_oscjson._tcp.local.";
const AVATAR_PARAMETERS_PATH: &str = "/avatar/parameters";
const AVATAR_PARAMETERS_PREFIX: &str = "/avatar/parameters/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscQueryEndpoint {
    pub oscquery_host: String,
    pub oscquery_port: u16,
    pub osc_host: String,
    pub osc_port: u16,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OscQueryStatus {
    pub endpoint: Option<OscQueryEndpoint>,
    pub last_error: Option<String>,
}

pub struct OscQueryClient {
    http: reqwest::blocking::Client,
    status: OscQueryStatus,
}

impl OscQueryClient {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            status: OscQueryStatus::default(),
        })
    }

    pub fn status(&self) -> OscQueryStatus {
        self.status.clone()
    }

    pub fn discover(&mut self, log_hint_port: Option<u16>) -> Option<OscQueryEndpoint> {
        if let Some(previous) = self.status.endpoint.clone() {
            match check_oscquery_port(&self.http, &previous.oscquery_host, previous.oscquery_port) {
                Ok(endpoint) => {
                    let endpoint = OscQueryEndpoint {
                        source: if previous.source.trim().is_empty() {
                            "cached".to_string()
                        } else {
                            previous.source
                        },
                        ..endpoint
                    };
                    self.status.endpoint = Some(endpoint.clone());
                    self.status.last_error = None;
                    return Some(endpoint);
                }
                Err(err) => {
                    self.status.last_error = Some(format!("cached endpoint check failed: {err}"));
                }
            }
        }

        if let Some(endpoint) = discover_via_mdns(&self.http, Duration::from_secs(5)) {
            self.status.endpoint = Some(endpoint.clone());
            self.status.last_error = None;
            return Some(endpoint);
        }

        if let Some(port) = log_hint_port {
            match check_oscquery_port(&self.http, "127.0.0.1", port) {
                Ok(endpoint) => {
                    let endpoint = OscQueryEndpoint {
                        source: "vrchat-log".to_string(),
                        ..endpoint
                    };
                    self.status.endpoint = Some(endpoint.clone());
                    self.status.last_error = None;
                    return Some(endpoint);
                }
                Err(err) => {
                    self.status.last_error = Some(err);
                }
            }
        }

        if self.status.last_error.is_none() {
            self.status.last_error = Some("OSCQuery endpoint not discovered".to_string());
        }
        None
    }

    pub fn fetch_bulk_values(&self) -> Result<HashMap<String, Value>, String> {
        self.fetch_values_at_path("/")
    }

    pub fn fetch_values_at_path(&self, path: &str) -> Result<HashMap<String, Value>, String> {
        let endpoint = self
            .status
            .endpoint
            .as_ref()
            .ok_or_else(|| "OSCQuery endpoint is not discovered".to_string())?;
        let query_path = normalize_query_path(path);
        let url = endpoint_url(&endpoint.oscquery_host, endpoint.oscquery_port, &query_path);
        let json: Value = self
            .http
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|e| e.to_string())?
            .json()
            .map_err(|e| e.to_string())?;
        let mut values = HashMap::new();
        collect_oscquery_values(&json, &mut values);
        Ok(filter_values_under_path(values, &query_path))
    }

    pub fn fetch_avatar_parameters(&self) -> Result<HashMap<String, Value>, String> {
        let values = self.fetch_values_at_path(AVATAR_PARAMETERS_PATH)?;
        let mut output = HashMap::new();
        for (path, value) in values {
            if let Some(param_name) = avatar_param_name_from_full_path(&path) {
                output.insert(param_name.to_string(), value);
            }
        }
        Ok(output)
    }
}

fn discover_via_mdns(
    http: &reqwest::blocking::Client,
    timeout: Duration,
) -> Option<OscQueryEndpoint> {
    let daemon = ServiceDaemon::new().ok()?;
    let receiver = daemon.browse(OSCQUERY_SERVICE).ok()?;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                let mut hosts = info
                    .get_addresses()
                    .iter()
                    .map(|address| ip_to_host(*address))
                    .collect::<Vec<_>>();
                hosts.sort_by_key(|host| host_discovery_priority(host));
                hosts.dedup();
                for host in hosts {
                    if let Ok(endpoint) = check_oscquery_port(http, &host, port) {
                        let endpoint = OscQueryEndpoint {
                            source: "mdns".to_string(),
                            ..endpoint
                        };
                        let _ = daemon.shutdown();
                        return Some(endpoint);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }

    let _ = daemon.shutdown();
    None
}

fn ip_to_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6.to_string(),
    }
}

fn host_discovery_priority(host: &str) -> u8 {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) if v4.is_loopback() => 0,
        Ok(IpAddr::V6(v6)) if v6.is_loopback() => 1,
        Ok(IpAddr::V4(_)) => 2,
        Ok(IpAddr::V6(_)) => 3,
        Err(_) => 4,
    }
}

fn host_for_url(host: &str) -> String {
    let host = host.trim();
    if host.starts_with('[') && host.ends_with(']') {
        return host.to_string();
    }
    if host.contains(':') {
        return format!("[{host}]");
    }
    host.to_string()
}

fn endpoint_url(host: &str, port: u16, path: &str) -> String {
    format!("http://{}:{}{}", host_for_url(host), port, path)
}

fn normalize_query_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn filter_values_under_path(values: HashMap<String, Value>, path: &str) -> HashMap<String, Value> {
    if path == "/" {
        return values;
    }
    let prefix = format!("{path}/");
    values
        .into_iter()
        .filter(|(key, _)| key == path || key.starts_with(&prefix))
        .collect()
}

pub fn avatar_param_name_from_full_path(path: &str) -> Option<&str> {
    path.strip_prefix(AVATAR_PARAMETERS_PREFIX)
        .and_then(|value| {
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        })
}

fn check_oscquery_port(
    http: &reqwest::blocking::Client,
    host: &str,
    port: u16,
) -> Result<OscQueryEndpoint, String> {
    if port == 0 {
        return Err("invalid OSCQuery port 0".to_string());
    }
    let url = endpoint_url(host, port, "/?HOST_INFO");
    let json: Value = http
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;

    let name = json.get("NAME").and_then(Value::as_str).unwrap_or_default();
    if !name.starts_with("VRChat-Client-") {
        return Err(format!(
            "endpoint host info NAME is not VRChat client: {name}"
        ));
    }

    let osc_host = json
        .get("OSC_IP")
        .and_then(Value::as_str)
        .ok_or_else(|| "HOST_INFO missing OSC_IP".to_string())?;

    let osc_port = json
        .get("OSC_PORT")
        .and_then(|value| {
            if let Some(num) = value.as_u64() {
                return u16::try_from(num).ok();
            }
            if let Some(text) = value.as_str() {
                return text.parse::<u16>().ok();
            }
            None
        })
        .ok_or_else(|| "HOST_INFO missing OSC_PORT".to_string())?;
    if osc_port == 0 {
        return Err("HOST_INFO OSC_PORT must be > 0".to_string());
    }

    Ok(OscQueryEndpoint {
        oscquery_host: host.to_string(),
        oscquery_port: port,
        osc_host: osc_host.to_string(),
        osc_port,
        source: String::new(),
    })
}

pub fn collect_oscquery_values(input: &Value, output: &mut HashMap<String, Value>) {
    match input {
        Value::Array(items) => {
            for child in items {
                collect_oscquery_values(child, output);
            }
        }
        Value::Object(map) => {
            if let (Some(path), Some(value)) = (
                map.get("FULL_PATH").and_then(Value::as_str),
                map.get("VALUE").and_then(Value::as_array),
            ) {
                if let Some(first) = value.first() {
                    output.insert(path.to_string(), first.clone());
                }
            }
            for child in map.values() {
                collect_oscquery_values(child, output);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_values_extracts_full_path_nodes() {
        let json = serde_json::json!({
            "FULL_PATH": "/avatar/parameters/A",
            "VALUE": [0.5],
            "CONTENTS": {
                "child": {
                    "FULL_PATH": "/avatar/parameters/B",
                    "VALUE": [true]
                }
            }
        });

        let mut out = HashMap::new();
        collect_oscquery_values(&json, &mut out);
        assert_eq!(
            out.get("/avatar/parameters/A"),
            Some(&serde_json::json!(0.5))
        );
        assert_eq!(
            out.get("/avatar/parameters/B"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn avatar_param_name_from_full_path_strips_prefix() {
        assert_eq!(
            avatar_param_name_from_full_path("/avatar/parameters/SPS_Contact"),
            Some("SPS_Contact")
        );
        assert_eq!(
            avatar_param_name_from_full_path("/avatar/parameters/"),
            None
        );
        assert_eq!(avatar_param_name_from_full_path("/other/path"), None);
    }

    #[test]
    fn normalize_query_path_handles_empty_and_slashes() {
        assert_eq!(normalize_query_path(""), "/");
        assert_eq!(normalize_query_path("/"), "/");
        assert_eq!(
            normalize_query_path("avatar/parameters"),
            "/avatar/parameters"
        );
        assert_eq!(
            normalize_query_path("/avatar/parameters/"),
            "/avatar/parameters"
        );
    }

    #[test]
    fn filter_values_under_path_keeps_matching_subtree() {
        let mut input = HashMap::new();
        input.insert("/avatar/parameters/A".to_string(), serde_json::json!(1.0));
        input.insert("/avatar/parameters/B".to_string(), serde_json::json!(false));
        input.insert("/chatbox/input".to_string(), serde_json::json!("hello"));

        let filtered = filter_values_under_path(input, "/avatar/parameters");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains_key("/avatar/parameters/A"));
        assert!(filtered.contains_key("/avatar/parameters/B"));
    }

    #[test]
    fn host_for_url_wraps_ipv6() {
        assert_eq!(host_for_url("127.0.0.1"), "127.0.0.1");
        assert_eq!(host_for_url("::1"), "[::1]");
        assert_eq!(host_for_url("[::1]"), "[::1]");
    }
}
