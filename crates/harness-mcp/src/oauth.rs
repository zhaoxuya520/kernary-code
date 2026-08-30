use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::protocol::McpError;

const OAUTH_FLOW_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_METADATA_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpOAuthConfig {
    pub client_id: String,
    pub resource_metadata_url: String,
    pub credential_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub callback_port: Option<u16>,
    pub expected_issuer: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpOAuthStart {
    pub server_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub expires_at_millis: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpOAuthStatus {
    pub server_id: String,
    pub configured: bool,
    pub pending: bool,
    pub authenticated: bool,
    pub credential_id: Option<String>,
}

#[derive(Clone, Debug)]
struct AuthorizationMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
}

struct PendingOAuth {
    state: String,
    verifier: String,
    redirect_uri: String,
    resource: String,
    metadata: AuthorizationMetadata,
    receiver: mpsc::Receiver<Result<OAuthCallback, McpError>>,
    expires_at_millis: i64,
}

struct OAuthCallback {
    code: String,
    state: String,
    issuer: String,
}

pub struct McpOAuthCoordinator {
    credentials: Arc<dyn CredentialStore>,
    transport: Arc<dyn StreamingHttpTransport>,
    pending: Mutex<BTreeMap<String, PendingOAuth>>,
}

impl McpOAuthCoordinator {
    pub fn new(
        credentials: Arc<dyn CredentialStore>,
        transport: Arc<dyn StreamingHttpTransport>,
    ) -> Self {
        Self {
            credentials,
            transport,
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn start(
        &self,
        server_id: &str,
        resource_endpoint: &str,
        config: &McpOAuthConfig,
    ) -> Result<McpOAuthStart, McpError> {
        if self
            .pending
            .lock()
            .map_err(|_| McpError::new("mcp-oauth-state-poisoned", "pending"))?
            .contains_key(server_id)
        {
            return Err(McpError::new("mcp-oauth-already-pending", server_id));
        }
        validate_oauth_config(resource_endpoint, config)?;
        let (resource, metadata) = self.discover(resource_endpoint, config)?;
        let listener = TcpListener::bind(("127.0.0.1", config.callback_port.unwrap_or(0)))
            .map_err(|error| McpError::new("mcp-oauth-callback-bind", error.to_string()))?;
        let callback_port = listener
            .local_addr()
            .map_err(|error| McpError::new("mcp-oauth-callback-address", error.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");
        let state = random_base64url(32)?;
        let verifier = random_base64url(32)?;
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        let mut authorization_url = Url::parse(&metadata.authorization_endpoint)
            .map_err(|error| McpError::new("mcp-oauth-authorization-url", error.to_string()))?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &config.client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &resource);
            if !config.scopes.is_empty() {
                query.append_pair("scope", &config.scopes.join(" "));
            }
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        spawn_callback_listener(listener, sender);
        let expires_at_millis = now_millis()
            .saturating_add(i64::try_from(OAUTH_FLOW_TIMEOUT.as_millis()).unwrap_or(i64::MAX));
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| McpError::new("mcp-oauth-state-poisoned", "pending"))?;
        if pending.contains_key(server_id) {
            return Err(McpError::new("mcp-oauth-already-pending", server_id));
        }
        pending.insert(
            server_id.to_owned(),
            PendingOAuth {
                state,
                verifier,
                redirect_uri: redirect_uri.clone(),
                resource,
                metadata,
                receiver,
                expires_at_millis,
            },
        );
        Ok(McpOAuthStart {
            server_id: server_id.to_owned(),
            authorization_url: authorization_url.into(),
            redirect_uri,
            expires_at_millis,
        })
    }

    pub fn finish(
        &self,
        server_id: &str,
        config: &McpOAuthConfig,
    ) -> Result<McpOAuthStatus, McpError> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| McpError::new("mcp-oauth-state-poisoned", "pending"))?
            .remove(server_id)
            .ok_or_else(|| McpError::new("mcp-oauth-not-pending", server_id))?;
        if pending.expires_at_millis < now_millis() {
            return Err(McpError::new("mcp-oauth-flow-expired", server_id));
        }
        let callback = match pending.receiver.try_recv() {
            Ok(callback) => callback?,
            Err(mpsc::TryRecvError::Empty) => {
                self.pending
                    .lock()
                    .map_err(|_| McpError::new("mcp-oauth-state-poisoned", "pending"))?
                    .insert(server_id.to_owned(), pending);
                return Err(McpError::new("mcp-oauth-callback-pending", server_id).retryable(true));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(McpError::new("mcp-oauth-callback-closed", server_id));
            }
        };
        if !constant_time_eq(callback.state.as_bytes(), pending.state.as_bytes()) {
            return Err(McpError::new("mcp-oauth-state-mismatch", server_id));
        }
        if callback.issuer != pending.metadata.issuer {
            return Err(McpError::new("mcp-oauth-issuer-mismatch", callback.issuer));
        }
        let token = self.exchange_token(
            &pending.metadata,
            vec![
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("code".to_owned(), callback.code),
                ("redirect_uri".to_owned(), pending.redirect_uri),
                ("client_id".to_owned(), config.client_id.clone()),
                ("code_verifier".to_owned(), pending.verifier),
                ("resource".to_owned(), pending.resource),
            ],
        )?;
        self.store_tokens(config, token)?;
        self.status(server_id, Some(config))
    }

    pub fn refresh(
        &self,
        server_id: &str,
        resource_endpoint: &str,
        config: &McpOAuthConfig,
    ) -> Result<McpOAuthStatus, McpError> {
        let (resource, metadata) = self.discover(resource_endpoint, config)?;
        let refresh_id = CredentialId::new(format!("{}:refresh", config.credential_id));
        let refresh = self
            .credentials
            .get(&refresh_id)
            .map_err(|error| McpError::new(error.code, error.message))?
            .ok_or_else(|| McpError::new("mcp-oauth-refresh-token-missing", server_id))?;
        let refresh = refresh
            .expose_secret()
            .map(str::to_owned)
            .map_err(|error| McpError::new(error.code, error.message))?;
        let token = self.exchange_token(
            &metadata,
            vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), refresh),
                ("client_id".to_owned(), config.client_id.clone()),
                ("resource".to_owned(), resource),
            ],
        )?;
        self.store_tokens(config, token)?;
        self.status(server_id, Some(config))
    }

    pub fn status(
        &self,
        server_id: &str,
        config: Option<&McpOAuthConfig>,
    ) -> Result<McpOAuthStatus, McpError> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| McpError::new("mcp-oauth-state-poisoned", "pending"))?
            .contains_key(server_id);
        let authenticated = match config {
            Some(config) => self
                .credentials
                .get(&CredentialId::new(config.credential_id.clone()))
                .map_err(|error| McpError::new(error.code, error.message))?
                .is_some(),
            None => false,
        };
        Ok(McpOAuthStatus {
            server_id: server_id.to_owned(),
            configured: config.is_some(),
            pending,
            authenticated,
            credential_id: config.map(|config| config.credential_id.clone()),
        })
    }

    fn discover(
        &self,
        resource_endpoint: &str,
        config: &McpOAuthConfig,
    ) -> Result<(String, AuthorizationMetadata), McpError> {
        let protected = fetch_json(
            &self.transport,
            &config.resource_metadata_url,
            Duration::from_secs(15),
        )?;
        let resource = protected
            .get("resource")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| McpError::new("mcp-oauth-resource-missing", "resource metadata"))?
            .to_owned();
        if normalize_url(&resource)? != normalize_url(resource_endpoint)? {
            return Err(McpError::new("mcp-oauth-resource-mismatch", resource));
        }
        let servers = protected
            .get("authorization_servers")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                McpError::new(
                    "mcp-oauth-authorization-server-missing",
                    "resource metadata",
                )
            })?;
        let issuer = if let Some(expected) = &config.expected_issuer {
            let expected_normalized = normalize_url(expected)?;
            servers
                .iter()
                .filter_map(serde_json::Value::as_str)
                .find(|server| {
                    normalize_url(server).is_ok_and(|server| server == expected_normalized)
                })
                .ok_or_else(|| McpError::new("mcp-oauth-issuer-mismatch", expected))?
                .to_owned()
        } else {
            servers
                .first()
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    McpError::new(
                        "mcp-oauth-authorization-server-missing",
                        "resource metadata",
                    )
                })?
                .to_owned()
        };
        let metadata_url = authorization_metadata_url(&issuer)?;
        let authorization = fetch_json(&self.transport, &metadata_url, Duration::from_secs(15))?;
        let returned_issuer = required_url_field(&authorization, "issuer")?;
        if normalize_url(&returned_issuer)? != normalize_url(&issuer)? {
            return Err(McpError::new(
                "mcp-oauth-metadata-issuer-mismatch",
                returned_issuer,
            ));
        }
        let methods = authorization
            .get("code_challenge_methods_supported")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| McpError::new("mcp-oauth-pkce-metadata-missing", &issuer))?;
        if !methods.iter().any(|method| method.as_str() == Some("S256")) {
            return Err(McpError::new("mcp-oauth-pkce-s256-required", issuer));
        }
        Ok((
            resource,
            AuthorizationMetadata {
                issuer: returned_issuer,
                authorization_endpoint: required_url_field(
                    &authorization,
                    "authorization_endpoint",
                )?,
                token_endpoint: required_url_field(&authorization, "token_endpoint")?,
            },
        ))
    }

    fn exchange_token(
        &self,
        metadata: &AuthorizationMetadata,
        fields: Vec<(String, String)>,
    ) -> Result<TokenResponse, McpError> {
        let response = self
            .transport
            .send(StreamingHttpRequest::form(
                metadata.token_endpoint.clone(),
                fields,
                Duration::from_secs(30),
            ))
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        if !(200..300).contains(&response.status) {
            return Err(McpError::new(
                "mcp-oauth-token-status",
                response.status.to_string(),
            ));
        }
        let value = read_json_response(response.body)?;
        serde_json::from_value(value)
            .map_err(|error| McpError::new("mcp-oauth-token-invalid", error.to_string()))
    }

    fn store_tokens(&self, config: &McpOAuthConfig, token: TokenResponse) -> Result<(), McpError> {
        if !token.token_type.eq_ignore_ascii_case("bearer") || token.access_token.is_empty() {
            return Err(McpError::new(
                "mcp-oauth-token-type-invalid",
                token.token_type,
            ));
        }
        if token.access_token.len() > 64 * 1024
            || token
                .refresh_token
                .as_ref()
                .is_some_and(|refresh| refresh.len() > 64 * 1024)
        {
            return Err(McpError::new("mcp-oauth-token-too-large", "64 KiB"));
        }
        if let Some(refresh_token) = token.refresh_token {
            self.credentials
                .put(
                    &CredentialId::new(format!("{}:refresh", config.credential_id)),
                    SecretString::new(refresh_token),
                )
                .map_err(|error| McpError::new(error.code, error.message))?;
        }
        self.credentials
            .put(
                &CredentialId::new(config.credential_id.clone()),
                SecretString::new(token.access_token),
            )
            .map_err(|error| McpError::new(error.code, error.message))?;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    #[serde(rename = "expires_in")]
    _expires_in: Option<u64>,
    #[serde(rename = "scope")]
    _scope: Option<String>,
}

