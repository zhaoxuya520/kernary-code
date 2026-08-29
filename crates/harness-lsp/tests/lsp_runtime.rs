use std::path::PathBuf;

use harness_lsp::LspManager;
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
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn process_lsp_bridge_is_lazy_then_normalizes_read_only_language_facts() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("project");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(
        root.join("src/main.rs"),
        "a😀中z\nfn main() {\n    value();\n    main();\n}\n",
    )
    .expect("source");
    let marker = temporary.path().join("started.marker");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_harness-lsp-test-server"));
    let normalized = |path: &std::path::Path| path.display().to_string().replace('\\', "/");
    let config = temporary.path().join("kernary.lsp.toml");
    std::fs::write(
        &config,
        format!(
            "schema_version = 1\n[[servers]]\nid = \"fixture\"\ncommand = \"{}\"\nargs = [\"{}\"]\nlanguage_ids = {{ rs = \"rust\" }}\nrequest_timeout_millis = 2000\nmax_message_bytes = 1048576\n",
            normalized(&executable),
            normalized(&marker)
        ),
    )
    .expect("config");
    let manager = LspManager::load(&config, &root).expect("manager");
    assert!(!marker.exists(), "metadata load must not spawn server");
    assert_eq!(manager.list().expect("list")[0].status, "sleeping");

    let symbols = manager
        .document_symbols("fixture", PathBuf::from("src/main.rs").as_path())
        .expect("symbols");
    assert!(marker.is_file());
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0].name, "main");
    assert_eq!(symbols[1].container_name.as_deref(), Some("main"));

    let definitions = manager
        .definition("fixture", PathBuf::from("src/main.rs").as_path(), 1, 3)
        .expect("definition");
    assert_eq!(definitions[0].path.as_deref(), Some("src/main.rs"));
    assert_eq!(definitions[0].range.start.character, 3);
    assert_eq!(
        definitions[0]
            .human_range
            .expect("human range")
            .start
            .character,
        3
    );
    let references = manager
        .references("fixture", PathBuf::from("src/main.rs").as_path(), 1, 3)
        .expect("references");
    assert_eq!(references.len(), 2);
    let diagnostics = manager
        .diagnostics("fixture", PathBuf::from("src/main.rs").as_path())
        .expect("diagnostics");
    assert_eq!(diagnostics[0].code.as_deref(), Some("fixture-warning"));
    let before = std::fs::read_to_string(root.join("src/main.rs")).expect("before preview");
    let rename = manager
        .rename_edit(
            "fixture",
            PathBuf::from("src/main.rs").as_path(),
            1,
            2,
            "renamed",
        )
        .expect("rename preview");
    assert_eq!(rename.files.len(), 1);
    assert!(rename.files[0].after_text.starts_with("arenamed中z"));
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).expect("preview is read-only"),
        before
    );
    let range = harness_lsp::HumanRange {
        start: harness_lsp::HumanPosition {
            line: 1,
            character: 1,
        },
        end: harness_lsp::HumanPosition {
            line: 1,
            character: 5,
        },
    };
    let direct = manager
        .code_action_edit(
            "fixture",
            PathBuf::from("src/main.rs").as_path(),
            range,
            0,
            Some("quickfix"),
        )
        .expect("direct action");
    assert!(direct.files[0].after_text.starts_with("afixed中z"));
    let resolved = manager
        .code_action_edit(
            "fixture",
            PathBuf::from("src/main.rs").as_path(),
            range,
            1,
            Some("refactor.rewrite"),
        )
        .expect("resolved action");
    assert!(resolved.files[0].after_text.starts_with("aresolved中z"));
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).expect("still read-only"),
        before
    );
    assert!(manager.stop("fixture").expect("stop"));
    assert_eq!(manager.list().expect("list")[0].status, "sleeping");
}

