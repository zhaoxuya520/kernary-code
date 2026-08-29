use std::io::{BufReader, Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use harness_auth::MemoryCredentialStore;
use harness_builtin_tools::{WorkspacePathGuard, WorkspaceSandbox};
use harness_http::{
    HttpBody, HttpMethod, HttpTransportError, StreamingHttpRequest, StreamingHttpResponse,
    StreamingHttpTransport, UreqStreamingTransport,
};
use harness_mcp::{
    McpConnectionStatus, McpManager, McpServerConfig, McpStdioConfig, McpStreamableHttpConfig,
    McpTransportConfig,
};
use harness_permission::{
    ApprovalPolicy, ExecutionEnvelope, GrantScope, InvocationOrigin, PermissionEngine,
    workspace_write_profile,
};
use harness_tool::{
    MemoryToolJournal, ToolInvocationStatus, ToolInvokeRequest, ToolRegistry, ToolRuntime,
};
use harness_types::{
    ActorId, ConfidentialityLabel, InformationFlowLabel, IntegrityLabel, MissionId,
    PermissionGrantId, PermissionRequestId, ProjectId, RunId, ToolInvocationId,
};
use tempfile::tempdir;

fn envelope() -> ExecutionEnvelope {
    ExecutionEnvelope {
        project_id: ProjectId::from("project:mcp"),
        mission_id: MissionId::from("mission:mcp"),
        run_id: Some(RunId::from("run:mcp")),
        actor_id: ActorId::from("agent:mcp"),
        origin: InvocationOrigin::Agent,
        information_flow: InformationFlowLabel {
            integrity: IntegrityLabel::Trusted,
            confidentiality: ConfidentialityLabel::ProjectPrivate,
        },
    }
}

#[test]
fn stdio_is_lazy_catalog_is_searchable_and_tools_use_unified_runtime() {
    let temporary = tempdir().expect("tempdir");
    let marker = temporary.path().join("mcp-started.marker");
    let registry = ToolRegistry::new();
    let manager = McpManager::new(
        temporary.path(),
        Arc::new(MemoryCredentialStore::new()),
        Arc::new(UreqStreamingTransport::default()),
        registry.clone(),
    )
    .expect("manager");
    let added = manager
        .add_server(McpServerConfig {
            id: "fake".to_owned(),
            name: "Fake MCP".to_owned(),
            enabled: true,
            trust_annotations: true,
            transport: McpTransportConfig::Stdio(McpStdioConfig {
                command: PathBuf::from(env!("CARGO_BIN_EXE_harness-mcp-test-server")),
                args: vec![marker.display().to_string()],
                cwd: Some(temporary.path().to_path_buf()),
                inherit_env: vec![],
                request_timeout_millis: Some(5_000),
                max_message_bytes: Some(1024 * 1024),
            }),
        })
        .expect("add metadata");
    assert_eq!(added.status, McpConnectionStatus::Disconnected);
    assert!(!marker.exists(), "metadata discovery must not spawn server");
    let disabled = manager.disable_server("fake").expect("disable");
    assert!(!disabled.enabled);
    assert_eq!(
        manager.connect("fake", false).expect_err("disabled").code,
        "mcp-server-disabled"
    );
    let enabled = manager.enable_server("fake").expect("enable");
    assert!(enabled.enabled);

    let connected = manager.connect("fake", false).expect("connect");
    assert_eq!(connected.status, McpConnectionStatus::Degraded);
    assert_eq!(connected.protocol_version.as_deref(), Some("2025-11-25"));
    assert_eq!(connected.tool_count, 3);
    assert_eq!(connected.supported_tool_count, 2);
    assert_eq!(connected.resource_count, 1);
    assert_eq!(connected.prompt_count, 1);
    assert!(marker.exists());
    let started = std::time::Instant::now();
    let notifications = loop {
        let notifications = manager.poll_notifications("fake").expect("notifications");
        if !notifications.is_empty() {
            break notifications;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(
        notifications[0]["method"],
        "notifications/tools/list_changed"
    );
    assert!(
        registry
            .select_for_prompt("unrelated", 8)
            .expect("search")
            .is_empty()
    );
    assert_eq!(
        registry
            .select_for_prompt("echo text", 8)
            .expect("search")
            .into_iter()
            .map(|tool| tool.canonical_name)
            .collect::<Vec<_>>(),
        vec!["mcp.fake.echo.read"]
    );

    let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
    let mut profile = workspace_write_profile(guard.root().to_path_buf());
    profile.mcp.allowed_server_ids = vec!["fake".to_owned()];
    profile.mcp.allowed_tool_patterns = vec!["*".to_owned()];
    let runtime = ToolRuntime::new(
        registry.clone(),
        PermissionEngine::new(profile, ApprovalPolicy::OnRequest),
        Arc::new(MemoryToolJournal::new()),
        Arc::new(WorkspaceSandbox::new(guard)),
    );
    let read = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("invocation:mcp-read"),
            approval_request_id: PermissionRequestId::from("approval:mcp-read"),
            idempotency_key: "mcp-read".to_owned(),
            envelope: envelope(),
            tool_name: "mcp.fake.echo.read".to_owned(),
            args: serde_json::json!({"text":"你好 MCP"}),
            now_millis: 1,
        })
        .expect("read tool");
    assert_eq!(
        read.invocation.status,
        ToolInvocationStatus::WaitingApproval
    );
    let read = runtime
        .resume_after_approval(
            &read.invocation.id,
            &envelope(),
            GrantScope::Once,
            PermissionGrantId::from("grant:mcp-read"),
            PermissionRequestId::from("approval:mcp-read-resume"),
            2,
        )
        .expect("approve MCP read");
    assert_eq!(read.invocation.status, ToolInvocationStatus::Completed);
    assert_eq!(
        read.invocation.result.expect("result")["structuredContent"]["args"]["text"],
        "你好 MCP"
    );
    let side_effect = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("invocation:mcp-send"),
            approval_request_id: PermissionRequestId::from("approval:mcp-send"),
            idempotency_key: "mcp-send".to_owned(),
            envelope: envelope(),
            tool_name: "mcp.fake.message.send".to_owned(),
            args: serde_json::json!({"text":"must ask"}),
            now_millis: 3,
        })
        .expect("side effect");
    assert_eq!(
        side_effect.invocation.status,
        ToolInvocationStatus::WaitingApproval
    );
    assert_eq!(
        manager
            .read_resource("fake", "memory://guide")
            .expect("resource")[0]["text"],
        "fixture resource"
    );

    let disconnected = manager.disconnect("fake").expect("disconnect");
    assert_eq!(disconnected.status, McpConnectionStatus::Disconnected);
    assert!(registry.list().is_empty());
}