fn fetch_json(
    transport: &Arc<dyn StreamingHttpTransport>,
    endpoint: &str,
    timeout: Duration,
) -> Result<serde_json::Value, McpError> {
    validate_https_or_loopback(endpoint)?;
    let response = transport
        .send(
            StreamingHttpRequest::get(endpoint.to_owned(), timeout)
                .with_header("Accept", "application/json"),
        )
        .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
    if !(200..300).contains(&response.status) {
        return Err(McpError::new(
            "mcp-oauth-metadata-status",
            response.status.to_string(),
        ));
    }
    read_json_response(response.body)
}

fn read_json_response(body: Box<dyn BufRead + Send>) -> Result<serde_json::Value, McpError> {
    let mut bytes = Vec::new();
    body.take((MAX_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| McpError::new("mcp-oauth-http-read", error.to_string()))?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(McpError::new(
            "mcp-oauth-response-too-large",
            bytes.len().to_string(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| McpError::new("mcp-oauth-json-invalid", error.to_string()))
}

fn spawn_callback_listener(
    listener: TcpListener,
    sender: mpsc::SyncSender<Result<OAuthCallback, McpError>>,
) {
    thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        let started = std::time::Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let result = read_callback(stream);
                    let _ = sender.send(result);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= OAUTH_FLOW_TIMEOUT {
                        let _ = sender.send(Err(McpError::new(
                            "mcp-oauth-callback-timeout",
                            "five minutes",
                        )));
                        return;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    let _ = sender.send(Err(McpError::new(
                        "mcp-oauth-callback-accept",
                        error.to_string(),
                    )));
                    return;
                }
            }
        }
    });
}

