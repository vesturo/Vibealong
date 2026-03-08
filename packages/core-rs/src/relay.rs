use std::collections::{HashMap, VecDeque};
use std::fmt::{Display, Formatter};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use url::Url;

fn is_companion_token_credential(value: &str) -> bool {
    value.trim().starts_with("vca1.")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySource {
    pub address: String,
    pub arg_type: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelayEvent {
    pub seq: u64,
    pub ts: i64,
    pub intensity: f64,
    pub peak: f64,
    pub raw: f64,
    pub source: RelaySource,
}

impl RelayEvent {
    pub fn as_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "ts": self.ts,
            "intensity": self.intensity,
            "peak": self.peak,
            "raw": self.raw,
            "source": {
                "address": self.source.address,
                "argType": self.source.arg_type
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayClientConfig {
    pub base_url: Url,
    pub creator_username: String,
    pub stream_key: String,
    pub session_path: String,
    pub ingest_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySessionState {
    pub stream_id: String,
    pub relay_token: String,
    pub relay_token_expires_at_ms: i64,
    pub clock_offset_ms: i64,
    pub max_skew_ms: i64,
}

impl Default for RelaySessionState {
    fn default() -> Self {
        Self {
            stream_id: String::new(),
            relay_token: String::new(),
            relay_token_expires_at_ms: 0,
            clock_offset_ms: 0,
            max_skew_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
}

pub trait HttpTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpResponse, RelayError>;
}

pub trait RelayPublisher: Send {
    fn push_event(&mut self, event: &RelayEvent, now_ms: i64) -> Result<(), RelayError>;
    fn next_client_ts_ms(&self, now_ms: i64) -> i64;
    fn session_state(&self) -> RelaySessionState;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    Transport(String),
    InvalidResponse(String),
    SessionAuthFailed(String),
    RelayTokenRejected,
    StreamOfflineOrRotated,
    Throttled,
    TimestampRejectedResynced,
    IngestFailed(String),
}

impl Display for RelayError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "Transport error: {message}"),
            Self::InvalidResponse(message) => write!(f, "Invalid response: {message}"),
            Self::SessionAuthFailed(message) => write!(f, "Session auth failed: {message}"),
            Self::RelayTokenRejected => f.write_str("Relay token rejected"),
            Self::StreamOfflineOrRotated => f.write_str("Creator stream is offline or rotated"),
            Self::Throttled => f.write_str("Relay event throttled"),
            Self::TimestampRejectedResynced => f.write_str(
                "Relay ingest rejected timestamp; re-synced clock offset and will retry",
            ),
            Self::IngestFailed(message) => write!(f, "Relay ingest failed: {message}"),
        }
    }
}

impl std::error::Error for RelayError {}

pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, RelayError> {
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| RelayError::Transport(e.to_string()))?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpResponse, RelayError> {
        let mut request = self.client.post(url).json(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .map_err(|e| RelayError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let mut out_headers = HashMap::new();
        for (name, value) in response.headers() {
            out_headers.insert(
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("").to_string(),
            );
        }
        let body = response
            .text()
            .map_err(|e| RelayError::Transport(e.to_string()))?;
        Ok(HttpResponse {
            status,
            headers: out_headers,
            body,
        })
    }
}

pub struct RelayClient<T: HttpTransport> {
    transport: T,
    config: RelayClientConfig,
    state: RelaySessionState,
}

impl<T: HttpTransport> RelayClient<T> {
    pub fn new(config: RelayClientConfig, transport: T) -> Self {
        Self {
            transport,
            config,
            state: RelaySessionState::default(),
        }
    }

    pub fn state(&self) -> &RelaySessionState {
        &self.state
    }

    pub fn next_client_ts_ms(&self, now_ms: i64) -> i64 {
        now_ms + self.state.clock_offset_ms
    }

    pub fn ensure_session(&mut self, now_ms: i64) -> Result<(), RelayError> {
        if !self.state.relay_token.is_empty()
            && self.state.relay_token_expires_at_ms - now_ms > 20_000
        {
            return Ok(());
        }
        self.create_session(now_ms)
    }

    pub fn create_session(&mut self, now_ms: i64) -> Result<(), RelayError> {
        let url = build_url(&self.config.base_url, &self.config.session_path)?;
        let credential = self.config.stream_key.trim();
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        let body = if is_companion_token_credential(credential) {
            headers.push((
                "authorization".to_string(),
                format!("Bearer {}", credential),
            ));
            json!({
                "creatorUsername": self.config.creator_username,
            })
        } else {
            json!({
                "creatorUsername": self.config.creator_username,
                "streamKey": credential,
            })
        };
        let response = self.transport.post_json(&url, &headers, &body)?;

        let payload = parse_json_body(&response.body).unwrap_or(Value::Null);
        if !(200..300).contains(&response.status) {
            let error_message = payload
                .get("error")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("HTTP {}", response.status));
            return Err(RelayError::SessionAuthFailed(error_message));
        }

        let token = payload
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let expires_at_iso = payload
            .get("expiresAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stream_id = payload
            .get("streamId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if token.is_empty() || expires_at_iso.is_empty() || stream_id.is_empty() {
            return Err(RelayError::InvalidResponse(
                "Session auth returned invalid payload".to_string(),
            ));
        }

        let expires_at_ms =
            parse_iso_or_error(&expires_at_iso, "Session auth returned invalid expiresAt")?;
        let server_now_ms = payload.get("serverNowMs").and_then(Value::as_i64);
        let response_date_ms = parse_response_date_ms(&response.headers);
        let effective_server_now_ms = server_now_ms.or(response_date_ms);
        let max_skew_ms = payload.get("maxSkewMs").and_then(Value::as_i64);
        let renewed_companion_token = payload
            .get("companionToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);

        self.state.relay_token = token;
        self.state.relay_token_expires_at_ms = expires_at_ms;
        self.state.stream_id = stream_id;
        if let Some(server_now_ms) = effective_server_now_ms {
            self.state.clock_offset_ms = server_now_ms - now_ms;
        }
        if let Some(max_skew_ms) = max_skew_ms {
            if max_skew_ms > 0 {
                self.state.max_skew_ms = max_skew_ms;
            }
        }
        if is_companion_token_credential(credential) {
            if let Some(renewed_companion_token) = renewed_companion_token {
                self.config.stream_key = renewed_companion_token;
            }
        }
        Ok(())
    }

    pub fn push_event(&mut self, event: &RelayEvent, now_ms: i64) -> Result<(), RelayError> {
        self.ensure_session(now_ms)?;

        let url = build_url(&self.config.base_url, &self.config.ingest_path)?;
        let response = self.transport.post_json(
            &url,
            &[
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "authorization".to_string(),
                    format!("Bearer {}", self.state.relay_token),
                ),
            ],
            &event.as_json(),
        )?;

        match response.status {
            401 | 403 => {
                self.state.relay_token.clear();
                self.state.relay_token_expires_at_ms = 0;
                return Err(RelayError::RelayTokenRejected);
            }
            409 => {
                self.state.relay_token.clear();
                self.state.relay_token_expires_at_ms = 0;
                return Err(RelayError::StreamOfflineOrRotated);
            }
            429 => return Err(RelayError::Throttled),
            _ => {}
        }

        if !(200..300).contains(&response.status) {
            if response.status == 400 && response.body.contains("Stale or invalid event timestamp")
            {
                if let Some(response_date_ms) = parse_response_date_ms(&response.headers) {
                    self.state.clock_offset_ms = response_date_ms - now_ms;
                }
                return Err(RelayError::TimestampRejectedResynced);
            }

            let suffix = if response.body.trim().is_empty() {
                format!("HTTP {}", response.status)
            } else {
                format!("HTTP {} {}", response.status, response.body.trim())
            };
            return Err(RelayError::IngestFailed(suffix));
        }
        Ok(())
    }
}

impl<T: HttpTransport + Send> RelayPublisher for RelayClient<T> {
    fn push_event(&mut self, event: &RelayEvent, now_ms: i64) -> Result<(), RelayError> {
        Self::push_event(self, event, now_ms)
    }

    fn next_client_ts_ms(&self, now_ms: i64) -> i64 {
        Self::next_client_ts_ms(self, now_ms)
    }

    fn session_state(&self) -> RelaySessionState {
        self.state.clone()
    }
}

fn build_url(base_url: &Url, path: &str) -> Result<String, RelayError> {
    base_url
        .join(path)
        .map(|url| url.to_string())
        .map_err(|e| RelayError::InvalidResponse(format!("Failed to build URL: {e}")))
}

fn parse_json_body(body: &str) -> Option<Value> {
    if body.trim().is_empty() {
        return None;
    }
    serde_json::from_str(body).ok()
}

fn parse_iso_or_error(value: &str, error_message: &str) -> Result<i64, RelayError> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| RelayError::InvalidResponse(error_message.to_string()))?;
    Ok(parsed.timestamp_millis())
}

fn parse_response_date_ms(headers: &HashMap<String, String>) -> Option<i64> {
    let date_header = headers.get("date")?;
    let parsed = httpdate::parse_http_date(date_header).ok()?;
    Some(system_time_to_unix_ms(parsed))
}

fn system_time_to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, Default)]
pub struct MockTransport {
    pub requests: Mutex<Vec<(String, Vec<(String, String)>, Value)>>,
    pub responses: Mutex<VecDeque<Result<HttpResponse, RelayError>>>,
}

