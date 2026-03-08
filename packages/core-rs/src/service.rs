use std::fmt::{Display, Formatter};
use std::sync::Mutex;

use crate::app_store::{AppProfile, LoadedProfile, ProfileStore, ProfileSummary, StoreError};
use crate::config::{normalize_in_app_config, ConfigError, NormalizedConfig};
use crate::relay::{RelayClient, RelayClientConfig, RelayPublisher, ReqwestTransport};
use crate::runtime::{
    BridgeRuntime, BridgeRuntimeHandle, RuntimeLogLine, RuntimeParamValue, RuntimeSnapshot,
};

pub trait RelayPublisherFactory: Send + Sync {
    fn build(
        &self,
        normalized_config: &NormalizedConfig,
    ) -> Result<Box<dyn RelayPublisher>, String>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultRelayPublisherFactory;

impl RelayPublisherFactory for DefaultRelayPublisherFactory {
    fn build(
        &self,
        normalized_config: &NormalizedConfig,
    ) -> Result<Box<dyn RelayPublisher>, String> {
        let transport = ReqwestTransport::new().map_err(|e| e.to_string())?;
        let client = RelayClient::new(
            RelayClientConfig {
                base_url: normalized_config.base_url.clone(),
                creator_username: normalized_config.creator_username.clone(),
                stream_key: normalized_config.stream_key.clone(),
                session_path: normalized_config.relay.session_path.clone(),
                ingest_path: normalized_config.relay.ingest_path.clone(),
            },
            transport,
        );
        Ok(Box::new(client))
    }
}

struct RuntimeSlot {
    profile_id: String,
    handle: BridgeRuntimeHandle,
}

#[derive(Debug)]
pub enum ServiceError {
    Store(StoreError),
    Config(ConfigError),
    MissingProfile(String),
    MissingStreamKey(String),
    Runtime(String),
    RelayFactory(String),
    MutexPoisoned(&'static str),
}

impl Display for ServiceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::Config(err) => write!(f, "{err}"),
            Self::MissingProfile(profile_id) => {
                write!(f, "Profile not found: {profile_id}")
            }
            Self::MissingStreamKey(profile_id) => {
                write!(f, "Profile is missing stream key: {profile_id}")
            }
            Self::Runtime(message) => write!(f, "Runtime error: {message}"),
            Self::RelayFactory(message) => write!(f, "Relay factory error: {message}"),
            Self::MutexPoisoned(name) => write!(f, "Mutex poisoned: {name}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<StoreError> for ServiceError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<ConfigError> for ServiceError {
    fn from(value: ConfigError) -> Self {
        Self::Config(value)
    }
}

pub struct AppBridgeService<T: ProfileStore, F: RelayPublisherFactory> {
    store: Mutex<T>,
    relay_factory: F,
    runtime: Mutex<Option<RuntimeSlot>>,
}

impl<T: ProfileStore, F: RelayPublisherFactory> AppBridgeService<T, F> {
    pub fn new(store: T, relay_factory: F) -> Self {
        Self {
            store: Mutex::new(store),
            relay_factory,
            runtime: Mutex::new(None),
        }
    }

    pub fn upsert_profile(
        &self,
        profile: &AppProfile,
        stream_key: &str,
    ) -> Result<String, ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .upsert_profile(profile, stream_key)
            .map_err(ServiceError::Store)
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileSummary>, ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store.list_profiles().map_err(ServiceError::Store)
    }

    pub fn load_profile(&self, profile_id: &str) -> Result<Option<LoadedProfile>, ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store.load_profile(profile_id).map_err(ServiceError::Store)
    }