fn read_callback(mut stream: TcpStream) -> Result<OAuthCallback, McpError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| McpError::new("mcp-oauth-callback-timeout", error.to_string()))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| McpError::new("mcp-oauth-callback-clone", error.to_string()))?,
    );
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|error| McpError::new("mcp-oauth-callback-read", error.to_string()))?;
    if request_line.len() > 8 * 1024 {
        return Err(McpError::new(
            "mcp-oauth-callback-too-large",
            "request line",
        ));
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| McpError::new("mcp-oauth-callback-invalid", "request target"))?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| McpError::new("mcp-oauth-callback-url", error.to_string()))?;
    if url.path() != "/callback" {
        return Err(McpError::new("mcp-oauth-callback-path", url.path()));
    }
    let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
    if let Some(error) = query.get("error") {
        write_callback_response(&mut stream, false);
        return Err(McpError::new("mcp-oauth-authorization-error", error));
    }
    let callback = OAuthCallback {
        code: query
            .get("code")
            .filter(|value| !value.is_empty() && value.len() <= 8 * 1024)
            .cloned()
            .ok_or_else(|| McpError::new("mcp-oauth-code-missing", "callback"))?,
        state: query
            .get("state")
            .filter(|value| value.len() <= 1024)
            .cloned()
            .ok_or_else(|| McpError::new("mcp-oauth-state-missing", "callback"))?,
        issuer: query
            .get("iss")
            .filter(|value| value.len() <= 2048)
            .cloned()
            .ok_or_else(|| McpError::new("mcp-oauth-issuer-missing", "callback"))?,
    };
    write_callback_response(&mut stream, true);
    Ok(callback)
}

