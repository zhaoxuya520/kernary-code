use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport};
use serde::{Deserialize, Serialize};

use crate::McpOAuthConfig;
use crate::protocol::{McpError, McpTransport};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpStreamableHttpConfig {
    pub endpoint: String,
    pub bearer_credential_id: Option<String>,
    pub oauth: Option<McpOAuthConfig>,
    #[serde(default)]
    pub legacy_sse_fallback: bool,
    pub request_timeout_millis: Option<u64>,
    pub max_response_bytes: Option<usize>,
}

pub struct StreamableHttpMcpTransport {
    config: McpStreamableHttpConfig,
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    last_event_id: Mutex<Option<String>>,
    authorization_challenge: Mutex<Option<String>>,
    closed: AtomicBool,
}

/// 一条已建立的 Server→Client SSE 通道。每次 `next_message` 只返回一个完整
/// JSON-RPC message；调用者处理并响应后可以继续读取下一条。
pub struct McpInboundStream {
    owner: Arc<StreamableHttpMcpTransport>,
    body: Box<dyn BufRead + Send>,
    consumed: usize,
    max_bytes: usize,
}

impl StreamableHttpMcpTransport {
    pub fn new(
        config: McpStreamableHttpConfig,
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Result<Arc<Self>, McpError> {
        validate_endpoint(&config.endpoint)?;
        let timeout = Duration::from_millis(config.request_timeout_millis.unwrap_or(30_000));
        if timeout == Duration::ZERO || timeout > Duration::from_secs(300) {
            return Err(McpError::new(
                "mcp-http-timeout-invalid",
                format!("{timeout:?}"),
            ));
        }
        let limit = config.max_response_bytes.unwrap_or(4 * 1024 * 1024);
        if !(1024..=32 * 1024 * 1024).contains(&limit) {
            return Err(McpError::new(
                "mcp-http-response-limit-invalid",
                limit.to_string(),
            ));
        }
        Ok(Arc::new(Self {
            config,
            credentials,
            transport,
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            last_event_id: Mutex::new(None),
            authorization_challenge: Mutex::new(None),
            closed: AtomicBool::new(false),
        }))
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.config.request_timeout_millis.unwrap_or(30_000))
    }

    fn response_limit(&self) -> usize {
        self.config.max_response_bytes.unwrap_or(4 * 1024 * 1024)
    }

