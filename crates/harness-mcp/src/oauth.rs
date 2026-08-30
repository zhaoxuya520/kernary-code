use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use harness_auth::{CredentialId, CredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, RSA_PKCS1_SHA256, RsaKeyPair},
};
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
    /// HTTPS URL used as the OAuth client ID when the authorization server
    /// advertises Client ID Metadata Document support (SEP-991).
    #[serde(default)]
    pub client_metadata_url: Option<String>,
    #[serde(default)]
    pub client_secret_credential_id: Option<String>,
    /// OS credential-store reference containing a PKCS#8 PEM private key.
    #[serde(default)]
    pub private_key_jwt_credential_id: Option<String>,
    /// JWS algorithm for `private_key_jwt`; currently ES256 and RS256.
    #[serde(default)]
    pub private_key_jwt_signing_algorithm: Option<String>,
    /// Optional JWS `kid` header supplied by the client's out-of-band registration.
    #[serde(default)]
    pub private_key_jwt_key_id: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
    pub resource_metadata_url: String,
    pub credential_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub callback_port: Option<u16>,
    pub expected_issuer: Option<String>,
    /// 2025-03-26 compatibility: root metadata may identify a same-origin
    /// authorization-server issuer with a path component.
    #[serde(default)]
    pub legacy_same_origin_issuer_discovery: bool,
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

/// SEP-990 enterprise cross-app access inputs. Secret values stay in the
/// credential store; this serializable config only carries stable references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCrossAppAccessConfig {
    pub idp_issuer: String,
    #[serde(default)]
    pub idp_token_endpoint: Option<String>,
    pub idp_client_id: String,
    pub idp_id_token_credential_id: String,
}

#[derive(Clone, Debug)]
struct AuthorizationMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    client_id_metadata_document_supported: bool,
    token_endpoint_auth_methods: Vec<String>,
    token_endpoint_auth_signing_algorithms: Vec<String>,
    scopes_supported: Vec<String>,
}

struct RegisteredClient {
    client_id: String,
    client_secret: Option<SecretString>,
    private_key: Option<SecretString>,
    private_key_signing_algorithm: Option<String>,
    private_key_id: Option<String>,
    token_endpoint_auth_method: String,
}

struct PendingOAuth {
    state: String,
    verifier: String,
    redirect_uri: String,
    resource: String,
    metadata: AuthorizationMetadata,
    client: RegisteredClient,
    scopes: Vec<String>,
    receiver: mpsc::Receiver<Result<OAuthCallback, McpError>>,
    expires_at_millis: i64,
}