#[test]
fn lsp_tools_are_on_demand_approved_journaled_and_reuse_exact_project_grant() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("project");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("source");
    let marker = temporary.path().join("tool-started.marker");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_harness-lsp-test-server"));
    let manager = LspManager::new(
        &root,
        vec![harness_lsp::LspServerConfig {
            id: "fixture".to_owned(),
            command: executable.clone(),
            args: vec![marker.display().to_string()],
            cwd: None,
            language_ids: [("rs".to_owned(), "rust".to_owned())].into_iter().collect(),
            inherit_env: vec![],
            initialization_options: None,
            request_timeout_millis: Some(2_000),
            max_message_bytes: Some(1024 * 1024),
        }],
    )
    .expect("manager");
    let registry = ToolRegistry::new();
    harness_lsp::register_lsp_tools(&registry, manager.clone()).expect("register tools");
    assert!(
        registry
            .select_for_prompt("find exact symbol definition", 2)
            .expect("search")
            .iter()
            .any(|tool| tool.canonical_name.starts_with("lsp."))
    );
    assert!(!marker.exists(), "Tool discovery must remain metadata-only");

    let mut profile = workspace_write_profile(root.clone());
    profile.subprocess.allowed_executables = vec![executable.clone()];
    let journal = Arc::new(MemoryToolJournal::new());
    let runtime = ToolRuntime::new(
        registry.clone(),
        PermissionEngine::new(profile.clone(), ApprovalPolicy::OnRequest),
        journal.clone(),
        Arc::new(
            harness_builtin_tools::WorkspaceSandbox::with_processes(
                harness_builtin_tools::WorkspacePathGuard::new(&root).expect("guard"),
                vec![executable.clone()],
            )
            .expect("sandbox"),
        ),
    );
    let envelope = ExecutionEnvelope {
        project_id: ProjectId::from("project:test"),
        mission_id: MissionId::from("mission:test"),
        run_id: Some(RunId::from("run:test")),
        actor_id: ActorId::from("agent:researcher"),
        origin: InvocationOrigin::Agent,
        information_flow: InformationFlowLabel {
            integrity: IntegrityLabel::Trusted,
            confidentiality: ConfidentialityLabel::ProjectPrivate,
        },
    };
    let waiting = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("tool:lsp-symbols"),
            approval_request_id: PermissionRequestId::from("approval:lsp"),
            idempotency_key: "lsp-symbols-once".to_owned(),
            envelope: envelope.clone(),
            tool_name: "lsp.symbols".to_owned(),
            args: serde_json::json!({"serverId":"fixture","path":"src/main.rs"}),
            now_millis: 1,
        })
        .expect("invoke");
    assert_eq!(
        waiting.invocation.status,
        ToolInvocationStatus::WaitingApproval
    );
    assert!(!marker.exists(), "Approval must precede process spawn");

    drop(runtime);
    let runtime = ToolRuntime::new(
        registry,
        PermissionEngine::new(profile, ApprovalPolicy::OnRequest),
        journal,
        Arc::new(
            harness_builtin_tools::WorkspaceSandbox::with_processes(
                harness_builtin_tools::WorkspacePathGuard::new(&root).expect("guard"),
                vec![executable],
            )
            .expect("sandbox"),
        ),
    );
    assert_eq!(runtime.rehydrate_pending_approvals().expect("rehydrate"), 1);

    let completed = runtime
        .resume_after_approval(
            &waiting.invocation.id,
            &envelope,
            GrantScope::Project,
            PermissionGrantId::from("grant:lsp-project"),
            PermissionRequestId::from("approval:next"),
            2,
        )
        .expect("resume");
    assert_eq!(completed.invocation.status, ToolInvocationStatus::Completed);
    assert!(marker.is_file());
    let completed_result = completed.invocation.result.as_ref().expect("result");
    assert_eq!(
        completed_result["fileHash"].as_str().expect("hash").len(),
        64
    );
    assert_eq!(completed_result["positionEncoding"], "utf-16");
    assert_eq!(
        completed_result["facts"].as_array().expect("facts").len(),
        2
    );

    let reused = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("tool:lsp-definition"),
            approval_request_id: PermissionRequestId::from("approval:unused"),
            idempotency_key: "lsp-definition-after-grant".to_owned(),
            envelope,
            tool_name: "lsp.definition".to_owned(),
            args: serde_json::json!({
                "serverId":"fixture",
                "path":"src/main.rs",
                "line":1,
                "character":4
            }),
            now_millis: 3,
        })
        .expect("reuse grant");
    assert_eq!(reused.invocation.status, ToolInvocationStatus::Completed);
    assert!(!reused.needs_approval);
    assert_eq!(runtime.journal().list().expect("journal").len(), 2);
    manager.stop("fixture").expect("stop");
}