    fn send(
        &self,
        payload: serde_json::Value,
        request_id: Option<u64>,
    ) -> Result<Option<serde_json::Value>, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::new("mcp-transport-closed", "streamable-http"));
        }
        let request =
            StreamingHttpRequest::json(self.config.endpoint.clone(), payload, self.timeout())
                .with_header("Accept", "application/json, text/event-stream");
        let request = self.prepare_request(request)?;
        let mut response = self
            .transport
            .send(request)
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        self.capture_session(&response.headers)?;
        if response.status == 202 && request_id.is_none() {
            return Ok(None);
        }
        if matches!(response.status, 401 | 403) {
            if let Some(challenge) = response.headers.get("www-authenticate") {
                validate_header_value(challenge, "www-authenticate")?;
                *self
                    .authorization_challenge
                    .lock()
                    .map_err(|_| McpError::new("mcp-authorization-challenge-poisoned", "lock"))? =
                    Some(challenge.clone());
            }
            return Err(McpError::new(
                "mcp-http-authorization-required",
                response.status.to_string(),
            ));
        }
        if !(200..300).contains(&response.status) {
            return Err(
                McpError::new("mcp-http-status", response.status.to_string())
                    .retryable(response.status == 429 || response.status >= 500),
            );
        }
        let content_type = response
            .headers
            .get("content-type")
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        if content_type.starts_with("application/json") {
            let bytes = read_capped(&mut response.body, self.response_limit())?;
            let value = serde_json::from_slice(&bytes)
                .map_err(|error| McpError::new("mcp-http-json-invalid", error.to_string()))?;
            return request_id
                .map(|id| parse_response(value, id))
                .transpose()
                .map(Option::flatten);
        }
        if content_type.starts_with("text/event-stream") {
            let id = request_id.ok_or_else(|| {
                McpError::new("mcp-http-notification-sse-unexpected", "notification")
            })?;
            let parsed = parse_sse_response(&mut response.body, id, self.response_limit())?;
            self.remember_last_event_id(parsed.last_event_id)?;
            return parsed.result.map_or_else(
                || self.resume_sse_response(id, parsed.retry_millis).map(Some),
                |result| Ok(Some(result)),
            );
        }
        Err(McpError::new("mcp-http-content-type-invalid", content_type))
    }

    fn prepare_request(
        &self,
        mut request: StreamingHttpRequest,
    ) -> Result<StreamingHttpRequest, McpError> {
        if let Some(session_id) = self
            .session_id
            .lock()
            .map_err(|_| McpError::new("mcp-session-poisoned", "lock"))?
            .clone()
        {
            request = request.with_header("Mcp-Session-Id", session_id);
        }
        if let Some(protocol_version) = self
            .protocol_version
            .lock()
            .map_err(|_| McpError::new("mcp-version-poisoned", "lock"))?
            .clone()
        {
            request = request.with_header("MCP-Protocol-Version", protocol_version);
        }
        let credential_id = self
            .config
            .bearer_credential_id
            .as_ref()
            .or_else(|| self.config.oauth.as_ref().map(|oauth| &oauth.credential_id));
        if let Some(credential_id) = credential_id {
            let secret = self
                .credentials
                .get(&CredentialId::new(credential_id.clone()))
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

    fn capture_session(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), McpError> {
        if let Some(session_id) = headers.get("mcp-session-id") {
            validate_header_value(session_id, "mcp-session-id")?;
            *self
                .session_id
                .lock()
                .map_err(|_| McpError::new("mcp-session-poisoned", "lock"))? =
                Some(session_id.clone());
        }
        Ok(())
    }

    pub fn open_inbound_stream(self: &Arc<Self>) -> Result<McpInboundStream, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::new("mcp-transport-closed", "streamable-http"));
        }
        let request = StreamingHttpRequest::get(self.config.endpoint.clone(), self.timeout())
            .with_header("Accept", "text/event-stream");
        let request = self.prepare_request(request)?;
        let response = self
            .transport
            .send(request)
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        self.capture_session(&response.headers)?;
        if !(200..300).contains(&response.status) {
            return Err(McpError::new(
                "mcp-http-inbound-status",
                response.status.to_string(),
            ));
        }
        if !response
            .headers
            .get("content-type")
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
        {
            return Err(McpError::new(
                "mcp-http-inbound-content-type",
                response
                    .headers
                    .get("content-type")
                    .cloned()
                    .unwrap_or_default(),
            ));
        }
        Ok(McpInboundStream {
            owner: self.clone(),
            body: response.body,
            consumed: 0,
            max_bytes: self.response_limit(),
        })
    }

    pub fn send_jsonrpc_result(
        &self,
        id: serde_json::Value,
        result: serde_json::Value,
    ) -> Result<(), McpError> {
        if !(id.is_string() || id.is_number()) {
            return Err(McpError::new(
                "mcp-jsonrpc-response-id-invalid",
                id.to_string(),
            ));
        }
        self.send(
            serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
            None,
        )?;
        Ok(())
    }

    pub fn authorization_challenge(&self) -> Result<Option<String>, McpError> {
        self.authorization_challenge
            .lock()
            .map_err(|_| McpError::new("mcp-authorization-challenge-poisoned", "lock"))
            .map(|challenge| challenge.clone())
    }

    fn remember_last_event_id(&self, event_id: Option<String>) -> Result<(), McpError> {
        if let Some(event_id) = event_id {
            validate_header_value(&event_id, "last-event-id")?;
            *self
                .last_event_id
                .lock()
                .map_err(|_| McpError::new("mcp-event-id-poisoned", "lock"))? = Some(event_id);
        }
        Ok(())
    }

    fn resume_sse_response(
        &self,
        request_id: u64,
        mut retry_millis: Option<u64>,
    ) -> Result<serde_json::Value, McpError> {
        const MAX_RESUME_ATTEMPTS: usize = 3;
        for _ in 0..MAX_RESUME_ATTEMPTS {
            let delay = retry_millis.unwrap_or(1_000).min(60_000);
            thread::sleep(Duration::from_millis(delay));
            let mut request =
                StreamingHttpRequest::get(self.config.endpoint.clone(), self.timeout())
                    .with_header("Accept", "text/event-stream");
            if let Some(last_event_id) = self
                .last_event_id
                .lock()
                .map_err(|_| McpError::new("mcp-event-id-poisoned", "lock"))?
                .clone()
            {
                request = request.with_header("Last-Event-ID", last_event_id);
            }
            let request = self.prepare_request(request)?;
            let mut response = self.transport.send(request).map_err(|error| {
                McpError::new(error.code, error.message).retryable(error.timeout)
            })?;
            self.capture_session(&response.headers)?;
            if !(200..300).contains(&response.status) {
                return Err(
                    McpError::new("mcp-http-resume-status", response.status.to_string())
                        .retryable(response.status == 429 || response.status >= 500),
                );
            }
            if !response
                .headers
                .get("content-type")
                .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
            {
                return Err(McpError::new(
                    "mcp-http-resume-content-type",
                    response
                        .headers
                        .get("content-type")
                        .cloned()
                        .unwrap_or_default(),
                ));
            }
            let parsed = parse_sse_response(&mut response.body, request_id, self.response_limit())?;
            self.remember_last_event_id(parsed.last_event_id)?;
            if let Some(result) = parsed.result {
                return Ok(result);
            }
            retry_millis = parsed.retry_millis.or(retry_millis);
        }
        Err(McpError::new("mcp-http-sse-resume-exhausted", request_id.to_string()).retryable(true))
    }
}

