use std::error::Error;
use std::sync::Arc;
use std::thread;

use harness_auth::MemoryCredentialStore;
use harness_http::UreqStreamingTransport;
use harness_mcp::{
    LATEST_STABLE_PROTOCOL_VERSION, McpClient, McpStreamableHttpConfig, StreamableHttpMcpTransport,
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
    let transport = StreamableHttpMcpTransport::new(
        McpStreamableHttpConfig {
            endpoint,
            bearer_credential_id: None,
            oauth: None,
            legacy_sse_fallback: false,
            request_timeout_millis: Some(30_000),
            max_response_bytes: Some(4 * 1024 * 1024),
        },
        Arc::new(MemoryCredentialStore::new()),
        Arc::new(UreqStreamingTransport::default()),
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
