use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, UdpSocket};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::NormalizedConfig;
use crate::engine::BridgeEngine;
use crate::mapping::Mapping;
use crate::osc::{extract_numeric_arg, parse_osc_packet, OscArg, OscMessage};
use crate::relay::{RelayError, RelayEvent, RelayPublisher, RelaySource};
use crate::smoothing::SmootherState;

const LOG_HISTORY_LIMIT: usize = 1000;
const AVATAR_PARAM_HISTORY_LIMIT: usize = 4096;
const DISCOVERY_ENTRY_MAX: usize = 5000;
const DISCOVERY_ENTRY_MAX_LEN: usize = 256;
const RELAY_RETRY_BASE_MS: i64 = 1200;
const RELAY_RETRY_MAX_MS: i64 = 45_000;
const RELAY_AUTH_RETRY_BASE_MS: i64 = 4_000;
const RELAY_AUTH_RETRY_MAX_MS: i64 = 120_000;
const RELAY_THROTTLE_RETRY_BASE_MS: i64 = 8_000;
const RELAY_THROTTLE_RETRY_MAX_MS: i64 = 180_000;

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeParamValue {
    Number(f64),
    Bool(bool),
    Text(String),
}

impl RuntimeParamValue {
    pub fn display_string(&self) -> String {
        match self {
            Self::Number(value) => format!("{value:.6}"),
            Self::Bool(value) => value.to_string(),
            Self::Text(value) => value.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLogLine {
    pub ts_ms: i64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeSnapshot {
    pub running: bool,
    pub started_at_ms: i64,
    pub last_tick_ms: i64,
    pub seq: u64,
    pub target_intensity: f64,
    pub current_intensity: f64,
    pub peak_intensity: f64,
    pub last_sent_at_ms: i64,
    pub relay_connected: bool,
    pub auto_disengaged: bool,
    pub last_error: String,
    pub last_source_address: String,
    pub last_source_arg_type: String,
    pub osc_packets_received: u64,
    pub osc_messages_received: u64,
    pub mapped_messages_received: u64,
    pub unmapped_messages_received: u64,
    pub discovery_entries_written: u64,
    pub last_osc_from: String,
}

impl Default for RuntimeSnapshot {
    fn default() -> Self {
        Self {
            running: false,
            started_at_ms: 0,
            last_tick_ms: 0,
            seq: 0,
            target_intensity: 0.0,
            current_intensity: 0.0,
            peak_intensity: 0.0,
            last_sent_at_ms: 0,
            relay_connected: false,
            auto_disengaged: false,
            last_error: String::new(),
            last_source_address: String::new(),
            last_source_arg_type: "f".to_string(),
            osc_packets_received: 0,
            osc_messages_received: 0,
            mapped_messages_received: 0,
            unmapped_messages_received: 0,
            discovery_entries_written: 0,
            last_osc_from: String::new(),
        }
    }
}

#[derive(Debug)]
pub struct BridgeRuntimeHandle {
    stop_tx: Sender<()>,
    join_handle: Option<JoinHandle<()>>,
    snapshot: Arc<Mutex<RuntimeSnapshot>>,
    logs: Arc<Mutex<VecDeque<RuntimeLogLine>>>,
    avatar_params: Arc<Mutex<HashMap<String, RuntimeParamValue>>>,
}

impl BridgeRuntimeHandle {
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot
            .lock()
            .expect("snapshot mutex poisoned")
            .clone()
    }

    pub fn recent_logs(&self, max_lines: usize) -> Vec<RuntimeLogLine> {
        if max_lines == 0 {
            return Vec::new();
        }
        let logs = self.logs.lock().expect("logs mutex poisoned");
        let start = logs.len().saturating_sub(max_lines);
        logs.iter().skip(start).cloned().collect()
    }

    pub fn avatar_params(&self) -> Vec<(String, RuntimeParamValue)> {
        let params = self
            .avatar_params
            .lock()
            .expect("avatar params mutex poisoned");
        let mut out = params
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn stop(mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for BridgeRuntimeHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub struct BridgeRuntime;

impl BridgeRuntime {
    pub fn start(
        config: NormalizedConfig,
        relay: Box<dyn RelayPublisher>,
    ) -> Result<BridgeRuntimeHandle, String> {
        let listen_socket = UdpSocket::bind(format!(
            "{}:{}",
            config.osc_listen.host, config.osc_listen.port
        ))
        .map_err(|e| format!("Failed to bind OSC listen socket: {e}"))?;
        listen_socket
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to set nonblocking listen socket: {e}"))?;
        let forward_socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind forward socket: {e}"))?;

        let now = unix_ms_now();
        let snapshot = Arc::new(Mutex::new(RuntimeSnapshot {
            running: true,
            started_at_ms: now,
            last_tick_ms: now,
            ..RuntimeSnapshot::default()
        }));
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        let avatar_params = Arc::new(Mutex::new(HashMap::new()));

        let (stop_tx, stop_rx) = channel();
        let snapshot_clone = Arc::clone(&snapshot);
        let logs_clone = Arc::clone(&logs);
        let avatar_params_clone = Arc::clone(&avatar_params);
        let join_handle = thread::Builder::new()
            .name("vibealong-runtime".to_string())
            .spawn(move || {
                run_loop(
                    config,
                    relay,
                    listen_socket,
                    forward_socket,
                    stop_rx,
                    snapshot_clone,
                    logs_clone,
                    avatar_params_clone,
                )
            })
            .map_err(|e| format!("Failed to spawn runtime thread: {e}"))?;

        Ok(BridgeRuntimeHandle {
            stop_tx,
            join_handle: Some(join_handle),
            snapshot,
            logs,
            avatar_params,
        })
    }
}

fn run_loop(
    config: NormalizedConfig,
    mut relay: Box<dyn RelayPublisher>,
    listen_socket: UdpSocket,
    forward_socket: UdpSocket,
    stop_rx: Receiver<()>,
    snapshot: Arc<Mutex<RuntimeSnapshot>>,
    logs: Arc<Mutex<VecDeque<RuntimeLogLine>>>,
    avatar_params: Arc<Mutex<HashMap<String, RuntimeParamValue>>>,
) {
    push_log(
        &logs,
        "INFO",
        format!(
            "Bridge runtime started (OSC {}:{}, mappings={}, forwardTargets={})",
            config.osc_listen.host,
            config.osc_listen.port,
            config.mappings.len(),
            config.forward_targets.len()
        ),
    );

    let mut discovery_entries = if config.discovery.enabled {
        match load_discovery_entries(&config.discovery.file_path) {
            Ok(entries) => {
                push_log(
                    &logs,
                    "INFO",
                    format!(
                        "OSC discovery enabled at {} (loaded {} entries)",
                        config.discovery.file_path.display(),
                        entries.len()
                    ),
                );
                entries
            }
            Err(err) => {
                push_log(
                    &logs,
                    "WARN",
                    format!("Failed to load discovery file: {err}"),
                );
                HashSet::new()
            }
        }
    } else {
        HashSet::new()
    };

    let mut engine = BridgeEngine::new(config.mappings.clone());
    let mut smoother = SmootherState::default();
    let mut seq = 0_u64;
    let mut last_tick_ms = unix_ms_now();
    let tick_ms = std::cmp::max(16, (1000.0 / config.output.emit_hz).round() as u64);
    let tick_duration = Duration::from_millis(tick_ms);
    let mut packet_buf = [0_u8; 65_535];
    let mut last_relay_ack_log_at_ms = 0_i64;
    let mut relay_retry_after_ms = 0_i64;
    let mut relay_backoff_attempts = 0_u32;
    let mut last_reconnect_log_at_ms = 0_i64;
    let mut blocked_sender_log_once = HashSet::<IpAddr>::new();

    loop {
        match stop_rx.try_recv() {
            Ok(_) | Err(TryRecvError::Disconnected) => {
                push_log(&logs, "INFO", "Bridge runtime stop requested");
                break;
            }
            Err(TryRecvError::Empty) => {}
        }

        loop {
            let recv_result = listen_socket.recv_from(&mut packet_buf);
            let Ok((packet_size, from)) = recv_result else {
                if let Err(err) = recv_result {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        break;
                    }
                    set_runtime_error(&snapshot, format!("listen socket error: {err}"));
                    push_log(&logs, "ERROR", format!("listen socket error: {err}"));
                }
                break;
            };

            let packet = &packet_buf[..packet_size];
            let sender_ip = from.ip();
            if !is_sender_allowed(&config, sender_ip) {
                if blocked_sender_log_once.insert(sender_ip) {
                    push_log(
                        &logs,
                        "WARN",
                        format!("ignored OSC packet from unauthorized sender {sender_ip}"),
                    );
                }
                continue;
            }
            if !config.forward_targets.is_empty() {
                for target in &config.forward_targets {
                    if let Err(err) =
                        forward_socket.send_to(packet, format!("{}:{}", target.host, target.port))
                    {
                        push_log(
                            &logs,
                            "WARN",
                            format!(
                                "forward target {}:{} send failed: {err}",
                                target.host, target.port
                            ),
                        );
                    }
                }
            }

            let messages = parse_osc_packet(packet);
            let mut mapped_count = 0_u64;
            let mut unmapped_count = 0_u64;

            if !messages.is_empty() {
                let _ = engine.process_messages(&messages);
                for message in &messages {
                    update_avatar_param_cache(&avatar_params, message);
                    let (mapping_state, message_mapped) =
                        classify_message_mapping(message, &config.mappings);
                    if message_mapped {
                        mapped_count += 1;
                    } else {
                        unmapped_count += 1;
                    }
                    log_osc_debug(&config, &logs, message, mapping_state);
                    if config.discovery.enabled {
                        let wrote =
                            record_discovery_entry(&config, &logs, &mut discovery_entries, message);
                        if wrote {
                            let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
                            lock.discovery_entries_written += 1;
                        }
                    }
                }
            }

            let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
            lock.osc_packets_received += 1;
            lock.osc_messages_received += messages.len() as u64;
            lock.mapped_messages_received += mapped_count;
            lock.unmapped_messages_received += unmapped_count;
            lock.last_osc_from = from.to_string();
        }

        let now_ms = unix_ms_now();
        smoother.target_intensity = engine.target_intensity;
        smoother.step(now_ms as u64, last_tick_ms as u64, config.output);
        last_tick_ms = now_ms;

        if now_ms < relay_retry_after_ms {
            let remaining_ms = relay_retry_after_ms - now_ms;
            {
                let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
                lock.relay_connected = false;
                lock.auto_disengaged = false;
                lock.last_error = format!(
                    "Relay reconnect backoff active (retry in {:.1}s)",
                    remaining_ms as f64 / 1000.0
                );
            }
            if now_ms - last_reconnect_log_at_ms >= 5000 {
                last_reconnect_log_at_ms = now_ms;
                push_log(
                    &logs,
                    "WARN",
                    format!(
                        "relay backoff active; next reconnect attempt in {:.1}s",
                        remaining_ms as f64 / 1000.0
                    ),
                );
            }
        } else if smoother.should_emit(now_ms as u64, config.output) {
            seq += 1;
            let source_arg_type = if engine.last_source.arg_type.trim().is_empty() {
                "f".to_string()
            } else {
                engine.last_source.arg_type.clone()
            };
            let event = RelayEvent {
                seq,
                ts: relay.next_client_ts_ms(now_ms),
                intensity: smoother.current_intensity,
                peak: smoother.peak_intensity,
                raw: engine.target_intensity,
                source: RelaySource {
                    address: engine.last_source.address.clone(),
                    arg_type: source_arg_type,
                },
            };

            match relay.push_event(&event, now_ms) {
                Ok(_) => {
                    relay_retry_after_ms = 0;
                    relay_backoff_attempts = 0;
                    smoother.mark_emitted(now_ms as u64);
                    let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
                    lock.last_error.clear();
                    lock.relay_connected = true;
                    lock.auto_disengaged = false;
                    lock.last_sent_at_ms = now_ms;
                    if config.debug.log_relay && now_ms - last_relay_ack_log_at_ms >= 1000 {
                        last_relay_ack_log_at_ms = now_ms;
                        push_log(
                            &logs,
                            "RELAY",
                            format!(
                                "ack seq={} intensity={:.3} peak={:.3} raw={:.3}",
                                event.seq, event.intensity, event.peak, event.raw
                            ),
                        );
                    }
                }
                Err(err) => {
                    relay_backoff_attempts = relay_backoff_attempts.saturating_add(1);
                    let retry_delay_ms = retry_backoff_ms_for_error(&err, relay_backoff_attempts);
                    relay_retry_after_ms = now_ms + retry_delay_ms;
                    let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
                    lock.last_error =
                        format!("{err} (retry in {:.1}s)", retry_delay_ms as f64 / 1000.0);
                    lock.relay_connected = false;
                    lock.auto_disengaged = false;
                    push_log(&logs, "WARN", lock.last_error.clone());
                }
            }
        }

        {
            let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
            lock.running = true;
            lock.last_tick_ms = now_ms;
            lock.seq = seq;
            lock.target_intensity = engine.target_intensity;
            lock.current_intensity = smoother.current_intensity;
            lock.peak_intensity = smoother.peak_intensity;
            lock.last_source_address = engine.last_source.address.clone();
            lock.last_source_arg_type = engine.last_source.arg_type.clone();
        }
        thread::sleep(tick_duration);
    }

    let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
    lock.running = false;
    push_log(&logs, "INFO", "Bridge runtime stopped");
}

fn exponential_backoff_ms(base_ms: i64, max_ms: i64, attempts: u32) -> i64 {
    let capped_attempts = attempts.saturating_sub(1).min(10);
    let factor = 1_i64 << capped_attempts;
    base_ms.saturating_mul(factor).min(max_ms).max(base_ms)
}

fn retry_backoff_ms_for_error(err: &RelayError, attempts: u32) -> i64 {
    match err {
        RelayError::Throttled => exponential_backoff_ms(
            RELAY_THROTTLE_RETRY_BASE_MS,
            RELAY_THROTTLE_RETRY_MAX_MS,
            attempts,
        ),
        RelayError::RelayTokenRejected
        | RelayError::SessionAuthFailed(_)
        | RelayError::StreamOfflineOrRotated => {
            exponential_backoff_ms(RELAY_AUTH_RETRY_BASE_MS, RELAY_AUTH_RETRY_MAX_MS, attempts)
        }
        RelayError::TimestampRejectedResynced => 500,
        RelayError::Transport(_) | RelayError::InvalidResponse(_) | RelayError::IngestFailed(_) => {
            exponential_backoff_ms(RELAY_RETRY_BASE_MS, RELAY_RETRY_MAX_MS, attempts)
        }
    }
}

fn classify_message_mapping(message: &OscMessage, mappings: &[Mapping]) -> (&'static str, bool) {
    let address_matched = mappings
        .iter()
        .any(|mapping| mapping.address == message.address);
    let numeric_arg = extract_numeric_arg(&message.args).is_some();
    if address_matched && numeric_arg {
        return ("mapped", true);
    }
    if address_matched {
        return ("address-match-non-numeric", false);
    }
    ("unmapped", false)
}

fn param_key_from_address(address: &str) -> String {
    const PREFIX: &str = "/avatar/parameters/";
    if let Some(stripped) = address.strip_prefix(PREFIX) {
        return stripped.to_string();
    }
    address.to_string()
}

fn first_arg_to_runtime_value(args: &[OscArg]) -> Option<RuntimeParamValue> {
    match args.first() {
        Some(OscArg::Int(value)) => Some(RuntimeParamValue::Number(*value as f64)),
        Some(OscArg::Float(value)) if value.is_finite() => {
            Some(RuntimeParamValue::Number(*value as f64))
        }
        Some(OscArg::Bool(value)) => Some(RuntimeParamValue::Bool(*value)),
        Some(OscArg::Str(value)) => Some(RuntimeParamValue::Text(value.clone())),
        _ => None,
    }
}

fn update_avatar_param_cache(
    avatar_params: &Arc<Mutex<HashMap<String, RuntimeParamValue>>>,
    message: &OscMessage,
) {
    let Some(value) = first_arg_to_runtime_value(&message.args) else {
        return;
    };
    let key = param_key_from_address(&message.address);
    let mut lock = avatar_params.lock().expect("avatar params mutex poisoned");
    if !lock.contains_key(&key) && lock.len() >= AVATAR_PARAM_HISTORY_LIMIT {
        if let Some(first_key) = lock.keys().next().cloned() {
            lock.remove(&first_key);
        }
    }
    lock.insert(key, value);
}

fn format_osc_arg_value(value: &OscArg) -> String {
    match value {
        OscArg::Int(v) => v.to_string(),
        OscArg::Float(v) if v.is_finite() => format!("{v:.6}"),
        OscArg::Float(_) => "NaN".to_string(),
        OscArg::Bool(v) => v.to_string(),
        OscArg::Str(v) => format!("{v:?}"),
    }
}

fn log_osc_debug(
    config: &NormalizedConfig,
    logs: &Arc<Mutex<VecDeque<RuntimeLogLine>>>,
    message: &OscMessage,
    mapping_state: &str,
) {
    if !config.debug.log_osc {
        return;
    }
    if config.debug.log_unmapped_only
        && (mapping_state == "mapped" || mapping_state == "address-match-non-numeric")
    {
        return;
    }
    if config.debug.log_configured_only && mapping_state == "unmapped" {
        return;
    }

    let type_tag = if message.arg_types.is_empty() {
        "-".to_string()
    } else {
        message.arg_types.clone()
    };
    let args_text = if message.args.is_empty() {
        "no-args".to_string()
    } else {
        message
            .args
            .iter()
            .map(format_osc_arg_value)
            .collect::<Vec<_>>()
            .join(", ")
    };

    push_log(
        logs,
        "OSC",
        format!(
            "{} {} [{}] -> {}",
            mapping_state, message.address, type_tag, args_text
        ),
    );
}

fn load_discovery_entries(path: &Path) -> Result<HashSet<String>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(err.to_string()),
    };
    let entries = raw
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| line.len() <= DISCOVERY_ENTRY_MAX_LEN)
        .take(DISCOVERY_ENTRY_MAX)
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    Ok(entries)
}

fn discovery_entry_for(message: &OscMessage, include_arg_types: bool) -> Option<String> {
    let address = message.address.trim();
    if address.is_empty() || !address.starts_with('/') {
        return None;
    }
    if !include_arg_types {
        return Some(address.to_string());
    }
    let type_tag = if message.arg_types.trim().is_empty() {
        "-".to_string()
    } else {
        message.arg_types.trim().to_string()
    };
    Some(format!("{address}\t[{type_tag}]"))
}

fn record_discovery_entry(
    config: &NormalizedConfig,
    logs: &Arc<Mutex<VecDeque<RuntimeLogLine>>>,
    seen_entries: &mut HashSet<String>,
    message: &OscMessage,
) -> bool {
    let Some(entry) = discovery_entry_for(message, config.discovery.include_arg_types) else {
        return false;
    };
    if !seen_entries.insert(entry.clone()) {
        return false;
    }
    if seen_entries.len() > DISCOVERY_ENTRY_MAX {
        let _ = seen_entries.remove(&entry);
        return false;
    }
    if entry.len() > DISCOVERY_ENTRY_MAX_LEN {
        let _ = seen_entries.remove(&entry);
        return false;
    }

    if let Some(parent) = config.discovery.file_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(err) = fs::create_dir_all(parent) {
                push_log(
                    logs,
                    "WARN",
                    format!(
                        "discovery mkdir failed for {}: {err}",
                        config.discovery.file_path.display()
                    ),
                );
                return false;
            }
        }
    }