fn write_callback_response(stream: &mut TcpStream, success: bool) {
    let body = if success {
        "Authorization received. Return to Harness."
    } else {
        "Authorization failed. Return to Harness."
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn validate_oauth_config(resource_endpoint: &str, config: &McpOAuthConfig) -> Result<(), McpError> {
    validate_https_or_loopback(resource_endpoint)?;
    validate_https_or_loopback(&config.resource_metadata_url)?;
    if config.client_id.trim().is_empty() || config.credential_id.trim().is_empty() {
        return Err(McpError::new(
            "mcp-oauth-config-invalid",
            "clientId/credentialId required",
        ));
    }
    if config
        .scopes
        .iter()
        .any(|scope| scope.trim().is_empty() || scope.contains(char::is_whitespace))
    {
        return Err(McpError::new("mcp-oauth-scope-invalid", "scope"));
    }
    Ok(())
}

fn required_url_field(value: &serde_json::Value, field: &str) -> Result<String, McpError> {
    let url = value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| McpError::new("mcp-oauth-metadata-field-missing", field))?
        .to_owned();
    validate_https_or_loopback(&url)?;
    Ok(url)
}

fn authorization_metadata_url(issuer: &str) -> Result<String, McpError> {
    let issuer = Url::parse(issuer)
        .map_err(|error| McpError::new("mcp-oauth-issuer-url", error.to_string()))?;
    validate_https_or_loopback(issuer.as_str())?;
    let mut metadata = issuer.clone();
    let issuer_path = issuer.path().trim_end_matches('/');
    metadata.set_path(&format!(
        "/.well-known/oauth-authorization-server{issuer_path}"
    ));
    metadata.set_query(None);
    metadata.set_fragment(None);
    Ok(metadata.into())
}

fn normalize_url(value: &str) -> Result<String, McpError> {
    let mut url = Url::parse(value)
        .map_err(|error| McpError::new("mcp-oauth-url-invalid", error.to_string()))?;
    url.set_fragment(None);
    if url.path().ends_with('/') && url.path() != "/" {
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&trimmed);
    }
    Ok(url.into())
}

fn validate_https_or_loopback(value: &str) -> Result<(), McpError> {
    let url = Url::parse(value)
        .map_err(|error| McpError::new("mcp-oauth-url-invalid", error.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(McpError::new("mcp-oauth-url-unsafe", value));
    }
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(McpError::new("mcp-oauth-url-insecure", value));
    }
    Ok(())
}