impl McpInboundStream {
    pub fn next_message(&mut self) -> Result<serde_json::Value, McpError> {
        let mut data_lines = Vec::new();
        let mut current_id = None::<String>;
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .body
                .read_line(&mut line)
                .map_err(|error| McpError::new("mcp-http-inbound-read", error.to_string()))?;
            if read == 0 {
                return Err(
                    McpError::new("mcp-http-inbound-eof", "SSE stream closed").retryable(true)
                );
            }
            self.consumed = self.consumed.saturating_add(read);
            if self.consumed > self.max_bytes {
                return Err(McpError::new(
                    "mcp-http-inbound-too-large",
                    self.max_bytes.to_string(),
                ));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if let Some(event_id) = current_id.take() {
                    self.owner.remember_last_event_id(Some(event_id))?;
                }
                if data_lines.is_empty() || data_lines.iter().all(String::is_empty) {
                    data_lines.clear();
                    continue;
                }
                let value = serde_json::from_str::<serde_json::Value>(&data_lines.join("\n"))
                    .map_err(|error| {
                        McpError::new("mcp-http-inbound-json-invalid", error.to_string())
                    })?;
                if value.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
                    return Err(McpError::new(
                        "mcp-http-inbound-jsonrpc-invalid",
                        value.to_string(),
                    ));
                }
                return Ok(value);
            }
            if let Some(data) = trimmed.strip_prefix("data:") {
                data_lines.push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            } else if let Some(id) = trimmed.strip_prefix("id:") {
                let id = id.strip_prefix(' ').unwrap_or(id);
                if id.contains('\0') {
                    return Err(McpError::new("mcp-http-sse-event-id-invalid", "NUL"));
                }
                current_id = Some(id.to_owned());
            }
        }
    }
}

