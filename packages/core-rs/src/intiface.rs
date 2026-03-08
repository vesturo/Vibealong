use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use tungstenite::client::client;
use tungstenite::protocol::WebSocket;
use tungstenite::{connect, Message};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntifaceConfig {
    pub host: String,
    pub port: u16,
    pub secure: bool,
}

impl Default for IntifaceConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 12345,
            secure: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntifaceFeature {
    pub command_type: String,
    pub index: u32,
    pub actuator_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntifaceDevice {
    pub device_index: u32,
    pub name: String,
    pub features: Vec<IntifaceFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntifaceSnapshot {
    pub checked_at_ms: i64,
    pub connected: bool,
    pub server_name: String,
    pub devices: Vec<IntifaceDevice>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntifaceScalarCommand {
    pub device_index: u32,
    pub scalar_index: u32,
    pub actuator_type: String,
    pub level: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntifaceBridgeConfig {
    pub intiface: IntifaceConfig,
    pub emit_hz: f64,
    pub min_delta: f64,
    pub heartbeat_ms: u64,
    pub routes: Vec<IntifaceRouteRule>,
}

impl Default for IntifaceBridgeConfig {
    fn default() -> Self {
        Self {
            intiface: IntifaceConfig::default(),
            emit_hz: 8.0,
            min_delta: 0.02,
            heartbeat_ms: 1200,
            routes: vec![IntifaceRouteRule::default()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntifaceSourceKind {
    Intensity,
    AvatarParam(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntifaceRouteRule {
    pub enabled: bool,
    pub label: String,
    pub target_device_contains: String,
    pub target_actuator_type: String,
    pub source: IntifaceSourceKind,
    pub scale: f64,
    pub idle: f64,
    pub min_output: f64,
    pub max_output: f64,
    pub invert: bool,
}

impl Default for IntifaceRouteRule {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "All Scalar <- intensity".to_string(),
            target_device_contains: String::new(),
            target_actuator_type: String::new(),
            source: IntifaceSourceKind::Intensity,
            scale: 1.0,
            idle: 0.0,
            min_output: 0.0,
            max_output: 1.0,
            invert: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntifaceBridgeSnapshot {
    pub running: bool,
    pub connected: bool,
    pub server_name: String,
    pub device_count: usize,
    pub route_count: usize,
    pub commands_sent: u64,
    pub last_level: f64,
    pub last_sent_at_ms: i64,
    pub last_error: String,
    pub checked_at_ms: i64,
}

impl Default for IntifaceBridgeSnapshot {
    fn default() -> Self {
        Self {
            running: false,
            connected: false,
            server_name: String::new(),
            device_count: 0,
            route_count: 0,
            commands_sent: 0,
            last_level: 0.0,
            last_sent_at_ms: 0,
            last_error: String::new(),
            checked_at_ms: 0,
        }
    }
}

#[derive(Debug)]
pub struct IntifaceBridgeHandle {
    source_values: Arc<Mutex<std::collections::HashMap<String, f64>>>,
    routes: Arc<Mutex<Vec<IntifaceRouteRule>>>,
    snapshot: Arc<Mutex<IntifaceBridgeSnapshot>>,
    stop_tx: Sender<()>,
    join_handle: Option<JoinHandle<()>>,
}

pub struct IntifaceClient {
    pub config: IntifaceConfig,
    pub client_name: String,
}

impl IntifaceClient {
    pub fn new(config: IntifaceConfig) -> Self {
        Self {
            config,
            client_name: "Vibealong".to_string(),
        }
    }

    pub fn probe_snapshot(&self) -> IntifaceSnapshot {
        let now = unix_ms_now();
        match self.connect_and_query() {
            Ok((server_name, devices)) => IntifaceSnapshot {
                checked_at_ms: now,
                connected: true,
                server_name,
                devices,
                last_error: None,
            },
            Err(err) => IntifaceSnapshot {
                checked_at_ms: now,
                connected: false,
                server_name: String::new(),
                devices: Vec::new(),
                last_error: Some(err),
            },
        }
    }

    pub fn set_scalar_level(
        &self,
        device_index: u32,
        scalar_index: u32,
        actuator_type: &str,
        level: f64,
    ) -> Result<(), String> {
        let clamped_level = level.clamp(0.0, 1.0);
        self.with_socket(|socket| {
            send_buttplug_packet(
                socket,
                "RequestServerInfo",
                1,
                json!({
                    "ClientName": self.client_name,
                    "MessageVersion": 3
                }),
            )?;
            let _ = read_until_type(socket, "ServerInfo", 8)?;
            send_buttplug_packet(
                socket,
                "ScalarCmd",
                2,
                json!({
                    "DeviceIndex": device_index,
                    "Scalars": [{
                        "Index": scalar_index,
                        "Scalar": clamped_level,
                        "ActuatorType": actuator_type
                    }]
                }),
            )?;
            let _ = read_until_type(socket, "Ok", 8)?;
            Ok(())
        })
    }

    pub fn set_scalar_levels(&self, commands: &[IntifaceScalarCommand]) -> Result<(), String> {
        if commands.is_empty() {
            return Ok(());
        }

        self.with_socket(|socket| {
            send_buttplug_packet(
                socket,
                "RequestServerInfo",
                1,
                json!({
                    "ClientName": self.client_name,
                    "MessageVersion": 3
                }),
            )?;
            let _ = read_until_type(socket, "ServerInfo", 8)?;

            for (idx, command) in commands.iter().enumerate() {
                let level = command.level.clamp(0.0, 1.0);
                send_buttplug_packet(
                    socket,
                    "ScalarCmd",
                    2 + idx as u32,
                    json!({
                        "DeviceIndex": command.device_index,
                        "Scalars": [{
                            "Index": command.scalar_index,
                            "Scalar": level,
                            "ActuatorType": command.actuator_type
                        }]
                    }),
                )?;
                let _ = read_until_type(socket, "Ok", 8)?;
            }
            Ok(())
        })
    }

    fn connect_and_query(&self) -> Result<(String, Vec<IntifaceDevice>), String> {
        self.with_socket(|socket| {
            send_buttplug_packet(
                socket,
                "RequestServerInfo",
                1,
                json!({
                    "ClientName": self.client_name,
                    "MessageVersion": 3
                }),
            )?;
            let server_info = read_until_type(socket, "ServerInfo", 12)?;
            let server_name = server_info
                .get("ServerName")
                .and_then(Value::as_str)
                .unwrap_or("Intiface")
                .to_string();

            send_buttplug_packet(socket, "RequestDeviceList", 2, json!({}))?;
            let device_list = read_until_type(socket, "DeviceList", 16)?;
            let devices = parse_device_list(&device_list);

            Ok((server_name, devices))
        })
    }

    fn with_socket<T>(
        &self,
        func: impl FnOnce(&mut dyn SocketIo) -> Result<T, String>,
    ) -> Result<T, String> {
        let scheme = if self.config.secure { "wss" } else { "ws" };
        let url = Url::parse(&format!(
            "{scheme}://{}:{}/",
            self.config.host, self.config.port
        ))
        .map_err(|e| e.to_string())?;

        if self.config.secure {
            let (mut socket, _) = connect(url.as_str()).map_err(|e| e.to_string())?;
            return func(&mut socket);
        }

        let tcp = TcpStream::connect((self.config.host.as_str(), self.config.port))
            .map_err(|e| e.to_string())?;
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(2)));
        let (mut socket, _) = client(url.as_str(), tcp).map_err(|e| e.to_string())?;
        func(&mut socket)
    }
}

impl IntifaceBridgeHandle {
    pub fn start(config: IntifaceBridgeConfig) -> Result<Self, String> {
        let mut initial_sources = HashMap::new();
        initial_sources.insert("intensity".to_string(), 0.0);
        let source_values = Arc::new(Mutex::new(initial_sources));
        let routes = Arc::new(Mutex::new(config.routes.clone()));
        let snapshot = Arc::new(Mutex::new(IntifaceBridgeSnapshot {
            running: true,
            checked_at_ms: unix_ms_now(),
            ..IntifaceBridgeSnapshot::default()
        }));

        let (stop_tx, stop_rx) = channel();
        let source_values_clone = Arc::clone(&source_values);
        let routes_clone = Arc::clone(&routes);
        let snapshot_clone = Arc::clone(&snapshot);
        let join_handle = thread::Builder::new()
            .name("vrl-intiface-bridge".to_string())
            .spawn(move || {
                run_intiface_bridge_loop(
                    config,
                    source_values_clone,
                    routes_clone,
                    snapshot_clone,
                    stop_rx,
                )
            })
            .map_err(|e| e.to_string())?;

        Ok(Self {
            source_values,
            routes,
            snapshot,
            stop_tx,
            join_handle: Some(join_handle),
        })
    }

    pub fn set_source_values(&self, values: HashMap<String, f64>) {
        if let Ok(mut lock) = self.source_values.lock() {
            *lock = values
                .into_iter()
                .map(|(key, value)| (key, value.clamp(0.0, 1.0)))
                .collect();
        }
    }

    pub fn set_routes(&self, routes: Vec<IntifaceRouteRule>) {
        if let Ok(mut lock) = self.routes.lock() {
            *lock = routes;
        }
    }

    pub fn snapshot(&self) -> IntifaceBridgeSnapshot {
        self.snapshot
            .lock()
            .expect("intiface bridge snapshot mutex poisoned")
            .clone()
    }

    pub fn stop(mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for IntifaceBridgeHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

fn run_intiface_bridge_loop(
    config: IntifaceBridgeConfig,
    source_values: Arc<Mutex<HashMap<String, f64>>>,
    routes: Arc<Mutex<Vec<IntifaceRouteRule>>>,
    snapshot: Arc<Mutex<IntifaceBridgeSnapshot>>,
    stop_rx: Receiver<()>,
) {
    let client = IntifaceClient::new(config.intiface.clone());
    let emit_hz = config.emit_hz.clamp(1.0, 30.0);
    let tick_duration = Duration::from_millis((1000.0 / emit_hz) as u64);
    let mut last_probe_at_ms = 0_i64;
    let mut last_sent_at_ms = 0_i64;
    let mut last_sent_level = 0.0_f64;
    let mut last_devices = Vec::<IntifaceDevice>::new();

    loop {
        match stop_rx.try_recv() {
            Ok(_) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }

        let now_ms = unix_ms_now();
        if now_ms - last_probe_at_ms >= 3000 {
            last_probe_at_ms = now_ms;
            let probe = client.probe_snapshot();
            {
                let mut lock = snapshot
                    .lock()
                    .expect("intiface bridge snapshot mutex poisoned");
                lock.checked_at_ms = now_ms;
                lock.connected = probe.connected;
                lock.server_name = probe.server_name.clone();
                lock.device_count = probe.devices.len();
                lock.route_count = routes.lock().map(|r| r.len()).unwrap_or(0);
                lock.last_error = probe.last_error.clone().unwrap_or_default();
            }
            if probe.connected {
                last_devices = probe.devices;
            }
        }

        let current_sources = source_values
            .lock()
            .map(|values| values.clone())
            .unwrap_or_default();
        let current_routes = routes.lock().map(|items| items.clone()).unwrap_or_default();
        let target = current_sources.get("intensity").copied().unwrap_or(0.0);
        if should_emit_level(
            last_sent_level,
            target,
            now_ms,
            last_sent_at_ms,
            config.min_delta,
            config.heartbeat_ms,
        ) && !last_devices.is_empty()
        {
            let mut commands = Vec::new();
            for device in &last_devices {
                for feature in &device.features {
                    if feature.command_type == "ScalarCmd" {
                        if let Some(level) =
                            route_scalar_level(device, feature, &current_sources, &current_routes)
                        {
                            commands.push(IntifaceScalarCommand {
                                device_index: device.device_index,
                                scalar_index: feature.index,
                                actuator_type: feature
                                    .actuator_type
                                    .clone()
                                    .unwrap_or_else(|| "Vibrate".to_string()),
                                level,
                            });
                        }
                    }
                }
            }

            if !commands.is_empty() {
                match client.set_scalar_levels(&commands) {
                    Ok(_) => {
                        last_sent_level = target;
                        last_sent_at_ms = now_ms;
                        let mut lock = snapshot
                            .lock()
                            .expect("intiface bridge snapshot mutex poisoned");
                        lock.commands_sent += commands.len() as u64;
                        lock.last_level = target;
                        lock.last_sent_at_ms = now_ms;
                        lock.last_error.clear();
                    }
                    Err(err) => {
                        let mut lock = snapshot
                            .lock()
                            .expect("intiface bridge snapshot mutex poisoned");
                        lock.last_error = err;
                        lock.connected = false;
                    }
                }
            }
        }

        thread::sleep(tick_duration);
    }

    let mut lock = snapshot
        .lock()
        .expect("intiface bridge snapshot mutex poisoned");
    lock.running = false;
}

fn should_emit_level(
    last_level: f64,
    current_level: f64,
    now_ms: i64,
    last_sent_at_ms: i64,
    min_delta: f64,
    heartbeat_ms: u64,
) -> bool {
    if (current_level - last_level).abs() >= min_delta.max(0.0) {
        return true;
    }
    if last_sent_at_ms == 0 {
        return true;
    }
    now_ms - last_sent_at_ms >= heartbeat_ms as i64
}

fn route_scalar_level(
    device: &IntifaceDevice,
    feature: &IntifaceFeature,
    source_values: &HashMap<String, f64>,
    routes: &[IntifaceRouteRule],
) -> Option<f64> {
    let mut out: Option<f64> = None;
    for route in routes {
        if !route.enabled {
            continue;
        }
        if !rule_matches_feature(route, device, feature) {
            continue;
        }
        let Some(source_value) = resolve_route_source(route, source_values) else {
            continue;
        };
        let mapped = map_route_level(route, source_value);
        out = Some(match out {
            Some(existing) => existing.max(mapped),
            None => mapped,
        });
    }
    out
}

fn rule_matches_feature(
    route: &IntifaceRouteRule,
    device: &IntifaceDevice,
    feature: &IntifaceFeature,
) -> bool {
    if !route.target_device_contains.trim().is_empty() {
        let device_name = device.name.to_ascii_lowercase();
        let expected = route.target_device_contains.trim().to_ascii_lowercase();
        if !device_name.contains(&expected) {
            return false;
        }
    }
    if !route.target_actuator_type.trim().is_empty() {
        let expected = route.target_actuator_type.trim().to_ascii_lowercase();
        let actual = feature
            .actuator_type
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        if actual != expected {
            return false;
        }
    }
    true
}

fn resolve_route_source(
    route: &IntifaceRouteRule,
    source_values: &HashMap<String, f64>,
) -> Option<f64> {
    match &route.source {
        IntifaceSourceKind::Intensity => source_values.get("intensity").copied(),
        IntifaceSourceKind::AvatarParam(param) => {
            let key = format!("avatar:{}", param.trim());
            source_values.get(&key).copied()
        }
    }
}

fn map_route_level(route: &IntifaceRouteRule, source_value: f64) -> f64 {
    let mut level = source_value.clamp(0.0, 1.0);
    if route.invert {
        level = 1.0 - level;
    }
    level *= route.scale.max(0.0);
    let idle = route.idle.clamp(0.0, 1.0);
    if idle > 0.0 {
        level = level * (1.0 - idle) + idle;
    }
    let min_output = route.min_output.clamp(0.0, 1.0);
    let max_output = route.max_output.clamp(0.0, 1.0).max(min_output);
    level.clamp(min_output, max_output)
}

trait SocketIo {
    fn write_message(&mut self, message: Message) -> Result<(), String>;
    fn read_message(&mut self) -> Result<Message, String>;
}

impl<S: Read + Write> SocketIo for WebSocket<S> {
    fn write_message(&mut self, message: Message) -> Result<(), String> {
        self.send(message).map_err(|e| e.to_string())
    }

    fn read_message(&mut self) -> Result<Message, String> {
        self.read().map_err(|e| e.to_string())
    }
}

fn send_buttplug_packet(
    socket: &mut dyn SocketIo,
    message_type: &str,
    id: u32,
    body: Value,
) -> Result<(), String> {
    let mut payload = body;
    if let Value::Object(map) = &mut payload {
        map.insert("Id".to_string(), Value::from(id));
    } else {
        return Err("Buttplug packet body must be an object".to_string());
    }
    let packet = json!([{ message_type: payload }]);
    socket.write_message(Message::Text(packet.to_string()))
}

fn read_until_type(
    socket: &mut dyn SocketIo,
    expected_type: &str,
    max_reads: usize,
) -> Result<Value, String> {
    for _ in 0..max_reads {
        let message = socket.read_message()?;
        match message {
            Message::Text(text) => {
                for (msg_type, body) in parse_buttplug_packet(&text)? {
                    if msg_type == expected_type {
                        return Ok(body);
                    }
                }
            }
            Message::Binary(binary) => {
                if let Ok(text) = String::from_utf8(binary.to_vec()) {
                    for (msg_type, body) in parse_buttplug_packet(&text)? {
                        if msg_type == expected_type {
                            return Ok(body);
                        }
                    }
                }
            }
            Message::Ping(payload) => {
                let _ = socket.write_message(Message::Pong(payload));
            }
            Message::Close(_) => {
                return Err("Intiface closed the websocket".to_string());
            }
            _ => {}
        }
    }
    Err(format!(
        "Timed out waiting for Buttplug message type {expected_type}"
    ))
}

fn parse_buttplug_packet(text: &str) -> Result<Vec<(String, Value)>, String> {
    let json: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let array = json
        .as_array()
        .ok_or_else(|| "Buttplug packet must be a JSON array".to_string())?;
    let mut out = Vec::new();
    for entry in array {
        let object = entry
            .as_object()
            .ok_or_else(|| "Buttplug packet entries must be JSON objects".to_string())?;
        for (message_type, body) in object {
            out.push((message_type.clone(), body.clone()));
        }
    }
    Ok(out)
}

fn parse_device_list(body: &Value) -> Vec<IntifaceDevice> {
    let mut devices = Vec::new();
    let list = body
        .get("Devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for item in list {
        let device_index = item
            .get("DeviceIndex")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let name = item
            .get("DeviceName")
            .and_then(Value::as_str)
            .unwrap_or("Unknown Device")
            .to_string();

        let mut features = Vec::new();
        if let Some(device_messages) = item.get("DeviceMessages").and_then(Value::as_object) {
            if let Some(scalars) = device_messages.get("ScalarCmd").and_then(Value::as_array) {
                for (idx, scalar) in scalars.iter().enumerate() {
                    let actuator_type = scalar
                        .get("ActuatorType")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    features.push(IntifaceFeature {
                        command_type: "ScalarCmd".to_string(),
                        index: idx as u32,
                        actuator_type,
                    });
                }
            }
            if let Some(linears) = device_messages.get("LinearCmd").and_then(Value::as_array) {
                for (idx, _) in linears.iter().enumerate() {
                    features.push(IntifaceFeature {
                        command_type: "LinearCmd".to_string(),
                        index: idx as u32,
                        actuator_type: None,
                    });
                }
            }
            if let Some(rotates) = device_messages.get("RotateCmd").and_then(Value::as_array) {
                for (idx, _) in rotates.iter().enumerate() {
                    features.push(IntifaceFeature {
                        command_type: "RotateCmd".to_string(),
                        index: idx as u32,
                        actuator_type: None,
                    });
                }
            }
        }

        devices.push(IntifaceDevice {
            device_index,
            name,
            features,
        });
    }
    devices
}

fn unix_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_device_list_extracts_features() {
        let body = json!({
            "Devices": [{
                "DeviceIndex": 2,
                "DeviceName": "Test Toy",
                "DeviceMessages": {
                    "ScalarCmd": [{ "ActuatorType": "Vibrate" }],
                    "LinearCmd": [{}, {}],
                    "RotateCmd": [{}]
                }
            }]
        });
        let devices = parse_device_list(&body);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_index, 2);
        assert_eq!(devices[0].name, "Test Toy");
        assert_eq!(devices[0].features.len(), 4);
    }

    #[test]
    fn parse_buttplug_packet_decodes_array_payload() {
        let payload = r#"[{"ServerInfo":{"Id":1,"ServerName":"Intiface"}}]"#;
        let messages = parse_buttplug_packet(payload).expect("parse packet");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "ServerInfo");
        assert_eq!(
            messages[0]
                .1
                .get("ServerName")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "Intiface"
        );
    }

    #[test]
    fn emit_level_respects_delta_and_heartbeat() {
        assert!(should_emit_level(0.0, 0.5, 1000, 0, 0.02, 1200));
        assert!(should_emit_level(0.10, 0.13, 2000, 1500, 0.02, 1200));
        assert!(!should_emit_level(0.10, 0.11, 2000, 1500, 0.02, 1200));
        assert!(should_emit_level(0.10, 0.11, 3000, 1500, 0.02, 1200));
    }

    #[test]
    fn route_scalar_level_uses_matching_route() {
        let device = IntifaceDevice {
            device_index: 1,
            name: "Test Device".to_string(),
            features: Vec::new(),
        };
        let feature = IntifaceFeature {
            command_type: "ScalarCmd".to_string(),
            index: 0,
            actuator_type: Some("Vibrate".to_string()),
        };
        let routes = vec![IntifaceRouteRule {
            enabled: true,
            label: "Rule".to_string(),
            target_device_contains: "test".to_string(),
            target_actuator_type: "vibrate".to_string(),
            source: IntifaceSourceKind::AvatarParam("SPS_Contact".to_string()),
            scale: 1.0,
            idle: 0.0,
            min_output: 0.0,
            max_output: 1.0,
            invert: false,
        }];
        let mut sources = HashMap::new();
        sources.insert("avatar:SPS_Contact".to_string(), 0.7);
        let level = route_scalar_level(&device, &feature, &sources, &routes).expect("level");
        assert!((level - 0.7).abs() < 1e-6);
    }

    #[test]
    fn map_route_applies_idle_and_invert() {
        let route = IntifaceRouteRule {
            enabled: true,
            label: "Rule".to_string(),
            target_device_contains: String::new(),
            target_actuator_type: String::new(),
            source: IntifaceSourceKind::Intensity,
            scale: 1.0,
            idle: 0.2,
            min_output: 0.0,
            max_output: 1.0,
            invert: true,
        };
        let out = map_route_level(&route, 0.75);
        assert!(out > 0.35 && out < 0.45);
    }
}