fn random_base64url(bytes: usize) -> Result<String, McpError> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value)
        .map_err(|error| McpError::new("mcp-oauth-random", error.to_string()))?;
    Ok(base64url(&value))
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = u32::from(chunk[0]);
        let b = u32::from(*chunk.get(1).unwrap_or(&0));
        let c = u32::from(*chunk.get(2).unwrap_or(&0));
        let value = (a << 16) | (b << 8) | c;
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((value >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[(value & 63) as usize] as char);
        }
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{BufReader, Cursor};

    use harness_auth::{CredentialStore, MemoryCredentialStore};
    use harness_http::{HttpBody, HttpTransportError, StreamingHttpResponse};

    use super::*;

    struct MockTransport {
        responses: Mutex<VecDeque<StreamingHttpResponse>>,
        forms: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl StreamingHttpTransport for MockTransport {
        fn send(
            &self,
            request: StreamingHttpRequest,
        ) -> Result<StreamingHttpResponse, HttpTransportError> {
            if let HttpBody::Form(fields) = request.body {
                self.forms.lock().expect("forms").push(fields);
            }
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

    fn json_response(value: serde_json::Value) -> StreamingHttpResponse {
        StreamingHttpResponse {
            status: 200,
            headers: [("content-type".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            body: Box::new(BufReader::new(Cursor::new(value.to_string().into_bytes()))),
        }
    }

    fn oauth_config() -> McpOAuthConfig {
        McpOAuthConfig {
            client_id: "public-client".to_owned(),
            resource_metadata_url: "https://mcp.example.test/.well-known/oauth-protected-resource"
                .to_owned(),
            credential_id: "mcp:test:access".to_owned(),
            scopes: vec!["tools.read".to_owned()],
            callback_port: Some(0),
            expected_issuer: Some("https://auth.example.test/tenant".to_owned()),
        }
    }

    #[test]
    fn oauth_pkce_validates_state_issuer_resource_and_stores_rotatable_tokens() {
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(
                [
                    json_response(serde_json::json!({
                        "resource":"https://mcp.example.test/mcp",
                        "authorization_servers":["https://auth.example.test/tenant"]
                    })),
                    json_response(serde_json::json!({
                        "issuer":"https://auth.example.test/tenant",
                        "authorization_endpoint":"https://auth.example.test/authorize",
                        "token_endpoint":"https://auth.example.test/token",
                        "code_challenge_methods_supported":["S256"]
                    })),
                    json_response(serde_json::json!({
                        "access_token":"access-secret",
                        "token_type":"Bearer",
                        "refresh_token":"refresh-secret",
                        "expires_in":3600
                    })),
                ]
                .into_iter()
                .collect(),
            ),
            forms: Mutex::new(vec![]),
        });
        let credentials = Arc::new(MemoryCredentialStore::new());
        let coordinator = McpOAuthCoordinator::new(credentials.clone(), transport.clone());
        let config = oauth_config();
        let started = coordinator
            .start("test", "https://mcp.example.test/mcp", &config)
            .expect("start");
        let authorization = Url::parse(&started.authorization_url).expect("authorization URL");
        let query = authorization
            .query_pairs()
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("resource").map(String::as_str),
            Some("https://mcp.example.test/mcp")
        );
        assert!(!query["code_challenge"].contains('='));

        let callback = Url::parse_with_params(
            &started.redirect_uri,
            [
                ("code", "authorization-code"),
                ("state", query["state"].as_str()),
                ("iss", "https://auth.example.test/tenant"),
            ],
        )
        .expect("callback");
        let address = format!("127.0.0.1:{}", callback.port().expect("callback port"));
        let mut stream = TcpStream::connect(address).expect("connect callback");
        write!(
            stream,
            "GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            callback.path(),
            callback.query().expect("callback query")
        )
        .expect("callback request");
        stream.flush().expect("callback flush");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("callback response");
        assert!(response.contains("Authorization received"));

        let started_wait = std::time::Instant::now();
        let status = loop {
            match coordinator.finish("test", &config) {
                Ok(status) => break status,
                Err(error)
                    if error.code == "mcp-oauth-callback-pending"
                        && started_wait.elapsed() < Duration::from_secs(2) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("finish: {error:?}"),
            }
        };
        assert!(status.authenticated);
        assert_eq!(
            credentials
                .get(&CredentialId::new("mcp:test:access"))
                .expect("access")
                .expect("access secret")
                .expose_secret()
                .expect("access expose"),
            "access-secret"
        );
        assert_eq!(
            credentials
                .get(&CredentialId::new("mcp:test:access:refresh"))
                .expect("refresh")
                .expect("refresh secret")
                .expose_secret()
                .expect("refresh expose"),
            "refresh-secret"
        );
        let forms = transport.forms.lock().expect("forms");
        let token_form = &forms[0];
        assert!(
            token_form
                .iter()
                .any(|(key, value)| key == "resource" && value == "https://mcp.example.test/mcp")
        );
        assert!(
            token_form
                .iter()
                .any(|(key, value)| key == "code_verifier" && value.len() >= 43)
        );
    }

    #[test]
    fn oauth_rejects_resource_metadata_confusion() {
        let transport = Arc::new(MockTransport {
            responses: Mutex::new(
                [json_response(serde_json::json!({
                    "resource":"https://evil.example/mcp",
                    "authorization_servers":["https://auth.example.test/tenant"]
                }))]
                .into_iter()
                .collect(),
            ),
            forms: Mutex::new(vec![]),
        });
        let coordinator =
            McpOAuthCoordinator::new(Arc::new(MemoryCredentialStore::new()), transport);
        let error = coordinator
            .start("test", "https://mcp.example.test/mcp", &oauth_config())
            .expect_err("resource mismatch");
        assert_eq!(error.code, "mcp-oauth-resource-mismatch");
    }
}