    let mut writer = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.discovery.file_path)
    {
        Ok(writer) => writer,
        Err(err) => {
            push_log(
                logs,
                "WARN",
                format!(
                    "discovery open failed for {}: {err}",
                    config.discovery.file_path.display()
                ),
            );
            return false;
        }
    };
    if let Err(err) = writeln!(writer, "{entry}") {
        push_log(
            logs,
            "WARN",
            format!(
                "discovery write failed for {}: {err}",
                config.discovery.file_path.display()
            ),
        );
        return false;
    }

    push_log(&logs, "DISCOVERY", format!("+ {entry}"));
    true
}

fn is_sender_allowed(config: &NormalizedConfig, sender_ip: IpAddr) -> bool {
    if !config.allow_network_osc && !sender_ip.is_loopback() {
        return false;
    }
    if !config.osc_allowed_senders.is_empty() && !config.osc_allowed_senders.contains(&sender_ip) {
        return false;
    }
    true
}

fn push_log(logs: &Arc<Mutex<VecDeque<RuntimeLogLine>>>, level: &str, message: impl Into<String>) {
    let mut lock = logs.lock().expect("logs mutex poisoned");
    lock.push_back(RuntimeLogLine {
        ts_ms: unix_ms_now(),
        level: level.to_string(),
        message: message.into(),
    });
    while lock.len() > LOG_HISTORY_LIMIT {
        lock.pop_front();
    }
}

