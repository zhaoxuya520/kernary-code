use std::error::Error;
use std::io::Read;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use harness_auth::{CredentialId, CredentialStore, MemoryCredentialStore, SecretString};
use harness_http::{StreamingHttpRequest, StreamingHttpTransport, UreqStreamingTransport};
use harness_mcp::{
    LATEST_STABLE_PROTOCOL_VERSION, McpClient, McpOAuthConfig, McpOAuthCoordinator,
    McpStreamableHttpConfig, StreamableHttpMcpTransport,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("kernary-mcp-conformance-error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let endpoint = std::env::args()
        .nth(1)
        .ok_or("conformance server URL is required")?;
    let scenario = std::env::var("MCP_CONFORMANCE_SCENARIO")
        .unwrap_or_else(|_| "initialize".to_owned())
        .replace('_', "-");
    let protocol_version = std::env::var("MCP_CONFORMANCE_PROTOCOL_VERSION")
        .unwrap_or_else(|_| LATEST_STABLE_PROTOCOL_VERSION.to_owned());
    let credentials = Arc::new(MemoryCredentialStore::new());
    let http = Arc::new(UreqStreamingTransport::default());
    if scenario.starts_with("auth/") {
        return run_oauth_conformance(&endpoint, &scenario, &protocol_version, credentials, http);
    }
    let transport = StreamableHttpMcpTransport::new(
        McpStreamableHttpConfig {
            endpoint,
            bearer_credential_id: None,
            oauth: None,
            legacy_sse_fallback: false,
            request_timeout_millis: Some(30_000),
            max_response_bytes: Some(4 * 1024 * 1024),
        },
        credentials,
        http,
    )?;
    let client_capabilities = if scenario == "elicitation-sep1034-client-defaults" {
        serde_json::json!({"elicitation":{"form":{"applyDefaults":true}}})
    } else {
        serde_json::json!({})
    };
    let client = McpClient::initialize_with_version_and_capabilities(
        transport.clone(),
        &protocol_version,
        client_capabilities,
    )?;
    match scenario.as_str() {
        "initialize" => {}
        "tools-call" => {
            let tools = client.list_tools()?;
            if !tools.iter().any(|tool| tool.name == "add_numbers") {
                return Err("conformance add_numbers tool missing".into());
            }
            let result = client.call_tool("add_numbers", serde_json::json!({"a":17,"b":25}))?;
            if result.is_error {
                return Err("conformance add_numbers returned isError".into());
            }
        }
        "sse-retry" => {
            let tools = client.list_tools()?;
            if !tools.iter().any(|tool| tool.name == "test_reconnection") {
                return Err("conformance test_reconnection tool missing".into());
            }
            let result = client.call_tool("test_reconnection", serde_json::json!({}))?;
            if result.is_error {
                return Err("conformance reconnection tool returned isError".into());
            }
        }
        "elicitation-sep1034-client-defaults" => {
            let tools = client.list_tools()?;
            let tool_name = "test_client_elicitation_defaults";
            if !tools.iter().any(|tool| tool.name == tool_name) {
                return Err("conformance elicitation tool missing".into());
            }
            let mut inbound = transport.open_inbound_stream()?;
            let response_transport = transport.clone();
            let responder = thread::spawn(move || -> Result<(), String> {
                let message = inbound.next_message().map_err(|error| error.to_string())?;
                if message.get("method").and_then(serde_json::Value::as_str)
                    != Some("elicitation/create")
                {
                    return Err(format!("unexpected inbound MCP method: {message}"));
                }
                let id = message
                    .get("id")
                    .cloned()
                    .ok_or_else(|| "elicitation request missing id".to_owned())?;
                let properties = message
                    .pointer("/params/requestedSchema/properties")
                    .and_then(serde_json::Value::as_object)
                    .ok_or_else(|| "elicitation schema properties missing".to_owned())?;
                let content = properties
                    .iter()
                    .filter_map(|(name, schema)| {
                        schema
                            .get("default")
                            .cloned()
                            .map(|value| (name.clone(), value))
                    })
                    .collect::<serde_json::Map<_, _>>();
                response_transport
                    .send_jsonrpc_result(
                        id,
                        serde_json::json!({"action":"accept","content":content}),
                    )
                    .map_err(|error| error.to_string())
            });
            let result = client.call_tool(tool_name, serde_json::json!({}))?;
            responder
                .join()
                .map_err(|_| "elicitation responder panicked")?
                .map_err(|error| format!("elicitation responder failed: {error}"))?;
            if result.is_error {
                return Err("conformance elicitation tool returned isError".into());
            }
        }
        other => return Err(format!("unsupported conformance scenario: {other}").into()),
    }
    client.close()?;
    Ok(())
}

fn run_oauth_conformance(
    endpoint: &str,
    scenario: &str,
    protocol_version: &str,
    credentials: Arc<MemoryCredentialStore>,
    http: Arc<UreqStreamingTransport>,
) -> Result<(), Box<dyn Error>> {
    let unauthenticated = StreamableHttpMcpTransport::new(
        McpStreamableHttpConfig {
            endpoint: endpoint.to_owned(),
            bearer_credential_id: None,
            oauth: None,
            legacy_sse_fallback: false,
            request_timeout_millis: Some(30_000),
            max_response_bytes: Some(4 * 1024 * 1024),
        },
        credentials.clone(),
        http.clone(),
    )?;
    let error = McpClient::initialize_with_version(unauthenticated.clone(), protocol_version)
        .err()
        .ok_or("OAuth endpoint unexpectedly accepted anonymous initialize")?;
    if error.code != "mcp-http-authorization-required" {
        return Err(error.into());
    }
    let challenge = unauthenticated.authorization_challenge()?;
    let challenge_scopes = challenge
        .as_deref()
        .and_then(|value| challenge_parameter(value, "scope"))
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let context = std::env::var("MCP_CONFORMANCE_CONTEXT")
        .ok()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let configured_client_id = context
        .get("client_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let client_secret_credential_id = context
        .get("client_secret")
        .and_then(serde_json::Value::as_str)
        .map(|secret| {
            let credential_id = "mcp:conformance:client-secret".to_owned();
            credentials
                .put(
                    &CredentialId::new(credential_id.clone()),
                    SecretString::new(secret),
                )
                .map_err(|error| format!("{}: {}", error.code, error.message))?;
            Ok::<String, String>(credential_id)
        })
        .transpose()?;
    let candidates = resource_metadata_candidates(endpoint, challenge.as_deref())?;
    let coordinator = McpOAuthCoordinator::new(credentials.clone(), http.clone());
    if scenario == "auth/client-credentials-basic" {
        for resource_metadata_url in &candidates {
            let config = McpOAuthConfig {
                client_id: configured_client_id.clone(),
                client_secret_credential_id: client_secret_credential_id.clone(),
                token_endpoint_auth_method: Some("client_secret_basic".to_owned()),
                resource_metadata_url: resource_metadata_url.clone(),
                credential_id: "mcp:conformance:access".to_owned(),
                scopes: challenge_scopes.clone(),
                callback_port: None,
                expected_issuer: None,
            };
            match coordinator.client_credentials("conformance", endpoint, &config) {
                Ok(status) if status.authenticated => {
                    return verify_authenticated(
                        endpoint,
                        protocol_version,
                        &config.credential_id,
                        credentials,
                        http,
                    );
                }
                Ok(_) => return Err("client credentials did not authenticate".into()),
                Err(error) if error.code == "mcp-oauth-metadata-status" => {}
                Err(error) => return Err(error.into()),
            }
        }
        return Err("client credentials metadata discovery failed".into());
    }
    let mut selected = None;
    for resource_metadata_url in candidates {
        let config = McpOAuthConfig {
            client_id: configured_client_id.clone(),
            client_secret_credential_id: client_secret_credential_id.clone(),
            token_endpoint_auth_method: None,
            resource_metadata_url,
            credential_id: "mcp:conformance:access".to_owned(),
            scopes: challenge_scopes.clone(),
            callback_port: None,
            expected_issuer: None,
        };
        match coordinator.start("conformance", endpoint, &config) {
            Ok(start) => {
                selected = Some((config, start));
                break;
            }
            Err(error) if error.code == "mcp-oauth-metadata-status" => {}
            Err(error)
                if scenario == "auth/resource-mismatch"
                    && error.code == "mcp-oauth-resource-mismatch" =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    if selected.is_none() && scenario.starts_with("auth/2025-03-26-") {
        let endpoint_url = url::Url::parse(endpoint)?;
        let config = McpOAuthConfig {
            client_id: configured_client_id,
            client_secret_credential_id,
            token_endpoint_auth_method: None,
            resource_metadata_url: String::new(),
            credential_id: "mcp:conformance:access".to_owned(),
            scopes: challenge_scopes,
            callback_port: None,
            expected_issuer: Some(endpoint_url.origin().ascii_serialization()),
        };
        let start = coordinator.start("conformance", endpoint, &config)?;
        selected = Some((config, start));
    }
    let (config, start) = selected.ok_or("OAuth metadata discovery failed")?;
    let mut authorization = http
        .send(StreamingHttpRequest::get(
            start.authorization_url,
            Duration::from_secs(30),
        ))
        .map_err(|error| format!("{}: {}", error.code, error.message))?;
    let mut callback_body = Vec::new();
    authorization.body.read_to_end(&mut callback_body)?;
    if !(200..400).contains(&authorization.status) {
        return Err(format!("authorization navigation HTTP {}", authorization.status).into());
    }
    let wait_started = Instant::now();
    loop {
        match coordinator.finish("conformance", &config) {
            Ok(status) if status.authenticated => break,
            Ok(_) => return Err("OAuth finish did not authenticate".into()),
            Err(error)
                if error.code == "mcp-oauth-callback-pending"
                    && wait_started.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    verify_authenticated(
        endpoint,
        protocol_version,
        &config.credential_id,
        credentials,
        http,
    )
}

fn verify_authenticated(
    endpoint: &str,
    protocol_version: &str,
    credential_id: &str,
    credentials: Arc<MemoryCredentialStore>,
    http: Arc<UreqStreamingTransport>,
) -> Result<(), Box<dyn Error>> {
    let authenticated = StreamableHttpMcpTransport::new(
        McpStreamableHttpConfig {
            endpoint: endpoint.to_owned(),
            bearer_credential_id: Some(credential_id.to_owned()),
            oauth: None,
            legacy_sse_fallback: false,
            request_timeout_millis: Some(30_000),
            max_response_bytes: Some(4 * 1024 * 1024),
        },
        credentials,
        http,
    )?;
    let client = McpClient::initialize_with_version(authenticated, protocol_version)?;
    let _ = client.list_tools()?;
    client.close()?;
    Ok(())
}

fn resource_metadata_candidates(
    endpoint: &str,
    challenge: Option<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut candidates = Vec::new();
    if let Some(challenge) = challenge
        && let Some(metadata) = challenge_parameter(challenge, "resource_metadata")
    {
        candidates.push(metadata);
    }
    let endpoint = url::Url::parse(endpoint)?;
    let origin = endpoint.origin().ascii_serialization();
    let path = endpoint.path().trim_end_matches('/');
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    let mut seen = std::collections::BTreeSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    Ok(candidates)
}

fn challenge_parameter(challenge: &str, name: &str) -> Option<String> {
    challenge.split(',').find_map(|part| {
        let part = part.trim();
        let value = part.strip_prefix(&format!("{name}="))?;
        Some(value.trim_matches('"').to_owned())
    })
}