impl McpTransport for StreamableHttpMcpTransport {
    fn kind(&self) -> &'static str {
        "streamable-http"
    }

    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send(
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":method,
                "params":params
            }),
            Some(id),
        )?
        .ok_or_else(|| McpError::new("mcp-http-response-missing", method))
    }

    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        self.send(
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":method,
                "params":params
            }),
            None,
        )?;
        Ok(())
    }

    fn set_protocol_version(&self, protocol_version: &str) -> Result<(), McpError> {
        validate_header_value(protocol_version, "mcp-protocol-version")?;
        *self
            .protocol_version
            .lock()
            .map_err(|_| McpError::new("mcp-version-poisoned", "lock"))? =
            Some(protocol_version.to_owned());
        Ok(())
    }

    fn poll_notifications(&self) -> Result<Vec<serde_json::Value>, McpError> {
        let mut request = StreamingHttpRequest::get(self.config.endpoint.clone(), self.timeout())
            .with_header("Accept", "text/event-stream");
        if let Some(last_event_id) = self
            .last_event_id
            .lock()
            .map_err(|_| McpError::new("mcp-event-id-poisoned", "lock"))?
            .clone()
        {
            request = request.with_header("Last-Event-ID", last_event_id);
        }
        let request = self.prepare_request(request)?;
        let mut response = self
            .transport
            .send(request)
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        if response.status == 405 {
            return Err(McpError::new("mcp-http-poll-unavailable", "405"));
        }
        if !(200..300).contains(&response.status) {
            return Err(McpError::new(
                "mcp-http-poll-status",
                response.status.to_string(),
            ));
        }
        if !response
            .headers
            .get("content-type")
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"))
        {
            return Err(McpError::new(
                "mcp-http-poll-content-type",
                response
                    .headers
                    .get("content-type")
                    .cloned()
                    .unwrap_or_default(),
            ));
        }
        let (events, last_event_id) =
            parse_sse_events(&mut response.body, self.response_limit(), 1024)?;
        if let Some(last_event_id) = last_event_id {
            validate_header_value(&last_event_id, "last-event-id")?;
            *self
                .last_event_id
                .lock()
                .map_err(|_| McpError::new("mcp-event-id-poisoned", "lock"))? = Some(last_event_id);
        }
        Ok(events)
    }

    fn close(&self) -> Result<(), McpError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let session_id = self
            .session_id
            .lock()
            .map_err(|_| McpError::new("mcp-session-poisoned", "lock"))?
            .take();
        if let Some(session_id) = session_id {
            let request =
                StreamingHttpRequest::delete(self.config.endpoint.clone(), self.timeout())
                    .with_header("Mcp-Session-Id", session_id);
            let request = self.prepare_request(request)?;
            let response = self.transport.send(request).map_err(|error| {
                McpError::new(error.code, error.message).retryable(error.timeout)
            })?;
            if !matches!(response.status, 200 | 202 | 204 | 404 | 405) {
                return Err(McpError::new(
                    "mcp-http-session-close-status",
                    response.status.to_string(),
                ));
            }
        }
        Ok(())
    }
}

struct ParsedSseResponse {
    result: Option<serde_json::Value>,
    last_event_id: Option<String>,
    retry_millis: Option<u64>,
}

fn parse_sse_response(
    reader: &mut Box<dyn BufRead + Send>,
    request_id: u64,
    max_bytes: usize,
) -> Result<ParsedSseResponse, McpError> {
    let mut consumed = 0_usize;
    let mut data_lines = Vec::new();
    let mut current_id = None::<String>;
    let mut last_event_id = None::<String>;
    let mut retry_millis = None::<u64>;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| McpError::new("mcp-http-sse-read", error.to_string()))?;
        if read == 0 {
            if current_id.is_some() {
                last_event_id = current_id;
            }
            return Ok(ParsedSseResponse {
                result: None,
                last_event_id,
                retry_millis,
            });
        }
        consumed = consumed.saturating_add(read);
        if consumed > max_bytes {
            return Err(McpError::new(
                "mcp-http-response-too-large",
                max_bytes.to_string(),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if current_id.is_some() {
                last_event_id.clone_from(&current_id);
            }
            current_id = None;
            if data_lines.is_empty() || data_lines.iter().all(String::is_empty) {
                data_lines.clear();
                continue;
            }
            let data = data_lines.join("\n");
            data_lines.clear();
            let value = serde_json::from_str::<serde_json::Value>(&data)
                .map_err(|error| McpError::new("mcp-http-sse-json-invalid", error.to_string()))?;
            if let Some(result) = parse_response(value, request_id)? {
                return Ok(ParsedSseResponse {
                    result: Some(result),
                    last_event_id,
                    retry_millis,
                });
            }
        } else if let Some(data) = trimmed.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        } else if let Some(id) = trimmed.strip_prefix("id:") {
            let id = id.strip_prefix(' ').unwrap_or(id);
            if id.contains('\0') {
                return Err(McpError::new("mcp-http-sse-event-id-invalid", "NUL"));
            }
            current_id = Some(id.to_owned());
        } else if let Some(retry) = trimmed.strip_prefix("retry:") {
            let retry = retry.strip_prefix(' ').unwrap_or(retry);
            retry_millis = Some(
                retry
                    .parse::<u64>()
                    .map_err(|_| McpError::new("mcp-http-sse-retry-invalid", retry))?,
            );
        }
    }
}