impl MockTransport {
    pub fn with_responses(responses: Vec<Result<HttpResponse, RelayError>>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        }
    }
}

impl HttpTransport for MockTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpResponse, RelayError> {
        self.requests.lock().expect("requests lock").push((
            url.to_string(),
            headers.to_vec(),
            body.clone(),
        ));
        self.responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .unwrap_or_else(|| Err(RelayError::Transport("No mock response queued".to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RelayClientConfig {
        RelayClientConfig {
            base_url: Url::parse("https://dev.vrlewds.com").expect("url"),
            creator_username: "Vee".to_string(),
            stream_key: "secret".to_string(),
            session_path: "/api/sps/session".to_string(),
            ingest_path: "/api/sps/ingest".to_string(),
        }
    }

    fn ok_session_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: HashMap::from([(
                "date".to_string(),
                "Wed, 21 Oct 2015 07:28:00 GMT".to_string(),
            )]),
            body: json!({
                "token": "tok_123",
                "expiresAt": "2026-03-06T05:00:00.000Z",
                "streamId": "stream_1",
                "serverNowMs": 1_000_000_i64,
                "maxSkewMs": 45000
            })
            .to_string(),
        }
    }

    #[test]
    fn create_session_sets_state_fields() {
        let mock = MockTransport::with_responses(vec![Ok(ok_session_response())]);
        let mut client = RelayClient::new(cfg(), mock);
        client.create_session(900_000).expect("session");

        assert_eq!(client.state().relay_token, "tok_123");
        assert_eq!(client.state().stream_id, "stream_1");
        assert_eq!(client.state().clock_offset_ms, 100_000);
        assert_eq!(client.state().max_skew_ms, 45_000);
    }

    #[test]
    fn ingest_401_clears_token() {
        let mock = MockTransport::with_responses(vec![
            Ok(ok_session_response()),
            Ok(HttpResponse {
                status: 401,
                headers: HashMap::new(),
                body: String::new(),
            }),
        ]);
        let mut client = RelayClient::new(cfg(), mock);
        let event = RelayEvent {
            seq: 1,
            ts: 10,
            intensity: 0.5,
            peak: 0.6,
            raw: 0.7,
            source: RelaySource {
                address: "/avatar/parameters/SPS_Contact".to_string(),
                arg_type: "f".to_string(),
            },
        };

        let error = client.push_event(&event, 1_000_000).expect_err("must fail");
        assert_eq!(error, RelayError::RelayTokenRejected);
        assert!(client.state().relay_token.is_empty());
        assert_eq!(client.state().relay_token_expires_at_ms, 0);
    }

    #[test]
    fn ingest_stale_timestamp_resyncs_clock_offset() {
        let mock = MockTransport::with_responses(vec![
            Ok(ok_session_response()),
            Ok(HttpResponse {
                status: 400,
                headers: HashMap::from([(
                    "date".to_string(),
                    "Wed, 21 Oct 2015 07:30:00 GMT".to_string(),
                )]),
                body: "Stale or invalid event timestamp".to_string(),
            }),
        ]);
        let mut client = RelayClient::new(cfg(), mock);
        let event = RelayEvent {
            seq: 1,
            ts: 10,
            intensity: 0.2,
            peak: 0.4,
            raw: 0.2,
            source: RelaySource {
                address: "/x".to_string(),
                arg_type: "f".to_string(),
            },
        };

        let now_ms = 1_000;
        let error = client.push_event(&event, now_ms).expect_err("must fail");
        assert_eq!(error, RelayError::TimestampRejectedResynced);
        assert!(client.state().clock_offset_ms > 0);
    }

    #[test]
    fn valid_existing_token_skips_session_request() {
        let mock = MockTransport::with_responses(vec![Ok(HttpResponse {
            status: 200,
            headers: HashMap::new(),
            body: String::new(),
        })]);
        let mut client = RelayClient::new(cfg(), mock);
        client.state.relay_token = "tok_live".to_string();
        client.state.relay_token_expires_at_ms = 1_000_000;

        let event = RelayEvent {
            seq: 10,
            ts: 10,
            intensity: 0.2,
            peak: 0.4,
            raw: 0.2,
            source: RelaySource {
                address: "/x".to_string(),
                arg_type: "f".to_string(),
            },
        };
        client.push_event(&event, 900_000).expect("ingest");
    }
}
