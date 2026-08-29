use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use harness_browser::*;
use harness_permission::{
    ApprovalPolicy, ExecutionEnvelope, InvocationOrigin, PermissionAction, PermissionEngine,
    workspace_write_profile,
};
use harness_tool::{
    MemoryToolJournal, SandboxPort, ToolDescriptor, ToolError, ToolExecutionInput,
    ToolInvocationStatus, ToolInvokeRequest, ToolProvider, ToolRegistry, ToolRuntime, ToolSource,
};
use harness_types::{
    ActorId, BrowserActionId, BrowserSessionId, ConfidentialityLabel, ContentHash,
    InformationFlowLabel, IntegrityLabel, MissionId, PermissionRequestId, ProjectId, RunId,
    ToolInvocationId,
};
use tempfile::tempdir;

struct FakeAdapter {
    alive: AtomicBool,
    commands: Mutex<Vec<BrowserCommand>>,
    artifact_path: PathBuf,
}

impl BrowserAdapter for FakeAdapter {
    fn launch(&self, _config: &BrowserSessionConfig) -> Result<(), BrowserError> {
        self.alive.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn execute(
        &self,
        _config: &BrowserSessionConfig,
        command: &BrowserCommand,
    ) -> Result<BrowserResult, BrowserError> {
        self.commands
            .lock()
            .expect("commands")
            .push(command.clone());
        Ok(match command {
            BrowserCommand::Navigate { .. }
            | BrowserCommand::Click { .. }
            | BrowserCommand::Type { .. }
            | BrowserCommand::Wait { .. }
            | BrowserCommand::Upload { .. } => BrowserResult::Unit,
            BrowserCommand::Snapshot => BrowserResult::Snapshot {
                snapshot: BrowserSnapshot {
                    url: "http://127.0.0.1:4173/".to_owned(),
                    title: "Harness".to_owned(),
                    generation: 1,
                    nodes: vec![BrowserSnapshotNode {
                        ref_id: Some("e1".to_owned()),
                        role: "button".to_owned(),
                        name: "Apply".to_owned(),
                        description: None,
                        sensitive: false,
                    }],
                },
            },
            BrowserCommand::Read { .. } => BrowserResult::Text {
                text: "value".to_owned(),
            },
            BrowserCommand::Inspect { ref_id } => BrowserResult::Inspect {
                result: BrowserInspectResult {
                    ref_id: ref_id.clone(),
                    role: "button".to_owned(),
                    name: "Apply".to_owned(),
                    tag: Some("button".to_owned()),
                    attributes: serde_json::json!({"type":"button"}),
                    bounds: Some([0.0, 0.0, 10.0, 10.0]),
                },
            },
            BrowserCommand::Screenshot | BrowserCommand::Download { .. } => {
                BrowserResult::Artifact {
                    artifact: BrowserArtifactRef {
                        id: "artifact:1".to_owned(),
                        path: self.artifact_path.clone(),
                        mime_type: "image/png".to_owned(),
                        bytes: 10,
                        sha256: ContentHash::from("hash"),
                    },
                }
            }
        })
    }

    fn close(&self, _config: &BrowserSessionConfig) -> Result<(), BrowserError> {
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn handoff(&self, _config: &BrowserSessionConfig) -> Result<(), BrowserError> {
        self.alive.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

fn runtime(
    root: &std::path::Path,
) -> (
    Arc<BrowserRuntime>,
    Arc<FakeAdapter>,
    Arc<SqliteBrowserJournal>,
) {
    let artifacts = root.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts");
    let adapter = Arc::new(FakeAdapter {
        alive: AtomicBool::new(false),
        commands: Mutex::new(vec![]),
        artifact_path: artifacts.join("shot.png"),
    });
    let journal =
        Arc::new(SqliteBrowserJournal::open(root.join("browser.sqlite")).expect("journal"));
    let runtime = Arc::new(
        BrowserRuntime::new(
            BrowserSessionConfig {
                id: BrowserSessionId::from("browser:test"),
                browser_executable: root.join("browser.exe"),
                profile_directory: root.join("profile"),
                artifact_directory: artifacts,
                download_directory: root.join("downloads"),
                headless: true,
                allowed_origins: ["http://127.0.0.1:4173".to_owned()].into_iter().collect(),
                upload_roots: vec![root.to_path_buf()],
                allow_uploads: false,
                allow_downloads: true,
                timeout_millis: 10_000,
            },
            adapter.clone(),
            journal.clone(),
        )
        .expect("runtime"),
    );
    (runtime, adapter, journal)
}

#[test]
fn runtime_enforces_origin_secret_and_transfer_policy_and_journals_failures() {
    let temporary = tempdir().expect("tempdir");
    let (runtime, adapter, journal) = runtime(temporary.path());
    runtime.open(1).expect("open");
    runtime.open(1).expect("idempotent open");
    runtime
        .execute(
            BrowserActionId::from("action:navigate"),
            BrowserCommand::Navigate {
                url: "http://127.0.0.1:4173/page?token=TOP-TOKEN-123".to_owned(),
            },
            2,
        )
        .expect("navigate");
    let snapshot = runtime
        .execute(
            BrowserActionId::from("action:snapshot"),
            BrowserCommand::Snapshot,
            3,
        )
        .expect("snapshot");
    assert!(matches!(snapshot, BrowserResult::Snapshot { .. }));
    assert_eq!(runtime.view().expect("view").snapshot_generation, 1);
    assert_eq!(
        runtime
            .execute(
                BrowserActionId::from("action:origin-deny"),
                BrowserCommand::Navigate {
                    url: "https://example.com/".to_owned(),
                },
                4,
            )
            .expect_err("origin deny")
            .code,
        "browser-origin-not-allowed"
    );
    assert_eq!(
        runtime
            .execute(
                BrowserActionId::from("action:secret-deny"),
                BrowserCommand::Type {
                    ref_id: "e1".to_owned(),
                    text: "TOP-SECRET-123".to_owned(),
                    classification: ConfidentialityLabel::UserSecret,
                },
                5,
            )
            .expect_err("secret deny")
            .code,
        "browser-secret-input-requires-user-handoff"
    );
    assert_eq!(
        runtime
            .execute(
                BrowserActionId::from("action:upload-deny"),
                BrowserCommand::Upload {
                    ref_id: "e1".to_owned(),
                    path: temporary.path().join("file.txt"),
                },
                6,
            )
            .expect_err("upload deny")
            .code,
        "browser-upload-disabled"
    );
    assert_eq!(
        runtime
            .execute(
                BrowserActionId::from("action:credential-deny"),
                BrowserCommand::Navigate {
                    url: "http://user:password@127.0.0.1:4173/".to_owned(),
                },
                7,
            )
            .expect_err("credential deny")
            .code,
        "browser-url-credentials-denied"
    );
    let handoff = runtime
        .handoff(BrowserActionId::from("action:handoff"), 8)
        .expect("handoff");
    assert!(handoff.adapter_alive);
    assert_eq!(handoff.status, BrowserSessionStatus::UserControl);
    assert_eq!(
        runtime
            .execute(
                BrowserActionId::from("action:user-control-deny"),
                BrowserCommand::Snapshot,
                9,
            )
            .expect_err("Agent must stop while the user controls the browser")
            .code,
        "browser-session-not-ready"
    );
    let reclaimed = runtime
        .reclaim(BrowserActionId::from("action:reclaim"), 10)
        .expect("reclaim");
    assert_eq!(reclaimed.status, BrowserSessionStatus::Ready);
    let actions = journal
        .list(&BrowserSessionId::from("browser:test"))
        .expect("actions");
    assert_eq!(actions.len(), 9);
    assert_eq!(actions[0].sequence, 1);
    assert_eq!(actions[4].status, BrowserActionStatus::Failed);
    assert_eq!(actions[6].action, BrowserActionKind::Handoff);
    assert_eq!(actions[7].status, BrowserActionStatus::Failed);
    assert_eq!(actions[8].action, BrowserActionKind::Reclaim);
    assert_eq!(
        actions[0].target.as_deref(),
        Some("http://127.0.0.1:4173/page")
    );
    let connection =
        rusqlite::Connection::open(temporary.path().join("browser.sqlite")).expect("journal db");
    let records = connection
        .prepare("SELECT record_json FROM browser_actions")
        .expect("prepare")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("records")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    assert!(
        records
            .iter()
            .all(|record| !record.contains("TOP-SECRET-123") && !record.contains("TOP-TOKEN-123"))
    );
    assert_eq!(adapter.commands.lock().expect("commands").len(), 2);
    runtime.close(11).expect("close");
    runtime.close(12).expect("idempotent close");
}

struct BrowserSandbox;
impl SandboxPort for BrowserSandbox {
    fn execute(
        &self,
        descriptor: &ToolDescriptor,
        action: &PermissionAction,
        provider: &dyn ToolProvider,
        input: ToolExecutionInput,
    ) -> Result<serde_json::Value, ToolError> {
        if descriptor.source != ToolSource::Internal
            || !descriptor.canonical_name.starts_with("browser.")
            || !matches!(
                action,
                PermissionAction::BrowserOpen { .. }
                    | PermissionAction::BrowserSnapshot { .. }
                    | PermissionAction::BrowserAct { .. }
                    | PermissionAction::BrowserUpload { .. }
                    | PermissionAction::BrowserDownload { .. }
            )
        {
            return Err(ToolError::new(
                "browser-sandbox-denied",
                descriptor.canonical_name.clone(),
            ));
        }
        provider.execute(input)
    }
}

#[test]
fn catalog_exposes_only_structured_browser_tools_through_permission_and_journal() {
    let temporary = tempdir().expect("tempdir");
    let (browser, _adapter, journal) = runtime(temporary.path());
    browser.open(1).expect("open");
    browser
        .execute(
            BrowserActionId::from("action:navigate"),
            BrowserCommand::Navigate {
                url: "http://127.0.0.1:4173/".to_owned(),
            },
            2,
        )
        .expect("navigate");
    let mut registry = ToolRegistry::new();
    register_browser_tools(&mut registry, browser).expect("register");
    assert_eq!(registry.list().len(), 10);
    assert!(registry.list().iter().all(|tool| {
        tool.canonical_name.starts_with("browser.")
            && !tool.canonical_name.contains("cdp")
            && tool.prompt_loading == harness_tool::ToolPromptLoading::OnDemand
    }));
    let runtime = ToolRuntime::new(
        registry,
        PermissionEngine::new(
            workspace_write_profile(temporary.path().to_path_buf()),
            ApprovalPolicy::NeverWithinSandbox,
        ),
        Arc::new(MemoryToolJournal::new()),
        Arc::new(BrowserSandbox),
    );
    let response = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("tool:browser-snapshot"),
            approval_request_id: PermissionRequestId::from("approval:browser"),
            idempotency_key: "browser:snapshot:1".to_owned(),
            envelope: ExecutionEnvelope {
                project_id: ProjectId::from("project:test"),
                mission_id: MissionId::from("mission:test"),
                run_id: Some(RunId::from("run:test")),
                actor_id: ActorId::from("agent:browser"),
                origin: InvocationOrigin::Agent,
                information_flow: InformationFlowLabel {
                    integrity: IntegrityLabel::Trusted,
                    confidentiality: ConfidentialityLabel::ProjectPrivate,
                },
            },
            tool_name: "browser.snapshot".to_owned(),
            args: serde_json::json!({}),
            now_millis: 3,
        })
        .expect("invoke");
    assert_eq!(response.invocation.status, ToolInvocationStatus::Completed);
    assert_eq!(
        journal
            .list(&BrowserSessionId::from("browser:test"))
            .expect("journal")
            .len(),
        2
    );
}

#[test]
fn interrupted_action_becomes_uncertain_and_session_needs_reconciliation() {
    let temporary = tempdir().expect("tempdir");
    let journal =
        SqliteBrowserJournal::open(temporary.path().join("browser.sqlite")).expect("journal");
    journal
        .upsert_session(
            &BrowserSessionId::from("browser:test"),
            BrowserSessionStatus::Ready,
            1,
        )
        .expect("session");
    journal
        .begin(BrowserActionRecord {
            id: BrowserActionId::from("action:running"),
            session_id: BrowserSessionId::from("browser:test"),
            sequence: 0,
            action: BrowserActionKind::Click,
            status: BrowserActionStatus::Running,
            origin: Some("http://127.0.0.1:4173".to_owned()),
            target: Some("e1".to_owned()),
            arguments_sha256: ContentHash::from("hash"),
            result_summary: None,
            error: None,
            started_at_millis: 1,
            completed_at_millis: None,
        })
        .expect("begin");
    assert_eq!(journal.recover_interrupted(2).expect("recover"), 1);
    let actions = journal
        .list(&BrowserSessionId::from("browser:test"))
        .expect("actions");
    assert_eq!(actions[0].status, BrowserActionStatus::Uncertain);
}