fn parse_sse_events(
    reader: &mut Box<dyn BufRead + Send>,
    max_bytes: usize,
    max_events: usize,
) -> Result<(Vec<serde_json::Value>, Option<String>), McpError> {
    let mut consumed = 0_usize;
    let mut events = Vec::new();
    let mut data_lines = Vec::new();
    let mut current_id = None::<String>;
    let mut last_event_id = None::<String>;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| McpError::new("mcp-http-sse-read", error.to_string()))?;
        if read == 0 {
            if !data_lines.is_empty() {
                push_sse_event(&mut events, &data_lines, max_events)?;
                if current_id.is_some() {
                    last_event_id = current_id;
                }
            }
            return Ok((events, last_event_id));
        }
        consumed = consumed.saturating_add(read);
        if consumed > max_bytes {
            return Err(McpError::new(
                "mcp-http-response-too-large",
                max_bytes.to_string(),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data_lines.is_empty() {
                push_sse_event(&mut events, &data_lines, max_events)?;
                data_lines.clear();
                if current_id.is_some() {
                    last_event_id.clone_from(&current_id);
                }
                current_id = None;
            }
        } else if let Some(data) = trimmed.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        } else if let Some(id) = trimmed.strip_prefix("id:") {
            let id = id.strip_prefix(' ').unwrap_or(id);
            if id.contains('\0') {
                return Err(McpError::new("mcp-http-sse-event-id-invalid", "NUL"));
            }
            current_id = Some(id.to_owned());
        }
    }
}

fn push_sse_event(
    events: &mut Vec<serde_json::Value>,
    data_lines: &[String],
    max_events: usize,
) -> Result<(), McpError> {
    if events.len() >= max_events {
        return Err(McpError::new(
            "mcp-http-sse-event-limit",
            max_events.to_string(),
        ));
    }
    let value = serde_json::from_str::<serde_json::Value>(&data_lines.join("\n"))
        .map_err(|error| McpError::new("mcp-http-sse-json-invalid", error.to_string()))?;
    if value.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err(McpError::new("mcp-http-jsonrpc-version-invalid", "poll"));
    }
    events.push(value);
    Ok(())
}

fn parse_response(
    value: serde_json::Value,
    request_id: u64,
) -> Result<Option<serde_json::Value>, McpError> {
    let Some(object) = value.as_object() else {
        return Err(McpError::new(
            "mcp-http-message-not-object",
            request_id.to_string(),
        ));
    };
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err(McpError::new(
            "mcp-http-jsonrpc-version-invalid",
            request_id.to_string(),
        ));
    }
    if object.get("id").and_then(serde_json::Value::as_u64) != Some(request_id) {
        return Ok(None);
    }
    if let Some(error) = object.get("error") {
        let code = error
            .get("code")
            .map_or_else(|| "unknown".to_owned(), serde_json::Value::to_string);
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("MCP server error")
            .chars()
            .take(512)
            .collect::<String>();
        return Err(McpError::new(format!("mcp-jsonrpc-error-{code}"), message));
    }
    Ok(object.get("result").cloned())
}

