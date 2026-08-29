use std::collections::{BTreeMap, VecDeque};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport};
use url::Url;

use crate::http::McpStreamableHttpConfig;
use crate::protocol::{McpError, McpTransport};

type PendingSender = mpsc::SyncSender<Result<serde_json::Value, McpError>>;

pub struct LegacySseMcpTransport {
    message_endpoint: String,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
    credential_id: Option<String>,
    pending: Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: Arc<Mutex<VecDeque<serde_json::Value>>>,
    next_id: AtomicU64,
    timeout: Duration,
    closed: AtomicBool,
}

impl LegacySseMcpTransport {
    pub fn connect(
        config: &McpStreamableHttpConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Arc<Self>, McpError> {
        let timeout = Duration::from_millis(config.request_timeout_millis.unwrap_or(30_000));
        let credential_id = config.bearer_credential_id.clone().or_else(|| {
            config
                .oauth
                .as_ref()
                .map(|oauth| oauth.credential_id.clone())
        });
        let request = authenticated_request(
            StreamingHttpRequest::get(config.endpoint.clone(), timeout)
                .with_header("Accept", "text/event-stream"),
            &credentials,
            credential_id.as_deref(),
        )?;
        let mut response = transport
            .send(request)
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        if response.status != 200 {
            return Err(McpError::new(
                "mcp-legacy-sse-status",
                response.status.to_string(),
            ));
        }
        if !response
            .headers
            .get("content-type")
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
        {
            return Err(McpError::new(
                "mcp-legacy-sse-content-type",
                response
                    .headers
                    .get("content-type")
                    .cloned()
                    .unwrap_or_default(),
            ));
        }
        let frame = read_frame(
            &mut response.body,
            config.max_response_bytes.unwrap_or(1024 * 1024),
        )?
        .ok_or_else(|| McpError::new("mcp-legacy-endpoint-missing", "empty SSE"))?;
        if frame.event.as_deref() != Some("endpoint") {
            return Err(McpError::new(
                "mcp-legacy-endpoint-event-missing",
                frame.event.unwrap_or_default(),
            ));
        }
        let message_endpoint = resolve_same_origin(&config.endpoint, &frame.data)?;
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let notifications = Arc::new(Mutex::new(VecDeque::new()));
        spawn_reader(
            response.body,
            pending.clone(),
            notifications.clone(),
            config.max_response_bytes.unwrap_or(1024 * 1024),
        );
        Ok(Arc::new(Self {
            message_endpoint,
            credentials,
            transport,
            credential_id,
            pending,
            notifications,
            next_id: AtomicU64::new(1),
            timeout,
            closed: AtomicBool::new(false),
        }))
    }

    fn post(&self, payload: serde_json::Value) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::new("mcp-transport-closed", "legacy-sse"));
        }
        let request = authenticated_request(
            StreamingHttpRequest::json(self.message_endpoint.clone(), payload, self.timeout)
                .with_header("Accept", "application/json"),
            &self.credentials,
            self.credential_id.as_deref(),
        )?;
        let response = self
            .transport
            .send(request)
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        if !(200..300).contains(&response.status) {
            return Err(McpError::new(
                "mcp-legacy-post-status",
                response.status.to_string(),
            ));
        }
        Ok(())
    }
}

impl McpTransport for LegacySseMcpTransport {
    fn kind(&self) -> &'static str {
        "legacy-http-sse"
    }

    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .map_err(|_| McpError::new("mcp-legacy-pending-poisoned", "lock"))?
            .insert(id, sender);
        if let Err(error) = self.post(serde_json::json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":method,
            "params":params
        })) {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        match receiver.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                let _ = self.notify(
                    "notifications/cancelled",
                    serde_json::json!({"requestId":id,"reason":"client timeout"}),
                );
                Err(McpError::new("mcp-legacy-request-timeout", method).retryable(true))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(McpError::new("mcp-legacy-response-channel-closed", method))
            }
        }
    }

    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        self.post(serde_json::json!({
            "jsonrpc":"2.0",
            "method":method,
            "params":params
        }))
    }

    fn set_protocol_version(&self, _protocol_version: &str) -> Result<(), McpError> {
        Ok(())
    }

    fn poll_notifications(&self) -> Result<Vec<serde_json::Value>, McpError> {
        let mut notifications = self
            .notifications
            .lock()
            .map_err(|_| McpError::new("mcp-notifications-poisoned", "legacy-sse"))?;
        Ok(notifications.drain(..).collect())
    }

    fn close(&self) -> Result<(), McpError> {
        self.closed.store(true, Ordering::SeqCst);
        fail_all(
            &self.pending,
            McpError::new("mcp-transport-closed", "legacy-sse"),
        );
        Ok(())
    }
}