#[test]
fn failed_server_isolated_and_backoff_does_not_break_registry() {
    let temporary = tempdir().expect("tempdir");
    let registry = ToolRegistry::new();
    let manager = McpManager::new(
        temporary.path(),
        Arc::new(MemoryCredentialStore::new()),
        Arc::new(UreqStreamingTransport::default()),
        registry,
    )
    .expect("manager");
    manager
        .add_server(McpServerConfig {
            id: "broken".to_owned(),
            name: "Broken MCP".to_owned(),
            enabled: true,
            trust_annotations: false,
            transport: McpTransportConfig::Stdio(McpStdioConfig {
                command: temporary.path().join("missing-server"),
                args: vec![],
                cwd: None,
                inherit_env: vec![],
                request_timeout_millis: Some(100),
                max_message_bytes: None,
            }),
        })
        .expect("metadata accepts not-yet-present executable");
    let failed = manager
        .connect("broken", false)
        .expect("failure is isolated view");
    assert_eq!(failed.status, McpConnectionStatus::Failed);
    assert!(failed.retry_after_millis.is_some());
    assert!(failed.last_error.is_some());
    assert_eq!(manager.list_servers().expect("list").len(), 1);
}

struct ChannelRead {
    receiver: mpsc::Receiver<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}

impl Read for ChannelRead {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.current.position() >= self.current.get_ref().len() as u64 {
            let bytes = self
                .receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "channel"))?;
            self.current = Cursor::new(bytes);
        }
        self.current.read(buffer)
    }
}