    pub fn last_selected_profile_id(&self) -> Result<Option<String>, ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .last_selected_profile_id()
            .map_err(ServiceError::Store)
    }

    pub fn set_last_selected_profile_id(
        &self,
        profile_id: Option<&str>,
    ) -> Result<(), ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .set_last_selected_profile_id(profile_id)
            .map_err(ServiceError::Store)
    }

    pub fn close_to_background_preference(&self) -> Result<Option<bool>, ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .close_to_background_preference()
            .map_err(ServiceError::Store)
    }

    pub fn set_close_to_background_preference(&self, enabled: bool) -> Result<(), ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .set_close_to_background_preference(enabled)
            .map_err(ServiceError::Store)
    }

    pub fn set_stream_key(&self, profile_id: &str, stream_key: &str) -> Result<(), ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .set_stream_key(profile_id, stream_key)
            .map_err(ServiceError::Store)
    }

    pub fn clear_stream_key(&self, profile_id: &str) -> Result<(), ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .clear_stream_key(profile_id)
            .map_err(ServiceError::Store)
    }

    pub fn delete_profile(&self, profile_id: &str) -> Result<(), ServiceError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("store"))?;
        store
            .delete_profile(profile_id)
            .map_err(ServiceError::Store)?;

        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        if runtime
            .as_ref()
            .is_some_and(|slot| slot.profile_id == profile_id)
        {
            if let Some(slot) = runtime.take() {
                slot.handle.stop();
            }
        }
        Ok(())
    }

    pub fn start_profile(&self, profile_id: &str) -> Result<(), ServiceError> {
        let loaded = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| ServiceError::MutexPoisoned("store"))?;
            store
                .load_profile(profile_id)
                .map_err(ServiceError::Store)?
                .ok_or_else(|| ServiceError::MissingProfile(profile_id.to_string()))?
        };

        let stream_key = loaded
            .stream_key
            .ok_or_else(|| ServiceError::MissingStreamKey(profile_id.to_string()))?;
        let normalized = normalize_in_app_config(&loaded.profile.config, &stream_key)?;
        let relay = self
            .relay_factory
            .build(&normalized)
            .map_err(ServiceError::RelayFactory)?;
        let runtime_handle =
            BridgeRuntime::start(normalized, relay).map_err(ServiceError::Runtime)?;

        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        if let Some(slot) = runtime.take() {
            slot.handle.stop();
        }
        *runtime = Some(RuntimeSlot {
            profile_id: profile_id.to_string(),
            handle: runtime_handle,
        });
        Ok(())
    }

    pub fn stop_runtime(&self) -> Result<(), ServiceError> {
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        if let Some(slot) = runtime.take() {
            slot.handle.stop();
        }
        Ok(())
    }

    pub fn runtime_snapshot(&self) -> Result<Option<RuntimeSnapshot>, ServiceError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        Ok(runtime.as_ref().map(|slot| slot.handle.snapshot()))
    }

    pub fn runtime_logs(&self, max_lines: usize) -> Result<Vec<RuntimeLogLine>, ServiceError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        Ok(runtime
            .as_ref()
            .map(|slot| slot.handle.recent_logs(max_lines))
            .unwrap_or_default())
    }

    pub fn runtime_avatar_params(&self) -> Result<Vec<(String, RuntimeParamValue)>, ServiceError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        Ok(runtime
            .as_ref()
            .map(|slot| slot.handle.avatar_params())
            .unwrap_or_default())
    }

    pub fn active_profile_id(&self) -> Result<Option<String>, ServiceError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ServiceError::MutexPoisoned("runtime"))?;
        Ok(runtime.as_ref().map(|slot| slot.profile_id.clone()))
    }
}