fn read_capped(
    reader: &mut Box<dyn BufRead + Send>,
    max_bytes: usize,
) -> Result<Vec<u8>, McpError> {
    let mut output = Vec::new();
    reader
        .take(
            u64::try_from(max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut output)
        .map_err(|error| McpError::new("mcp-http-read", error.to_string()))?;
    if output.len() > max_bytes {
        return Err(McpError::new(
            "mcp-http-response-too-large",
            max_bytes.to_string(),
        ));
    }
    Ok(output)
}

fn validate_header_value(value: &str, name: &'static str) -> Result<(), McpError> {
    if value.is_empty() || value.len() > 1024 || value.contains(['\r', '\n']) {
        return Err(McpError::new("mcp-http-header-invalid", name));
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str) -> Result<(), McpError> {
    let parsed = url::Url::parse(endpoint)
        .map_err(|error| McpError::new("mcp-http-endpoint-invalid", error.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| McpError::new("mcp-http-endpoint-invalid", "Endpoint 缺少 host"))?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.scheme(), "http" | "https")
    {
        return Err(McpError::new(
            "mcp-http-endpoint-invalid",
            "Endpoint 不允许空主机、userinfo、query 或 fragment",
        ));
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(McpError::new(
            "mcp-http-endpoint-insecure",
            "远程 MCP 必须 HTTPS；HTTP 只允许精确 loopback",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::io::{BufReader, Cursor};

    use harness_auth::{CredentialStore, MemoryCredentialStore};
    use harness_http::{
        HttpMethod, HttpTransportError, StreamingHttpRequest, StreamingHttpResponse,
    };

    use super::*;

    #[test]
    fn endpoint_accepts_root_path_and_keeps_remote_http_denied() {
        assert!(validate_endpoint("http://localhost:3210").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:3210/").is_ok());
        assert!(validate_endpoint("https://mcp.example.test").is_ok());
        assert!(validate_endpoint("http://mcp.example.test/mcp").is_err());
        assert!(validate_endpoint("https://mcp.example.test/mcp?token=bad").is_err());
        assert!(validate_endpoint("https://user@mcp.example.test/mcp").is_err());
    }

    struct CapturedRequest {
        method: HttpMethod,
        headers: BTreeMap<String, String>,
        sensitive_header_names: Vec<String>,
    }

    struct MockHttpTransport {
        responses: Mutex<VecDeque<StreamingHttpResponse>>,
        requests: Mutex<Vec<CapturedRequest>>,
    }

    impl StreamingHttpTransport for MockHttpTransport {
        fn send(
            &self,
            request: StreamingHttpRequest,
        ) -> Result<StreamingHttpResponse, HttpTransportError> {
            self.requests
                .lock()
                .expect("requests")
                .push(CapturedRequest {
                    method: request.method,
                    headers: request.headers,
                    sensitive_header_names: request
                        .sensitive_headers
                        .into_iter()
                        .map(|header| header.name)
                        .collect(),
                });
            self.responses
                .lock()
                .expect("responses")
                .pop_front()
                .ok_or_else(|| HttpTransportError {
                    code: "mock-empty".to_owned(),
                    message: "no response".to_owned(),
                    timeout: false,
                })
        }
    }

    fn response(status: u16, content_type: &str, body: &str) -> StreamingHttpResponse {
        StreamingHttpResponse {
            status,
            headers: [("content-type".to_owned(), content_type.to_owned())]
                .into_iter()
                .collect(),
            body: Box::new(BufReader::new(Cursor::new(body.as_bytes().to_vec()))),
        }
    }

    #[test]
    fn streamable_http_handles_json_sse_session_version_and_secret_header() {
        let mut initialize = response(
            200,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        );
        initialize
            .headers
            .insert("mcp-session-id".to_owned(), "session-1".to_owned());
        let sse = response(
            200,
            "text/event-stream",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"value\":42}}\n\n",
        );
        let accepted = response(202, "application/json", "");
        let mock = Arc::new(MockHttpTransport {
            responses: Mutex::new([initialize, sse, accepted].into_iter().collect()),
            requests: Mutex::new(vec![]),
        });
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials
            .put(
                &CredentialId::new("mcp:test"),
                SecretString::new("secret-token"),
            )
            .expect("credential");
        let transport = StreamableHttpMcpTransport::new(
            McpStreamableHttpConfig {
                endpoint: "https://example.test/mcp".to_owned(),
                bearer_credential_id: Some("mcp:test".to_owned()),
                oauth: None,
                legacy_sse_fallback: false,
                request_timeout_millis: Some(1_000),
                max_response_bytes: Some(16 * 1024),
            },
            credentials,
            mock.clone(),
        )
        .expect("transport");
        assert_eq!(
            transport
                .request("initialize", serde_json::json!({}))
                .expect("initialize")["ok"],
            true
        );
        transport
            .set_protocol_version("2025-11-25")
            .expect("version");
        assert_eq!(
            transport
                .request("tools/call", serde_json::json!({}))
                .expect("sse")["value"],
            42
        );
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .expect("notify");
        let requests = mock.requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1]
                .headers
                .get("Mcp-Session-Id")
                .map(String::as_str),
            Some("session-1")
        );
        assert_eq!(
            requests[1]
                .headers
                .get("MCP-Protocol-Version")
                .map(String::as_str),
            Some("2025-11-25")
        );
        assert_eq!(requests[0].sensitive_header_names, vec!["Authorization"]);
    }

    #[test]
    fn streamable_http_rejects_insecure_remote_and_header_injection() {
        let credentials = Arc::new(MemoryCredentialStore::new());
        let mock = Arc::new(MockHttpTransport {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(vec![]),
        });
        assert!(
            StreamableHttpMcpTransport::new(
                McpStreamableHttpConfig {
                    endpoint: "http://example.com/mcp".to_owned(),
                    bearer_credential_id: None,
                    oauth: None,
                    legacy_sse_fallback: false,
                    request_timeout_millis: None,
                    max_response_bytes: None,
                },
                credentials,
                mock,
            )
            .is_err()
        );
        assert!(validate_header_value("bad\r\nheader", "session").is_err());
    }

    #[test]
    fn request_sse_graceful_close_respects_retry_and_resumes_with_event_id() {
        let priming = response(
            200,
            "text/event-stream",
            "id: event-1\nretry: 1\ndata: \n\n",
        );
        let resumed = response(
            200,
            "text/event-stream",
            "id: event-2\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        );
        let mock = Arc::new(MockHttpTransport {
            responses: Mutex::new([priming, resumed].into_iter().collect()),
            requests: Mutex::new(vec![]),
        });
        let transport = StreamableHttpMcpTransport::new(
            McpStreamableHttpConfig {
                endpoint: "https://example.test/mcp".to_owned(),
                bearer_credential_id: None,
                oauth: None,
                legacy_sse_fallback: false,
                request_timeout_millis: Some(1_000),
                max_response_bytes: Some(16 * 1024),
            },
            Arc::new(MemoryCredentialStore::new()),
            mock.clone(),
        )
        .expect("transport");
        assert_eq!(
            transport
                .request("tools/call", serde_json::json!({}))
                .expect("resumed response")["ok"],
            true
        );
        let requests = mock.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, HttpMethod::Get);
        assert_eq!(
            requests[1].headers.get("Last-Event-ID").map(String::as_str),
            Some("event-1")
        );
    }

    #[test]
    fn streamable_http_poll_resumes_and_closes_session() {
        let mut initialize = response(
            200,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        );
        initialize
            .headers
            .insert("mcp-session-id".to_owned(), "session-poll".to_owned());
        let first_poll = response(
            200,
            "text/event-stream",
            "id: event-1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
        );
        let second_poll = response(
            200,
            "text/event-stream",
            "id: event-2\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/list_changed\"}\n\n",
        );
        let closed = response(204, "application/json", "");
        let mock = Arc::new(MockHttpTransport {
            responses: Mutex::new(
                [initialize, first_poll, second_poll, closed]
                    .into_iter()
                    .collect(),
            ),
            requests: Mutex::new(vec![]),
        });
        let transport = StreamableHttpMcpTransport::new(
            McpStreamableHttpConfig {
                endpoint: "https://example.test/mcp".to_owned(),
                bearer_credential_id: None,
                oauth: None,
                legacy_sse_fallback: false,
                request_timeout_millis: Some(1_000),
                max_response_bytes: Some(16 * 1024),
            },
            Arc::new(MemoryCredentialStore::new()),
            mock.clone(),
        )
        .expect("transport");
        transport
            .request("initialize", serde_json::json!({}))
            .expect("initialize");
        transport
            .set_protocol_version("2025-11-25")
            .expect("version");
        assert_eq!(transport.poll_notifications().expect("first poll").len(), 1);
        assert_eq!(
            transport.poll_notifications().expect("second poll").len(),
            1
        );
        transport.close().expect("close");
        let requests = mock.requests.lock().expect("requests");
        assert_eq!(requests[1].method, HttpMethod::Get);
        assert_eq!(
            requests[2].headers.get("Last-Event-ID").map(String::as_str),
            Some("event-1")
        );
        assert_eq!(requests[3].method, HttpMethod::Delete);
        assert_eq!(
            requests[3]
                .headers
                .get("Mcp-Session-Id")
                .map(String::as_str),
            Some("session-poll")
        );
    }
}