struct SseFrame {
    event: Option<String>,
    data: String,
}

fn read_frame(
    reader: &mut Box<dyn BufRead + Send>,
    max_bytes: usize,
) -> Result<Option<SseFrame>, McpError> {
    let mut consumed = 0_usize;
    let mut event = None;
    let mut data = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| McpError::new("mcp-legacy-sse-read", error.to_string()))?;
        if read == 0 {
            return if data.is_empty() {
                Ok(None)
            } else {
                Ok(Some(SseFrame {
                    event,
                    data: data.join("\n"),
                }))
            };
        }
        consumed = consumed.saturating_add(read);
        if consumed > max_bytes {
            return Err(McpError::new(
                "mcp-legacy-sse-frame-too-large",
                max_bytes.to_string(),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if data.is_empty() {
                continue;
            }
            return Ok(Some(SseFrame {
                event,
                data: data.join("\n"),
            }));
        }
        if let Some(value) = trimmed.strip_prefix("event:") {
            event = Some(value.strip_prefix(' ').unwrap_or(value).to_owned());
        } else if let Some(value) = trimmed.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
    }
}

fn spawn_reader(
    mut reader: Box<dyn BufRead + Send>,
    pending: Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: Arc<Mutex<VecDeque<serde_json::Value>>>,
    max_bytes: usize,
) {
    thread::spawn(move || {
        loop {
            match read_frame(&mut reader, max_bytes) {
                Ok(Some(frame)) => match serde_json::from_str::<serde_json::Value>(&frame.data) {
                    Ok(message) => dispatch(&pending, &notifications, message),
                    Err(error) => {
                        fail_all(
                            &pending,
                            McpError::new("mcp-legacy-json-invalid", error.to_string()),
                        );
                        return;
                    }
                },
                Ok(None) => {
                    fail_all(&pending, McpError::new("mcp-legacy-sse-closed", "EOF"));
                    return;
                }
                Err(error) => {
                    fail_all(&pending, error);
                    return;
                }
            }
        }
    });
}

fn dispatch(
    pending: &Arc<Mutex<BTreeMap<u64, PendingSender>>>,
    notifications: &Arc<Mutex<VecDeque<serde_json::Value>>>,
    value: serde_json::Value,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return;
    }
    let Some(id) = object.get("id").and_then(serde_json::Value::as_u64) else {
        if object
            .get("method")
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            let mut queued = notifications
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queued.len() == 1024 {
                queued.pop_front();
            }
            queued.push_back(value);
        }
        return;
    };
    let sender = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    let Some(sender) = sender else {
        return;
    };
    let result = if let Some(error) = object.get("error") {
        Err(McpError::new(
            "mcp-legacy-jsonrpc-error",
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("server error")
                .chars()
                .take(512)
                .collect::<String>(),
        ))
    } else {
        object
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::new("mcp-legacy-result-missing", id.to_string()))
    };
    let _ = sender.send(result);
}

fn fail_all(pending: &Arc<Mutex<BTreeMap<u64, PendingSender>>>, error: McpError) {
    let senders = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .split_off(&0);
    for sender in senders.into_values() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn authenticated_request(
    mut request: StreamingHttpRequest,
    credentials: &Arc<dyn CredentialStore>,
    credential_id: Option<&str>,
) -> Result<StreamingHttpRequest, McpError> {
    if let Some(credential_id) = credential_id {
        let secret = credentials
            .get(&CredentialId::new(credential_id))
            .map_err(|error| McpError::new(error.code, error.message))?
            .ok_or_else(|| McpError::new("mcp-http-credential-missing", credential_id))?;
        let bearer = secret
            .expose_secret()
            .map(|value| SecretString::new(format!("Bearer {value}")))
            .map_err(|error| McpError::new(error.code, error.message))?;
        request = request.with_sensitive_header("Authorization", bearer);
    }
    Ok(request)
}

fn resolve_same_origin(base: &str, endpoint: &str) -> Result<String, McpError> {
    let base = Url::parse(base)
        .map_err(|error| McpError::new("mcp-legacy-base-url", error.to_string()))?;
    let endpoint = base
        .join(endpoint)
        .map_err(|error| McpError::new("mcp-legacy-endpoint-url", error.to_string()))?;
    if base.scheme() != endpoint.scheme()
        || base.host_str() != endpoint.host_str()
        || base.port_or_known_default() != endpoint.port_or_known_default()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err(McpError::new(
            "mcp-legacy-endpoint-cross-origin",
            endpoint.to_string(),
        ));
    }
    Ok(endpoint.into())
}