impl<T: ProfileStore, F: RelayPublisherFactory> Drop for AppBridgeService<T, F> {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.runtime.lock() {
            if let Some(slot) = runtime.take() {
                slot.handle.stop();
            }
        }
    }
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
    use crate::app_store::{AppConfigStore, InMemorySecretStore};
    use crate::config::{
        DebugConfig, DiscoveryConfig, ForwardTarget, InAppConfig, OscListen, RelayPaths,
    };
    use crate::mapping::{Curve, Mapping};
    use crate::relay::{RelayError, RelayEvent, RelaySessionState};
    use crate::smoothing::OutputTuning;

    #[derive(Default)]
    struct TestRelay {
        events: Arc<Mutex<Vec<RelayEvent>>>,
        queued_errors: Arc<Mutex<VecDeque<RelayError>>>,
    }

    impl RelayPublisher for TestRelay {
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

    struct TestRelayFactory {
        events: Arc<Mutex<Vec<RelayEvent>>>,
    }

    impl RelayPublisherFactory for TestRelayFactory {
        fn build(
            &self,
            _normalized_config: &NormalizedConfig,
        ) -> Result<Box<dyn RelayPublisher>, String> {
            Ok(Box::new(TestRelay {
                events: Arc::clone(&self.events),
                queued_errors: Arc::new(Mutex::new(VecDeque::new())),
            }))
        }
    }

    fn free_port() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let port = socket.local_addr().expect("addr").port();
        drop(socket);
        port
    }

    fn sample_profile(port: u16) -> AppProfile {
        AppProfile {
            id: String::new(),
            name: "MVP Profile".to_string(),
            config: InAppConfig {
                website_base_url: "https://dev.vrlewds.com".to_string(),
                creator_username: "Vee".to_string(),
                allow_insecure_http: false,
                osc_listen: OscListen {
                    host: "127.0.0.1".to_string(),
                    port,
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
                    heartbeat_ms: 300,
                },
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

    #[test]
    fn service_profile_lifecycle_and_runtime_start_stop() {
        let store = AppConfigStore::in_memory(InMemorySecretStore::default()).expect("store");
        let events = Arc::new(Mutex::new(Vec::new()));
        let service = AppBridgeService::new(
            store,
            TestRelayFactory {
                events: Arc::clone(&events),
            },
        );

        let listen_port = free_port();
        let profile = sample_profile(listen_port);
        let profile_id = service
            .upsert_profile(&profile, "stream_key_abc")
            .expect("save profile");

        let profiles = service.list_profiles().expect("list");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, profile_id);
        assert!(profiles[0].has_stream_key);

        service.start_profile(&profile_id).expect("start runtime");
        assert_eq!(
            service.active_profile_id().expect("active profile"),
            Some(profile_id.clone())
        );

        thread::sleep(Duration::from_millis(60));
        let sender = UdpSocket::bind("127.0.0.1:0").expect("sender");
        let packet = osc_message_float("/avatar/parameters/SPS_Contact", 0.8);
        let _ = sender.send_to(&packet, format!("127.0.0.1:{listen_port}"));

        let timeout = SystemTime::now() + Duration::from_secs(2);
        loop {
            let hit = events
                .lock()
                .expect("events lock")
                .iter()
                .any(|event| event.source.address == "/avatar/parameters/SPS_Contact");
            if hit {
                break;
            }
            if SystemTime::now() > timeout {
                panic!("timed out waiting for service relay event");
            }
            thread::sleep(Duration::from_millis(20));
        }

        let snapshot = service
            .runtime_snapshot()
            .expect("runtime snapshot")
            .expect("snapshot");
        assert!(snapshot.running);
        assert!(snapshot.last_tick_ms > 0);
        service.stop_runtime().expect("stop runtime");
        assert_eq!(service.runtime_snapshot().expect("snapshot"), None);
    }

    #[test]
    fn service_returns_missing_profile_if_unknown() {
        let store = AppConfigStore::in_memory(InMemorySecretStore::default()).expect("store");
        let events = Arc::new(Mutex::new(Vec::new()));
        let service = AppBridgeService::new(
            store,
            TestRelayFactory {
                events: Arc::clone(&events),
            },
        );
        let error = service
            .start_profile("missing-profile")
            .expect_err("must fail");
        assert!(matches!(error, ServiceError::MissingProfile(_)));
    }

    #[test]
    fn default_factory_builds_reqwest_client() {
        let normalized = NormalizedConfig {
            base_url: Url::parse("https://dev.vrlewds.com").expect("url"),
            creator_username: "Vee".to_string(),
            stream_key: "stream_key".to_string(),
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
            forward_targets: Vec::new(),
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
            output: OutputTuning::default(),
        };
        let factory = DefaultRelayPublisherFactory;
        let relay = factory.build(&normalized).expect("build");
        let _state = relay.session_state();
    }
}