struct OAuthCallback {
    code: String,
    state: String,
    issuer: Option<String>,
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
        let listener = TcpListener::bind(("127.0.0.1", config.callback_port.unwrap_or(0)))
            .map_err(|error| McpError::new("mcp-oauth-callback-bind", error.to_string()))?;
        let callback_port = listener
            .local_addr()
            .map_err(|error| McpError::new("mcp-oauth-callback-address", error.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");
        let (resource, metadata) = self.discover(resource_endpoint, config)?;
        let client = self.resolve_client(config, &metadata, &redirect_uri)?;
        let scopes = if config.scopes.is_empty() {
            metadata.scopes_supported.clone()
        } else {
            config.scopes.clone()
        };
        let state = random_base64url(32)?;
        let verifier = random_base64url(32)?;
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        let mut authorization_url = Url::parse(&metadata.authorization_endpoint)
            .map_err(|error| McpError::new("mcp-oauth-authorization-url", error.to_string()))?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &client.client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &resource);
            if !scopes.is_empty() {
                query.append_pair("scope", &scopes.join(" "));
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
                client,
                scopes,
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
        if callback
            .issuer
            .as_ref()
            .is_some_and(|issuer| issuer != &pending.metadata.issuer)
        {
            return Err(McpError::new(
                "mcp-oauth-issuer-mismatch",
                callback.issuer.unwrap_or_default(),
            ));
        }
        let mut token_fields = vec![
            ("grant_type".to_owned(), "authorization_code".to_owned()),
            ("code".to_owned(), callback.code),
            ("redirect_uri".to_owned(), pending.redirect_uri),
            ("code_verifier".to_owned(), pending.verifier),
            ("resource".to_owned(), pending.resource),
        ];
        if !pending.scopes.is_empty() {
            token_fields.push(("scope".to_owned(), pending.scopes.join(" ")));
        }
        let token = self.exchange_token(&pending.metadata, token_fields, &pending.client)?;
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
        let client = RegisteredClient {
            client_id: config.client_id.clone(),
            client_secret: None,
            private_key: None,
            private_key_signing_algorithm: None,
            private_key_id: None,
            token_endpoint_auth_method: "none".to_owned(),
        };
        let token = self.exchange_token(
            &metadata,
            vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), refresh),
                ("resource".to_owned(), resource),
            ],
            &client,
        )?;
        self.store_tokens(config, token)?;
        self.status(server_id, Some(config))
    }

    pub fn client_credentials(
        &self,
        server_id: &str,
        resource_endpoint: &str,
        config: &McpOAuthConfig,
    ) -> Result<McpOAuthStatus, McpError> {
        validate_oauth_config(resource_endpoint, config)?;
        if config.client_id.trim().is_empty() {
            return Err(McpError::new(
                "mcp-oauth-client-credentials-client-id-missing",
                server_id,
            ));
        }
        let (resource, metadata) = self.discover(resource_endpoint, config)?;
        let client = self.resolve_client(config, &metadata, "")?;
        let scopes = if config.scopes.is_empty() {
            metadata.scopes_supported.clone()
        } else {
            config.scopes.clone()
        };
        let mut fields = vec![
            ("grant_type".to_owned(), "client_credentials".to_owned()),
            ("resource".to_owned(), resource),
        ];
        if !scopes.is_empty() {
            fields.push(("scope".to_owned(), scopes.join(" ")));
        }
        let token = self.exchange_token(&metadata, fields, &client)?;
        self.store_tokens(config, token)?;
        self.status(server_id, Some(config))
    }

    pub fn cross_app_access(
        &self,
        server_id: &str,
        resource_endpoint: &str,
        config: &McpOAuthConfig,
        cross_app: &McpCrossAppAccessConfig,
    ) -> Result<McpOAuthStatus, McpError> {
        validate_oauth_config(resource_endpoint, config)?;
        validate_cross_app_config(cross_app)?;
        if config.client_id.trim().is_empty() {
            return Err(McpError::new(
                "mcp-oauth-cross-app-client-id-missing",
                server_id,
            ));
        }
        let (resource, authorization_metadata) = self.discover(resource_endpoint, config)?;
        let client = self.resolve_client(config, &authorization_metadata, "")?;
        if !matches!(
            client.token_endpoint_auth_method.as_str(),
            "client_secret_basic" | "client_secret_post"
        ) {
            return Err(McpError::new(
                "mcp-oauth-cross-app-client-auth-unsupported",
                client.token_endpoint_auth_method,
            ));
        }

        let idp_metadata = fetch_json(
            &self.transport,
            &openid_metadata_url(&cross_app.idp_issuer)?,
            Duration::from_secs(15),
        )?;
        let discovered_idp_issuer = required_url_field(&idp_metadata, "issuer")?;
        if normalize_url(&discovered_idp_issuer)? != normalize_url(&cross_app.idp_issuer)? {
            return Err(McpError::new(
                "mcp-oauth-cross-app-idp-issuer-mismatch",
                discovered_idp_issuer,
            ));
        }
        let discovered_token_endpoint = required_url_field(&idp_metadata, "token_endpoint")?;
        let idp_token_endpoint = if let Some(configured) = &cross_app.idp_token_endpoint {
            if normalize_url(configured)? != normalize_url(&discovered_token_endpoint)? {
                return Err(McpError::new(
                    "mcp-oauth-cross-app-idp-token-endpoint-mismatch",
                    configured,
                ));
            }
            configured.clone()
        } else {
            discovered_token_endpoint
        };
        let id_token = self
            .credentials
            .get(&CredentialId::new(
                cross_app.idp_id_token_credential_id.clone(),
            ))
            .map_err(|error| McpError::new(error.code, error.message))?
            .ok_or_else(|| {
                McpError::new(
                    "mcp-oauth-cross-app-id-token-missing",
                    &cross_app.idp_id_token_credential_id,
                )
            })?;
        let id_token = id_token
            .expose_secret()
            .map_err(|error| McpError::new(error.code, error.message))?;
        let idp_response = self
            .transport
            .send(StreamingHttpRequest::form(
                idp_token_endpoint,
                vec![
                    (
                        "grant_type".to_owned(),
                        "urn:ietf:params:oauth:grant-type:token-exchange".to_owned(),
                    ),
                    ("subject_token".to_owned(), id_token.to_owned()),
                    (
                        "subject_token_type".to_owned(),
                        "urn:ietf:params:oauth:token-type:id_token".to_owned(),
                    ),
                    (
                        "requested_token_type".to_owned(),
                        "urn:ietf:params:oauth:token-type:id-jag".to_owned(),
                    ),
                    ("audience".to_owned(), authorization_metadata.issuer.clone()),
                    ("resource".to_owned(), resource.clone()),
                    ("client_id".to_owned(), cross_app.idp_client_id.clone()),
                ],
                Duration::from_secs(30),
            ))
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        if !(200..300).contains(&idp_response.status) {
            return Err(McpError::new(
                "mcp-oauth-cross-app-token-exchange-status",
                idp_response.status.to_string(),
            ));
        }
        let identity_grant: IdentityTokenExchangeResponse =
            serde_json::from_value(read_json_response(idp_response.body)?).map_err(|error| {
                McpError::new("mcp-oauth-cross-app-token-invalid", error.to_string())
            })?;
        if identity_grant.access_token.is_empty()
            || identity_grant.issued_token_type != "urn:ietf:params:oauth:token-type:id-jag"
        {
            return Err(McpError::new(
                "mcp-oauth-cross-app-token-type-invalid",
                identity_grant.issued_token_type,
            ));
        }
        if identity_grant.access_token.len() > 256 * 1024 {
            return Err(McpError::new(
                "mcp-oauth-cross-app-token-too-large",
                identity_grant.access_token.len().to_string(),
            ));
        }
        let identity_grant = SecretString::new(identity_grant.access_token);
        let mut fields = vec![
            (
                "grant_type".to_owned(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".to_owned(),
            ),
            (
                "assertion".to_owned(),
                identity_grant
                    .expose_secret()
                    .map(str::to_owned)
                    .map_err(|error| McpError::new(error.code, error.message))?,
            ),
            ("resource".to_owned(), resource),
        ];
        let scopes = if config.scopes.is_empty() {
            authorization_metadata.scopes_supported.clone()
        } else {
            config.scopes.clone()
        };
        if !scopes.is_empty() {
            fields.push(("scope".to_owned(), scopes.join(" ")));
        }
        let token = self.exchange_token(&authorization_metadata, fields, &client)?;
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
        let (resource, issuer, protected) = if config.resource_metadata_url.is_empty() {
            let endpoint = Url::parse(resource_endpoint)
                .map_err(|error| McpError::new("mcp-oauth-resource-url", error.to_string()))?;
            let issuer = config
                .expected_issuer
                .clone()
                .unwrap_or_else(|| endpoint.origin().ascii_serialization());
            (normalize_url(resource_endpoint)?, issuer, None)
        } else {
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
            if !resource_matches_endpoint(&resource, resource_endpoint)? {
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
            (resource, issuer, Some(protected))
        };
        let metadata_url = authorization_metadata_url(&issuer)?;
        let authorization =
            match fetch_json(&self.transport, &metadata_url, Duration::from_secs(15)) {
                Ok(metadata) => metadata,
                Err(error)
                    if error.code == "mcp-oauth-metadata-status" && error.message == "404" =>
                {
                    match fetch_json(
                        &self.transport,
                        &openid_metadata_url(&issuer)?,
                        Duration::from_secs(15),
                    ) {
                        Ok(metadata) => metadata,
                        Err(error)
                            if error.code == "mcp-oauth-metadata-status"
                                && error.message == "404"
                                && config.resource_metadata_url.is_empty() =>
                        {
                            serde_json::json!({
                                "issuer":issuer,
                                "authorization_endpoint":format!("{issuer}/authorize"),
                                "token_endpoint":format!("{issuer}/token"),
                                "registration_endpoint":format!("{issuer}/register"),
                                "code_challenge_methods_supported":["S256"],
                                "token_endpoint_auth_methods_supported":["none"]
                            })
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
        let returned_issuer = required_url_field(&authorization, "issuer")?;
        let exact_issuer = normalize_url(&returned_issuer)? == normalize_url(&issuer)?;
        let legacy_same_origin_issuer = config.legacy_same_origin_issuer_discovery
            && config.resource_metadata_url.is_empty()
            && urls_have_same_origin(&returned_issuer, &issuer)?;
        if !exact_issuer && !legacy_same_origin_issuer {
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
        let registration_endpoint = authorization
            .get("registration_endpoint")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| {
                        McpError::new(
                            "mcp-oauth-registration-endpoint-invalid",
                            "registration_endpoint",
                        )
                    })
                    .and_then(|endpoint| {
                        validate_https_or_loopback(endpoint)?;
                        Ok(endpoint.to_owned())
                    })
            })
            .transpose()?;
        let client_id_metadata_document_supported = authorization
            .get("client_id_metadata_document_supported")
            .map(|value| {
                value.as_bool().ok_or_else(|| {
                    McpError::new(
                        "mcp-oauth-client-metadata-support-invalid",
                        "client_id_metadata_document_supported",
                    )
                })
            })
            .transpose()?
            .unwrap_or(false);
        let token_endpoint_auth_methods = authorization
            .get("token_endpoint_auth_methods_supported")
            .and_then(serde_json::Value::as_array)
            .map(|methods| {
                methods
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|methods| !methods.is_empty())
            .unwrap_or_else(|| vec!["none".to_owned()]);
        let token_endpoint_auth_signing_algorithms = authorization
            .get("token_endpoint_auth_signing_alg_values_supported")
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| {
                        McpError::new(
                            "mcp-oauth-token-signing-algs-invalid",
                            "token_endpoint_auth_signing_alg_values_supported",
                        )
                    })?
                    .iter()
                    .map(|algorithm| {
                        algorithm
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                McpError::new(
                                    "mcp-oauth-token-signing-alg-invalid",
                                    "token_endpoint_auth_signing_alg_values_supported",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|algorithms| algorithms.into_iter().map(str::to_owned).collect())
            })
            .transpose()?
            .unwrap_or_default();
        let scopes_supported = authorization
            .get("scopes_supported")
            .or_else(|| {
                protected
                    .as_ref()
                    .and_then(|protected| protected.get("scopes_supported"))
            })
            .and_then(serde_json::Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter(|scope| !scope.is_empty() && !scope.contains(char::is_whitespace))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok((
            resource,
            AuthorizationMetadata {
                issuer: returned_issuer,
                authorization_endpoint: required_url_field(
                    &authorization,
                    "authorization_endpoint",
                )?,
                token_endpoint: required_url_field(&authorization, "token_endpoint")?,
                registration_endpoint,
                client_id_metadata_document_supported,
                token_endpoint_auth_methods,
                token_endpoint_auth_signing_algorithms,
                scopes_supported,
            },
        ))
    }

    fn resolve_client(
        &self,
        config: &McpOAuthConfig,
        metadata: &AuthorizationMetadata,
        redirect_uri: &str,
    ) -> Result<RegisteredClient, McpError> {
        if !config.client_id.trim().is_empty() {
            let client_secret = config
                .client_secret_credential_id
                .as_ref()
                .map(|credential_id| {
                    self.credentials
                        .get(&CredentialId::new(credential_id.clone()))
                        .map_err(|error| McpError::new(error.code, error.message))?
                        .ok_or_else(|| {
                            McpError::new("mcp-oauth-client-secret-missing", credential_id)
                        })
                })
                .transpose()?;
            let private_key = config
                .private_key_jwt_credential_id
                .as_ref()
                .map(|credential_id| {
                    self.credentials
                        .get(&CredentialId::new(credential_id.clone()))
                        .map_err(|error| McpError::new(error.code, error.message))?
                        .ok_or_else(|| {
                            McpError::new("mcp-oauth-private-key-missing", credential_id)
                        })
                })
                .transpose()?;
            if client_secret.is_some() && private_key.is_some() {
                return Err(McpError::new(
                    "mcp-oauth-client-credential-conflict",
                    "client secret and private key cannot both be configured",
                ));
            }
            let token_endpoint_auth_method = config
                .token_endpoint_auth_method
                .clone()
                .or_else(|| {
                    private_key
                        .as_ref()
                        .and_then(|_| {
                            metadata
                                .token_endpoint_auth_methods
                                .iter()
                                .any(|method| method == "private_key_jwt")
                                .then(|| "private_key_jwt".to_owned())
                        })
                        .or_else(|| {
                            client_secret.as_ref().and_then(|_| {
                                ["client_secret_basic", "client_secret_post"]
                                    .into_iter()
                                    .find(|method| {
                                        metadata
                                            .token_endpoint_auth_methods
                                            .iter()
                                            .any(|supported| supported == method)
                                    })
                                    .map(str::to_owned)
                            })
                        })
                })
                .unwrap_or_else(|| "none".to_owned());
            if !metadata
                .token_endpoint_auth_methods
                .iter()
                .any(|supported| supported == &token_endpoint_auth_method)
            {
                return Err(McpError::new(
                    "mcp-oauth-token-auth-unsupported",
                    token_endpoint_auth_method,
                ));
            }
            let private_key_signing_algorithm = if token_endpoint_auth_method == "private_key_jwt" {
                if client_secret.is_some() {
                    return Err(McpError::new(
                        "mcp-oauth-client-credential-conflict",
                        "private_key_jwt cannot use a client secret",
                    ));
                }
                if private_key.is_none() {
                    return Err(McpError::new(
                        "mcp-oauth-private-key-missing",
                        "private_key_jwt",
                    ));
                }
                let algorithm = config
                    .private_key_jwt_signing_algorithm
                    .as_deref()
                    .ok_or_else(|| {
                        McpError::new("mcp-oauth-private-key-algorithm-missing", "private_key_jwt")
                    })?;
                if !matches!(algorithm, "ES256" | "RS256") {
                    return Err(McpError::new(
                        "mcp-oauth-private-key-algorithm-unsupported",
                        algorithm,
                    ));
                }
                if !metadata
                    .token_endpoint_auth_signing_algorithms
                    .iter()
                    .any(|supported| supported == algorithm)
                {
                    return Err(McpError::new(
                        "mcp-oauth-private-key-algorithm-not-advertised",
                        algorithm,
                    ));
                }
                Some(algorithm.to_owned())
            } else {
                if private_key.is_some() {
                    return Err(McpError::new(
                        "mcp-oauth-private-key-auth-method-mismatch",
                        token_endpoint_auth_method,
                    ));
                }
                if token_endpoint_auth_method == "none" && client_secret.is_some() {
                    return Err(McpError::new(
                        "mcp-oauth-client-secret-auth-method-mismatch",
                        token_endpoint_auth_method,
                    ));
                }
                if matches!(
                    token_endpoint_auth_method.as_str(),
                    "client_secret_basic" | "client_secret_post"
                ) && client_secret.is_none()
                {
                    return Err(McpError::new(
                        "mcp-oauth-client-secret-missing",
                        token_endpoint_auth_method,
                    ));
                }
                None
            };
            return Ok(RegisteredClient {
                client_id: config.client_id.clone(),
                client_secret,
                private_key,
                private_key_signing_algorithm,
                private_key_id: config.private_key_jwt_key_id.clone(),
                token_endpoint_auth_method,
            });
        }
        if metadata.client_id_metadata_document_supported
            && let Some(client_metadata_url) = config.client_metadata_url.as_deref()
        {
            validate_client_metadata_url(client_metadata_url)?;
            let token_endpoint_auth_method = config
                .token_endpoint_auth_method
                .clone()
                .unwrap_or_else(|| "none".to_owned());
            if token_endpoint_auth_method != "none" {
                return Err(McpError::new(
                    "mcp-oauth-client-metadata-auth-unsupported",
                    token_endpoint_auth_method,
                ));
            }
            if !metadata
                .token_endpoint_auth_methods
                .iter()
                .any(|supported| supported == &token_endpoint_auth_method)
            {
                return Err(McpError::new(
                    "mcp-oauth-token-auth-unsupported",
                    token_endpoint_auth_method,
                ));
            }
            return Ok(RegisteredClient {
                client_id: client_metadata_url.to_owned(),
                client_secret: None,
                private_key: None,
                private_key_signing_algorithm: None,
                private_key_id: None,
                token_endpoint_auth_method,
            });
        }
        let registration_endpoint = metadata.registration_endpoint.as_ref().ok_or_else(|| {
            McpError::new(
                "mcp-oauth-dynamic-registration-unavailable",
                &metadata.issuer,
            )
        })?;
        let preferred_method = ["none", "client_secret_basic", "client_secret_post"]
            .into_iter()
            .find(|method| {
                metadata
                    .token_endpoint_auth_methods
                    .iter()
                    .any(|supported| supported == method)
            })
            .ok_or_else(|| {
                McpError::new(
                    "mcp-oauth-token-auth-unsupported",
                    metadata.token_endpoint_auth_methods.join(","),
                )
            })?;
        let response = self
            .transport
            .send(StreamingHttpRequest::json(
                registration_endpoint.clone(),
                serde_json::json!({
                    "client_name":"Kernary",
                    "redirect_uris":[redirect_uri],
                    "grant_types":["authorization_code","refresh_token"],
                    "response_types":["code"],
                    "token_endpoint_auth_method":preferred_method
                }),
                Duration::from_secs(30),
            ))
            .map_err(|error| McpError::new(error.code, error.message).retryable(error.timeout))?;
        if !matches!(response.status, 200 | 201) {
            return Err(McpError::new(
                "mcp-oauth-registration-status",
                response.status.to_string(),
            ));
        }
        let value = read_json_response(response.body)?;
        let client_id = value
            .get("client_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| McpError::new("mcp-oauth-registration-client-id-missing", "client_id"))?
            .to_owned();
        let client_secret = value
            .get("client_secret")
            .and_then(serde_json::Value::as_str)
            .map(SecretString::new);
        let token_endpoint_auth_method = value
            .get("token_endpoint_auth_method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(preferred_method)
            .to_owned();
        if !metadata
            .token_endpoint_auth_methods
            .iter()
            .any(|supported| supported == &token_endpoint_auth_method)
        {
            return Err(McpError::new(
                "mcp-oauth-registration-auth-method-mismatch",
                token_endpoint_auth_method,
            ));
        }
        Ok(RegisteredClient {
            client_id,
            client_secret,
            private_key: None,
            private_key_signing_algorithm: None,
            private_key_id: None,
            token_endpoint_auth_method,
        })
    }

    fn exchange_token(
        &self,
        metadata: &AuthorizationMetadata,
        mut fields: Vec<(String, String)>,
        client: &RegisteredClient,
    ) -> Result<TokenResponse, McpError> {
        let mut request = StreamingHttpRequest::form(
            metadata.token_endpoint.clone(),
            fields.clone(),
            Duration::from_secs(30),
        );
        match client.token_endpoint_auth_method.as_str() {
            "none" => fields.push(("client_id".to_owned(), client.client_id.clone())),
            "client_secret_post" => {
                fields.push(("client_id".to_owned(), client.client_id.clone()));
                let secret = client.client_secret.as_ref().ok_or_else(|| {
                    McpError::new("mcp-oauth-client-secret-missing", "client_secret_post")
                })?;
                fields.push((
                    "client_secret".to_owned(),
                    secret
                        .expose_secret()
                        .map(str::to_owned)
                        .map_err(|error| McpError::new(error.code, error.message))?,
                ));
            }
            "client_secret_basic" => {
                let secret = client.client_secret.as_ref().ok_or_else(|| {
                    McpError::new("mcp-oauth-client-secret-missing", "client_secret_basic")
                })?;
                let secret = secret
                    .expose_secret()
                    .map_err(|error| McpError::new(error.code, error.message))?;
                let encoded = base64_standard(format!("{}:{secret}", client.client_id).as_bytes());
                request = request.with_sensitive_header(
                    "Authorization",
                    SecretString::new(format!("Basic {encoded}")),
                );
            }
            "private_key_jwt" => {
                let private_key = client.private_key.as_ref().ok_or_else(|| {
                    McpError::new("mcp-oauth-private-key-missing", "private_key_jwt")
                })?;
                let private_key = private_key
                    .expose_secret()
                    .map_err(|error| McpError::new(error.code, error.message))?;
                let algorithm =
                    client
                        .private_key_signing_algorithm
                        .as_deref()
                        .ok_or_else(|| {
                            McpError::new(
                                "mcp-oauth-private-key-algorithm-missing",
                                "private_key_jwt",
                            )
                        })?;
                let assertion = sign_client_assertion(
                    &client.client_id,
                    &metadata.issuer,
                    private_key,
                    algorithm,
                    client.private_key_id.as_deref(),
                )?;
                fields.push((
                    "client_assertion_type".to_owned(),
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_owned(),
                ));
                fields.push(("client_assertion".to_owned(), assertion));
            }
            method => return Err(McpError::new("mcp-oauth-token-auth-unsupported", method)),
        }
        request.body = harness_http::HttpBody::Form(fields);
        let response = self
            .transport
            .send(request)
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

#[derive(Deserialize)]
struct IdentityTokenExchangeResponse {
    access_token: String,
    issued_token_type: String,
    #[serde(rename = "token_type")]
    _token_type: Option<String>,
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
            .cloned(),
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
    if config.resource_metadata_url.is_empty() {
        let issuer = config.expected_issuer.as_deref().ok_or_else(|| {
            McpError::new(
                "mcp-oauth-legacy-issuer-required",
                "expectedIssuer is required when PRM is unavailable",
            )
        })?;
        validate_https_or_loopback(issuer)?;
    } else {
        if config.legacy_same_origin_issuer_discovery {
            return Err(McpError::new(
                "mcp-oauth-legacy-issuer-mode-invalid",
                "legacy issuer discovery requires PRM to be unavailable",
            ));
        }
        validate_https_or_loopback(&config.resource_metadata_url)?;
    }
    if config.credential_id.trim().is_empty() {
        return Err(McpError::new(
            "mcp-oauth-config-invalid",
            "credentialId required",
        ));
    }
    if let Some(client_metadata_url) = config.client_metadata_url.as_deref() {
        validate_client_metadata_url(client_metadata_url)?;
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

fn validate_cross_app_config(config: &McpCrossAppAccessConfig) -> Result<(), McpError> {
    validate_https_or_loopback(&config.idp_issuer)?;
    let issuer = Url::parse(&config.idp_issuer)
        .map_err(|error| McpError::new("mcp-oauth-cross-app-idp-issuer", error.to_string()))?;
    if issuer.query().is_some() {
        return Err(McpError::new(
            "mcp-oauth-cross-app-idp-issuer-query",
            &config.idp_issuer,
        ));
    }
    if let Some(token_endpoint) = config.idp_token_endpoint.as_deref() {
        validate_https_or_loopback(token_endpoint)?;
    }
    if config.idp_client_id.trim().is_empty() || config.idp_client_id.len() > 8 * 1024 {
        return Err(McpError::new(
            "mcp-oauth-cross-app-idp-client-id-invalid",
            "idpClientId",
        ));
    }
    if config.idp_id_token_credential_id.trim().is_empty() {
        return Err(McpError::new(
            "mcp-oauth-cross-app-id-token-credential-invalid",
            "idpIdTokenCredentialId",
        ));
    }
    Ok(())
}

fn validate_client_metadata_url(value: &str) -> Result<(), McpError> {
    let url = Url::parse(value).map_err(|error| {
        McpError::new("mcp-oauth-client-metadata-url-invalid", error.to_string())
    })?;
    if url.scheme() != "https" {
        return Err(McpError::new(
            "mcp-oauth-client-metadata-url-insecure",
            value,
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(McpError::new("mcp-oauth-client-metadata-url-unsafe", value));
    }
    if matches!(url.path(), "" | "/") {
        return Err(McpError::new(
            "mcp-oauth-client-metadata-path-required",
            value,
        ));
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

fn openid_metadata_url(issuer: &str) -> Result<String, McpError> {
    let issuer = Url::parse(issuer)
        .map_err(|error| McpError::new("mcp-oauth-issuer-url", error.to_string()))?;
    validate_https_or_loopback(issuer.as_str())?;
    let mut metadata = issuer.clone();
    let issuer_path = issuer.path().trim_end_matches('/');
    metadata.set_path(&format!("{issuer_path}/.well-known/openid-configuration"));
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

fn resource_matches_endpoint(resource: &str, endpoint: &str) -> Result<bool, McpError> {
    if normalize_url(resource)? == normalize_url(endpoint)? {
        return Ok(true);
    }
    let resource = Url::parse(resource)
        .map_err(|error| McpError::new("mcp-oauth-resource-url", error.to_string()))?;
    let endpoint = Url::parse(endpoint)
        .map_err(|error| McpError::new("mcp-oauth-resource-url", error.to_string()))?;
    Ok(resource.origin() == endpoint.origin()
        && matches!(resource.path(), "" | "/")
        && resource.query().is_none())
}

fn urls_have_same_origin(left: &str, right: &str) -> Result<bool, McpError> {
    let left = Url::parse(left)
        .map_err(|error| McpError::new("mcp-oauth-url-invalid", error.to_string()))?;
    let right = Url::parse(right)
        .map_err(|error| McpError::new("mcp-oauth-url-invalid", error.to_string()))?;
    Ok(left.origin() == right.origin())
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

fn base64_standard(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = u32::from(chunk[0]);
        let b = u32::from(*chunk.get(1).unwrap_or(&0));
        let c = u32::from(*chunk.get(2).unwrap_or(&0));
        let value = (a << 16) | (b << 8) | c;
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn sign_client_assertion(
    client_id: &str,
    audience: &str,
    private_key_pem: &str,
    algorithm: &str,
    key_id: Option<&str>,
) -> Result<String, McpError> {
    if client_id.trim().is_empty() {
        return Err(McpError::new(
            "mcp-oauth-client-assertion-client-id-missing",
            "private_key_jwt",
        ));
    }
    let mut header = serde_json::Map::from_iter([
        (
            "alg".to_owned(),
            serde_json::Value::String(algorithm.to_owned()),
        ),
        (
            "typ".to_owned(),
            serde_json::Value::String("JWT".to_owned()),
        ),
    ]);
    if let Some(key_id) = key_id {
        if key_id.is_empty() || key_id.len() > 1024 {
            return Err(McpError::new("mcp-oauth-private-key-id-invalid", "kid"));
        }
        header.insert(
            "kid".to_owned(),
            serde_json::Value::String(key_id.to_owned()),
        );
    }
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let expires_at = issued_at.saturating_add(300);
    let claims = serde_json::json!({
        "iss":client_id,
        "sub":client_id,
        "aud":audience,
        "iat":issued_at,
        "exp":expires_at,
        "jti":random_base64url(18)?
    });
    let header = serde_json::to_vec(&header)
        .map_err(|error| McpError::new("mcp-oauth-client-assertion-json", error.to_string()))?;
    let claims = serde_json::to_vec(&claims)
        .map_err(|error| McpError::new("mcp-oauth-client-assertion-json", error.to_string()))?;
    let signing_input = format!("{}.{}", base64url(&header), base64url(&claims));
    let mut private_key_der = decode_pkcs8_pem(private_key_pem)?;
    let signature = (|| match algorithm {
        "ES256" => {
            let random = SystemRandom::new();
            let key_pair = EcdsaKeyPair::from_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &private_key_der,
                &random,
            )
            .map_err(|_| McpError::new("mcp-oauth-private-key-invalid", "ES256 PKCS#8"))?;
            key_pair
                .sign(&random, signing_input.as_bytes())
                .map(|signature| signature.as_ref().to_vec())
                .map_err(|_| McpError::new("mcp-oauth-client-assertion-sign", "ES256"))
        }
        "RS256" => {
            let random = SystemRandom::new();
            let key_pair = RsaKeyPair::from_pkcs8(&private_key_der)
                .map_err(|_| McpError::new("mcp-oauth-private-key-invalid", "RS256 PKCS#8"))?;
            let mut signature = vec![0_u8; key_pair.public().modulus_len()];
            key_pair
                .sign(
                    &RSA_PKCS1_SHA256,
                    &random,
                    signing_input.as_bytes(),
                    &mut signature,
                )
                .map_err(|_| McpError::new("mcp-oauth-client-assertion-sign", "RS256"))?;
            Ok(signature)
        }
        other => Err(McpError::new(
            "mcp-oauth-private-key-algorithm-unsupported",
            other,
        )),
    })();
    private_key_der.fill(0);
    let signature = signature?;
    Ok(format!("{signing_input}.{}", base64url(&signature)))
}

fn decode_pkcs8_pem(private_key_pem: &str) -> Result<Vec<u8>, McpError> {
    const MAX_PRIVATE_KEY_PEM_BYTES: usize = 256 * 1024;
    if private_key_pem.len() > MAX_PRIVATE_KEY_PEM_BYTES {
        return Err(McpError::new(
            "mcp-oauth-private-key-too-large",
            MAX_PRIVATE_KEY_PEM_BYTES.to_string(),
        ));
    }
    let encoded = private_key_pem
        .trim()
        .strip_prefix("-----BEGIN PRIVATE KEY-----")
        .and_then(|value| value.strip_suffix("-----END PRIVATE KEY-----"))
        .ok_or_else(|| {
            McpError::new(
                "mcp-oauth-private-key-format",
                "PKCS#8 PEM PRIVATE KEY required",
            )
        })?
        .lines()
        .map(str::trim)
        .collect::<String>();
    if encoded.is_empty() {
        return Err(McpError::new(
            "mcp-oauth-private-key-format",
            "empty PKCS#8 PEM",
        ));
    }
    STANDARD
        .decode(encoded)
        .map_err(|_| McpError::new("mcp-oauth-private-key-format", "invalid PEM base64"))
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

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use harness_auth::{CredentialStore, MemoryCredentialStore};
    use harness_http::{HttpBody, HttpTransportError, StreamingHttpResponse};
    use ring::signature::{ECDSA_P256_SHA256_FIXED, KeyPair, UnparsedPublicKey};

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
            client_metadata_url: None,
            client_secret_credential_id: None,
            private_key_jwt_credential_id: None,
            private_key_jwt_signing_algorithm: None,
            private_key_jwt_key_id: None,
            token_endpoint_auth_method: None,
            resource_metadata_url: "https://mcp.example.test/.well-known/oauth-protected-resource"
                .to_owned(),
            credential_id: "mcp:test:access".to_owned(),
            scopes: vec!["tools.read".to_owned()],
            callback_port: Some(0),
            expected_issuer: Some("https://auth.example.test/tenant".to_owned()),
            legacy_same_origin_issuer_discovery: false,
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
    fn oauth_prefers_client_id_metadata_document_over_dynamic_registration() {
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
                        "registration_endpoint":"https://auth.example.test/register",
                        "code_challenge_methods_supported":["S256"],
                        "token_endpoint_auth_methods_supported":["none"],
                        "client_id_metadata_document_supported":true
                    })),
                ]
                .into_iter()
                .collect(),
            ),
            forms: Mutex::new(vec![]),
        });
        let coordinator =
            McpOAuthCoordinator::new(Arc::new(MemoryCredentialStore::new()), transport.clone());
        let mut config = oauth_config();
        config.client_id.clear();
        config.client_metadata_url =
            Some("https://kernary.dev/oauth/client-metadata.json".to_owned());
        let started = coordinator
            .start("cimd", "https://mcp.example.test/mcp", &config)
            .expect("start CIMD flow without DCR request");
        let authorization = Url::parse(&started.authorization_url).expect("authorization URL");
        let query = authorization
            .query_pairs()
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some("https://kernary.dev/oauth/client-metadata.json")
        );
        assert!(
            transport.responses.lock().expect("responses").is_empty(),
            "CIMD must not consume a dynamic-registration response"
        );
    }

    #[test]
    fn oauth_rejects_insecure_or_root_client_metadata_urls() {
        let mut config = oauth_config();
        config.client_metadata_url = Some("http://kernary.dev/client.json".to_owned());
        let error = validate_oauth_config("https://mcp.example.test/mcp", &config)
            .expect_err("HTTP CIMD URL must be rejected");
        assert_eq!(error.code, "mcp-oauth-client-metadata-url-insecure");

        config.client_metadata_url = Some("https://kernary.dev/".to_owned());
        let error = validate_oauth_config("https://mcp.example.test/mcp", &config)
            .expect_err("root CIMD URL must be rejected");
        assert_eq!(error.code, "mcp-oauth-client-metadata-path-required");
    }

    #[test]
    fn private_key_jwt_es256_has_verified_signature_and_required_claims() {
        let random = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &random)
            .expect("generate ES256 key");
        let key_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &random)
                .expect("parse ES256 key");
        let encoded = STANDARD.encode(pkcs8.as_ref());
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{encoded}\n-----END PRIVATE KEY-----\n");
        let jwt = sign_client_assertion(
            "client-123",
            "https://auth.example.test",
            &pem,
            "ES256",
            Some("key-1"),
        )
        .expect("sign assertion");
        let parts = jwt.split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).expect("decode signature");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key_pair.public_key().as_ref())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("verify ES256 signature");

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).expect("decode header"))
                .expect("header JSON");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "key-1");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("decode claims"))
                .expect("claims JSON");
        assert_eq!(claims["iss"], "client-123");
        assert_eq!(claims["sub"], "client-123");
        assert_eq!(claims["aud"], "https://auth.example.test");
        assert!(claims["exp"].as_u64() > claims["iat"].as_u64());
        assert!(
            claims["jti"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[test]
    fn cross_app_access_exchanges_id_token_then_identity_grant() {
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
                        "code_challenge_methods_supported":["S256"],
                        "token_endpoint_auth_methods_supported":["client_secret_basic"]
                    })),
                    json_response(serde_json::json!({
                        "issuer":"https://idp.example.test",
                        "token_endpoint":"https://idp.example.test/token"
                    })),
                    json_response(serde_json::json!({
                        "access_token":"signed-id-jag",
                        "issued_token_type":"urn:ietf:params:oauth:token-type:id-jag",
                        "token_type":"N_A"
                    })),
                    json_response(serde_json::json!({
                        "access_token":"mcp-access-token",
                        "token_type":"Bearer"
                    })),
                ]
                .into_iter()
                .collect(),
            ),
            forms: Mutex::new(vec![]),
        });
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials
            .put(
                &CredentialId::new("mcp:test:client-secret"),
                SecretString::new("client-secret"),
            )
            .expect("client secret");
        credentials
            .put(
                &CredentialId::new("mcp:test:id-token"),
                SecretString::new("signed-id-token"),
            )
            .expect("ID token");
        let coordinator = McpOAuthCoordinator::new(credentials.clone(), transport.clone());
        let mut config = oauth_config();
        config.client_secret_credential_id = Some("mcp:test:client-secret".to_owned());
        config.token_endpoint_auth_method = Some("client_secret_basic".to_owned());
        let status = coordinator
            .cross_app_access(
                "cross-app",
                "https://mcp.example.test/mcp",
                &config,
                &McpCrossAppAccessConfig {
                    idp_issuer: "https://idp.example.test".to_owned(),
                    idp_token_endpoint: Some("https://idp.example.test/token".to_owned()),
                    idp_client_id: "idp-client".to_owned(),
                    idp_id_token_credential_id: "mcp:test:id-token".to_owned(),
                },
            )
            .expect("cross-app access");
        assert!(status.authenticated);
        let forms = transport.forms.lock().expect("forms");
        assert_eq!(forms.len(), 2);
        assert!(forms[0].iter().any(|(key, value)| {
            key == "grant_type" && value == "urn:ietf:params:oauth:grant-type:token-exchange"
        }));
        assert!(forms[0].iter().any(|(key, value)| {
            key == "requested_token_type" && value == "urn:ietf:params:oauth:token-type:id-jag"
        }));
        assert!(forms[1].iter().any(|(key, value)| {
            key == "grant_type" && value == "urn:ietf:params:oauth:grant-type:jwt-bearer"
        }));
        assert!(
            forms[1]
                .iter()
                .any(|(key, value)| key == "assertion" && value == "signed-id-jag")
        );
        drop(forms);
        assert_eq!(
            credentials
                .get(&CredentialId::new("mcp:test:access"))
                .expect("access read")
                .expect("access token")
                .expose_secret()
                .expect("access UTF-8"),
            "mcp-access-token"
        );
    }

    #[test]
    fn legacy_metadata_accepts_only_explicit_same_origin_issuer_discovery() {
        let response = || {
            json_response(serde_json::json!({
                "issuer":"https://mcp.example.test/oauth",
                "authorization_endpoint":"https://mcp.example.test/oauth/authorize",
                "token_endpoint":"https://mcp.example.test/oauth/token",
                "code_challenge_methods_supported":["S256"],
                "token_endpoint_auth_methods_supported":["none"]
            }))
        };
        let mut config = oauth_config();
        config.resource_metadata_url.clear();
        config.expected_issuer = Some("https://mcp.example.test".to_owned());
        config.legacy_same_origin_issuer_discovery = true;
        let coordinator = McpOAuthCoordinator::new(
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(MockTransport {
                responses: Mutex::new([response()].into_iter().collect()),
                forms: Mutex::new(vec![]),
            }),
        );
        let (_, metadata) = coordinator
            .discover("https://mcp.example.test/mcp", &config)
            .expect("same-origin legacy issuer");
        assert_eq!(metadata.issuer, "https://mcp.example.test/oauth");

        let coordinator = McpOAuthCoordinator::new(
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(MockTransport {
                responses: Mutex::new(
                    [json_response(serde_json::json!({
                        "issuer":"https://evil.example/oauth",
                        "authorization_endpoint":"https://evil.example/oauth/authorize",
                        "token_endpoint":"https://evil.example/oauth/token",
                        "code_challenge_methods_supported":["S256"],
                        "token_endpoint_auth_methods_supported":["none"]
                    }))]
                    .into_iter()
                    .collect(),
                ),
                forms: Mutex::new(vec![]),
            }),
        );
        let error = coordinator
            .discover("https://mcp.example.test/mcp", &config)
            .expect_err("cross-origin legacy issuer must be rejected");
        assert_eq!(error.code, "mcp-oauth-metadata-issuer-mismatch");
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