fn set_runtime_error(snapshot: &Arc<Mutex<RuntimeSnapshot>>, error_message: String) {
    let mut lock = snapshot.lock().expect("snapshot mutex poisoned");
    lock.last_error = error_message;
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::UdpSocket;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, SystemTime};

    use url::Url;

    use super::*;
    use crate::config::{
        DebugConfig, DiscoveryConfig, ForwardTarget, NormalizedConfig, OscListen, RelayPaths,
    };
    use crate::mapping::{Curve, Mapping};
    use crate::relay::{RelayError, RelayPublisher, RelaySessionState};
    use crate::smoothing::OutputTuning;

    #[derive(Default)]
    struct CollectingRelay {
        events: Arc<Mutex<Vec<RelayEvent>>>,
        queued_errors: Arc<Mutex<VecDeque<RelayError>>>,
    }

    impl CollectingRelay {
        fn new() -> Self {
            Self::default()
        }
    }

    impl RelayPublisher for CollectingRelay {
        fn push_event(&mut self, event: &RelayEvent, _now_ms: i64) -> Result<(), RelayError> {
            if let Some(err) = self.queued_errors.lock().expect("errors lock").pop_front() {
                return Err(err);
            }
            self.events.lock().expect("events lock").push(event.clone());
            Ok(())
        }

        fn next_client_ts_ms(&self, now_ms: i64) -> i64 {
            now_ms
        }

        fn session_state(&self) -> RelaySessionState {
            RelaySessionState::default()
        }
    }

    fn free_port() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let port = socket.local_addr().expect("addr").port();
        drop(socket);
        port
    }

    fn make_config(listen_port: u16, forward_port: Option<u16>) -> NormalizedConfig {
        let mut forward_targets = Vec::new();
        if let Some(port) = forward_port {
            forward_targets.push(ForwardTarget {
                host: "127.0.0.1".to_string(),
                port,
            });
        }
        NormalizedConfig {
            base_url: Url::parse("https://dev.vrlewds.com").expect("url"),
            creator_username: "Vee".to_string(),
            stream_key: "stream_key".to_string(),
            osc_listen: OscListen {
                host: "127.0.0.1".to_string(),
                port: listen_port,
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
            forward_targets,
            mappings: vec![Mapping::new(
                "/avatar/parameters/SPS_Contact",
                1.0,
                0.0,
                false,
                Curve::Linear,
                0.0,
                1.0,
            )
            .expect("mapping")],
            output: OutputTuning {
                emit_hz: 30.0,
                attack_ms: 10.0,
                release_ms: 20.0,
                ema_alpha: 1.0,
                min_delta: 0.01,
                heartbeat_ms: 200,
            },
        }
    }

    fn write_osc_string(buf: &mut Vec<u8>, value: &str) {
        buf.extend_from_slice(value.as_bytes());
        buf.push(0);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    fn osc_message_float(address: &str, value: f32) -> Vec<u8> {
        let mut msg = Vec::new();
        write_osc_string(&mut msg, address);
        write_osc_string(&mut msg, ",f");
        msg.extend_from_slice(&value.to_bits().to_be_bytes());
        msg
    }

    fn osc_message_bool(address: &str, value: bool) -> Vec<u8> {
        let mut msg = Vec::new();
        write_osc_string(&mut msg, address);
        write_osc_string(&mut msg, if value { ",T" } else { ",F" });
        msg
    }

    #[test]
    fn runtime_receives_osc_and_emits_relay_event() {
        let relay = CollectingRelay::new();
        let relay_events = relay.events.clone();
        let (handle, listen_port) = {
            let mut result: Option<(BridgeRuntimeHandle, u16)> = None;
            let mut relay_opt = Some(relay);
            for _ in 0..20 {
                let listen_port = free_port();
                let config = make_config(listen_port, None);
                let relay_instance = relay_opt.take().expect("relay present");
                match BridgeRuntime::start(config, Box::new(relay_instance)) {
                    Ok(handle) => {
                        result = Some((handle, listen_port));
                        break;
                    }
                    Err(err) => {
                        if !err.contains("os error 10048") {
                            panic!("start runtime: {err}");
                        }
                        relay_opt = Some(CollectingRelay {
                            events: Arc::clone(&relay_events),
                            queued_errors: Arc::new(Mutex::new(VecDeque::new())),
                        });
                    }
                }
            }
            result.expect("runtime start")
        };
        thread::sleep(Duration::from_millis(60));

        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
        let packet = osc_message_float("/avatar/parameters/SPS_Contact", 1.0);
        let _ = sender.send_to(&packet, format!("127.0.0.1:{listen_port}"));

        let timeout = SystemTime::now() + Duration::from_secs(2);
        loop {
            let has_contact_event = relay_events
                .lock()
                .expect("events lock")
                .iter()
                .any(|event| event.source.address == "/avatar/parameters/SPS_Contact");
            if has_contact_event {
                break;
            }
            if SystemTime::now() > timeout {
                panic!("timed out waiting for relay event");
            }
            thread::sleep(Duration::from_millis(20));
        }

        let snapshot = handle.snapshot();
        assert!(snapshot.last_tick_ms > 0);
        assert!(snapshot.osc_packets_received >= 1);
        assert!(snapshot.osc_messages_received >= 1);
        assert!(snapshot.mapped_messages_received >= 1);
        handle.stop();
    }

    #[test]
    fn runtime_caches_avatar_params() {
        let relay = CollectingRelay::new();
        let (handle, listen_port) = {
            let mut result: Option<(BridgeRuntimeHandle, u16)> = None;
            let mut relay_opt = Some(relay);
            for _ in 0..20 {
                let listen_port = free_port();
                let config = make_config(listen_port, None);
                let relay_instance = relay_opt.take().expect("relay present");
                match BridgeRuntime::start(config, Box::new(relay_instance)) {
                    Ok(handle) => {
                        result = Some((handle, listen_port));
                        break;
                    }
                    Err(err) => {
                        if !err.contains("os error 10048") {
                            panic!("start runtime: {err}");
                        }
                        relay_opt = Some(CollectingRelay::new());
                    }
                }
            }
            result.expect("runtime start")
        };
        thread::sleep(Duration::from_millis(60));

        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
        let packet = osc_message_bool("/avatar/parameters/TestToggle", true);
        let _ = sender.send_to(&packet, format!("127.0.0.1:{listen_port}"));

        let timeout = SystemTime::now() + Duration::from_secs(2);
        loop {
            let params = handle.avatar_params();
            if params.iter().any(|(key, _)| key == "TestToggle") {
                break;
            }
            if SystemTime::now() > timeout {
                panic!("timed out waiting for avatar param");
            }
            thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
    }
}