struct LegacyFallbackTransport {
    rejected_streamable: AtomicBool,
    sender: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
}

impl StreamingHttpTransport for LegacyFallbackTransport {
    fn send(
        &self,
        request: StreamingHttpRequest,
    ) -> Result<StreamingHttpResponse, HttpTransportError> {
        if request.method == HttpMethod::Post
            && request.endpoint == "https://legacy.example/mcp"
            && !self.rejected_streamable.swap(true, Ordering::SeqCst)
        {
            return Ok(StreamingHttpResponse {
                status: 404,
                headers: Default::default(),
                body: Box::new(BufReader::new(Cursor::new(Vec::<u8>::new()))),
            });
        }
        if request.method == HttpMethod::Get {
            let (sender, receiver) = mpsc::channel();
            sender
                .send(b"event: endpoint\ndata: /messages\n\n".to_vec())
                .expect("endpoint event");
            *self.sender.lock().expect("sender") = Some(sender);
            return Ok(StreamingHttpResponse {
                status: 200,
                headers: [("content-type".to_owned(), "text/event-stream".to_owned())]
                    .into_iter()
                    .collect(),
                body: Box::new(BufReader::new(ChannelRead {
                    receiver,
                    current: Cursor::new(vec![]),
                })),
            });
        }
        if request.method == HttpMethod::Post
            && request.endpoint == "https://legacy.example/messages"
        {
            if let HttpBody::Json(payload) = request.body
                && let Some(id) = payload.get("id").and_then(serde_json::Value::as_u64)
            {
                let method = payload["method"].as_str().unwrap_or_default();
                let result = if method == "initialize" {
                    serde_json::json!({
                        "protocolVersion":"2024-11-05",
                        "capabilities":{},
                        "serverInfo":{"name":"legacy-fixture","version":"1"}
                    })
                } else {
                    serde_json::json!({})
                };
                let frame = format!(
                    "event: message\ndata: {}\n\n",
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
                );
                self.sender
                    .lock()
                    .expect("sender")
                    .as_ref()
                    .expect("stream sender")
                    .send(frame.into_bytes())
                    .expect("response event");
            }
            return Ok(StreamingHttpResponse {
                status: 202,
                headers: Default::default(),
                body: Box::new(BufReader::new(Cursor::new(Vec::<u8>::new()))),
            });
        }
        Err(HttpTransportError {
            code: "unexpected-request".to_owned(),
            message: request.endpoint,
            timeout: false,
        })
    }
}

#[test]
fn streamable_http_falls_back_to_legacy_sse_only_when_explicitly_enabled() {
    let temporary = tempdir().expect("tempdir");
    let manager = McpManager::new(
        temporary.path(),
        Arc::new(MemoryCredentialStore::new()),
        Arc::new(LegacyFallbackTransport {
            rejected_streamable: AtomicBool::new(false),
            sender: Mutex::new(None),
        }),
        ToolRegistry::new(),
    )
    .expect("manager");
    manager
        .add_server(McpServerConfig {
            id: "legacy".to_owned(),
            name: "Legacy".to_owned(),
            enabled: true,
            trust_annotations: false,
            transport: McpTransportConfig::StreamableHttp(McpStreamableHttpConfig {
                endpoint: "https://legacy.example/mcp".to_owned(),
                bearer_credential_id: None,
                oauth: None,
                legacy_sse_fallback: true,
                request_timeout_millis: Some(2_000),
                max_response_bytes: Some(64 * 1024),
            }),
        })
        .expect("add");
    let view = manager.connect("legacy", false).expect("connect fallback");
    assert_eq!(view.status, McpConnectionStatus::Ready);
    assert_eq!(view.transport, "legacy-http-sse");
    assert_eq!(view.protocol_version.as_deref(), Some("2024-11-05"));
    manager.disconnect("legacy").expect("disconnect");
}
