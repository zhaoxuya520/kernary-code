use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_builtin_tools::{WorkspacePathGuard, WorkspaceSandbox};
use harness_permission::{
    ApprovalPolicy, ExecutionEnvelope, InvocationOrigin, PermissionEngine, workspace_write_profile,
};
use harness_plugin::{
    PluginLifecycleStatus, PluginManager, ServiceCardinality, ServiceDefinition, ServiceRegistry,
    compose_plugin_settings,
};
use harness_tool::{
    MemoryToolJournal, ToolInvocationStatus, ToolInvokeRequest, ToolRegistry, ToolRuntime,
};
use harness_types::{
    ActorId, ConfidentialityLabel, InformationFlowLabel, IntegrityLabel, MissionId,
    PermissionRequestId, ProjectId, RunId, ToolInvocationId,
};
use tempfile::tempdir;

fn copy_fixture(root: &Path, id: &str) -> PathBuf {
    let plugin = root.join(id);
    fs::create_dir_all(&plugin).expect("plugin dir");
    let source = PathBuf::from(env!("CARGO_BIN_EXE_harness-plugin-test-host"));
    let file_name = source.file_name().expect("fixture file name");
    let entry = plugin.join(file_name);
    fs::copy(source, &entry).expect("copy fixture");
    fs::write(
        plugin.join("input-schema.json"),
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
    )
    .expect("input schema");
    fs::write(plugin.join("output-schema.json"), r#"{"type":"string"}"#).expect("output schema");
    fs::create_dir_all(plugin.join("demo-skill")).expect("skill dir");
    fs::write(plugin.join("demo-skill/SKILL.md"), "# Demo skill\n").expect("skill");
    fs::write(
        plugin.join("demo-skill/skill.toml"),
        r#"id = "plugin_demo_skill"
name = "Plugin Demo Skill"
version = "1.0.0"
description = "plugin contributed skill"
entry = "SKILL.md"
"#,
    )
    .expect("skill manifest");
    fs::write(plugin.join("mcp.toml"), "servers = []\n").expect("mcp config");
    let manifest = format!(
        r#"id = "{id}"
name = "Demo Plugin"
version = "1.0.0"
description = "isolated fixture plugin"
engineRange = ">=0.1.0"
entry = "{}"
permissions = ["plugin.call"]
activationTimeoutMillis = 5000
toolTimeoutMillis = 5000
maxOutputBytes = 1048576

[contributions]
skills = ["demo-skill/skill.toml"]
mcpServers = ["mcp.toml"]
contextProviders = ["demo-context"]

[[contributions.tools]]
name = "uppercase"
version = "1"
description = "uppercase text"
effectClass = "read-only-retryable"
sideEffect = false
inputSchema = "input-schema.json"
outputSchema = "output-schema.json"
keywords = ["uppercase", "text"]
"#,
        entry.file_name().expect("entry").to_string_lossy()
    );
    fs::write(plugin.join("plugin.toml"), manifest).expect("manifest");
    plugin
}

fn envelope() -> ExecutionEnvelope {
    ExecutionEnvelope {
        project_id: ProjectId::from("project:plugin"),
        mission_id: MissionId::from("mission:plugin"),
        run_id: Some(RunId::from("run:plugin")),
        actor_id: ActorId::from("agent:plugin"),
        origin: InvocationOrigin::Agent,
        information_flow: InformationFlowLabel {
            integrity: IntegrityLabel::Trusted,
            confidentiality: ConfidentialityLabel::ProjectPrivate,
        },
    }
}

#[test]
fn discovery_is_metadata_only_review_is_exact_and_tool_uses_unified_runtime() {
    let temporary = tempdir().expect("tempdir");
    let plugin_root = copy_fixture(temporary.path(), "demo_plugin");
    let marker = temporary.path().join("activated.marker");
    let registry = ToolRegistry::new();
    let manager = PluginManager::new("0.1.0", registry.clone()).expect("manager");
    let discovered = manager
        .discover(&[temporary.path().to_path_buf()])
        .expect("discover");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].status, PluginLifecycleStatus::Disabled);
    assert!(
        !marker.exists(),
        "metadata discovery must not execute entry"
    );
    let review = manager.review("demo_plugin").expect("review");
    assert_eq!(review.tool_names, vec!["plugin.demo_plugin.uppercase"]);
    assert!(
        manager
            .enable(
                "demo_plugin",
                "project:test",
                "wrong-hash",
                serde_json::json!({"marker":marker})
            )
            .is_err()
    );
    assert!(!marker.exists());
    let active = manager
        .enable(
            "demo_plugin",
            "project:test",
            &review.manifest_hash,
            serde_json::json!({"marker":marker}),
        )
        .expect("enable");
    assert_eq!(active.status, PluginLifecycleStatus::Active);
    assert_eq!(active.contribution_count, 1);
    assert!(marker.exists());
    assert_eq!(
        registry
            .select_for_prompt("make this uppercase", 8)
            .expect("search")[0]
            .canonical_name,
        "plugin.demo_plugin.uppercase"
    );
    let guard = WorkspacePathGuard::new(temporary.path()).expect("guard");
    let mut profile = workspace_write_profile(guard.root().to_path_buf());
    profile.plugin.allowed_plugin_ids = vec!["demo_plugin".to_owned()];
    profile.plugin.allowed_capability_patterns = vec!["uppercase".to_owned()];
    let runtime = ToolRuntime::new(
        registry.clone(),
        PermissionEngine::new(profile, ApprovalPolicy::OnRequest),
        Arc::new(MemoryToolJournal::new()),
        Arc::new(WorkspaceSandbox::new(guard)),
    );
    let result = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("invocation:plugin"),
            approval_request_id: PermissionRequestId::from("approval:plugin"),
            idempotency_key: "plugin-uppercase".to_owned(),
            envelope: envelope(),
            tool_name: "plugin.demo_plugin.uppercase".to_owned(),
            args: serde_json::json!({"text":"harness"}),
            now_millis: 1,
        })
        .expect("invoke");
    assert_eq!(result.invocation.status, ToolInvocationStatus::Completed);
    assert_eq!(result.invocation.result.expect("result"), "HARNESS");
    let (skills, mcp) = manager
        .contribution_paths("demo_plugin")
        .expect("contributions");
    assert_eq!(
        skills,
        vec![fs::canonicalize(plugin_root.join("demo-skill/skill.toml")).expect("skill path")]
    );
    assert_eq!(
        mcp,
        vec![fs::canonicalize(plugin_root.join("mcp.toml")).expect("mcp path")]
    );
    let disabled = manager.disable("demo_plugin").expect("disable");
    assert_eq!(disabled.status, PluginLifecycleStatus::Disabled);
    assert!(registry.list().is_empty());
}

#[test]
fn activation_failure_is_isolated_and_does_not_register_contributions() {
    let temporary = tempdir().expect("tempdir");
    copy_fixture(temporary.path(), "broken_plugin");
    let registry = ToolRegistry::new();
    let manager = PluginManager::new("0.1.0", registry.clone()).expect("manager");
    manager
        .discover(&[temporary.path().to_path_buf()])
        .expect("discover");
    let review = manager.review("broken_plugin").expect("review");
    let failed = manager
        .enable(
            "broken_plugin",
            "project:test",
            &review.manifest_hash,
            serde_json::json!({"crashActivate":true}),
        )
        .expect("failure becomes view");
    assert_eq!(failed.status, PluginLifecycleStatus::Failed);
    assert!(failed.last_error.is_some());
    assert!(registry.list().is_empty());
}

#[test]
fn review_hash_replays_across_restart_and_rejects_changed_contribution() {
    let temporary = tempdir().expect("tempdir");
    let plugin_root = copy_fixture(temporary.path(), "replay_plugin");
    let first = PluginManager::new("0.1.0", ToolRegistry::new()).expect("first manager");
    first
        .discover(&[temporary.path().to_path_buf()])
        .expect("discover first");
    let review = first.review("replay_plugin").expect("review");
    drop(first);

    let replay_registry = ToolRegistry::new();
    let replay = PluginManager::new("0.1.0", replay_registry).expect("replay manager");
    replay
        .discover(&[temporary.path().to_path_buf()])
        .expect("discover replay");
    let active = replay
        .enable(
            "replay_plugin",
            "project:test",
            &review.manifest_hash,
            serde_json::json!({}),
        )
        .expect("replayed review hash");
    assert_eq!(active.status, PluginLifecycleStatus::Active);
    replay.disable("replay_plugin").expect("disable");

    fs::write(
        plugin_root.join("input-schema.json"),
        r#"{"type":"object","properties":{"changed":{"type":"boolean"}}}"#,
    )
    .expect("change contribution");
    let changed = PluginManager::new("0.1.0", ToolRegistry::new()).expect("changed manager");
    changed
        .discover(&[temporary.path().to_path_buf()])
        .expect("discover changed");
    let error = changed
        .enable(
            "replay_plugin",
            "project:test",
            &review.manifest_hash,
            serde_json::json!({}),
        )
        .expect_err("changed file invalidates review");
    assert_eq!(error.code, "plugin-review-hash-mismatch");
}

#[test]
fn service_registry_protects_core_and_settings_merge_replaces_arrays() {
    let mut services = ServiceRegistry::default();
    services
        .define(ServiceDefinition {
            id: "kernel-store".to_owned(),
            version: "1".to_owned(),
            cardinality: ServiceCardinality::One,
            core_owned: true,
        })
        .expect("define core");
    assert!(
        services
            .provide("evil", "project", "kernel-store", serde_json::json!({}))
            .is_err()
    );
    services
        .define(ServiceDefinition {
            id: "context".to_owned(),
            version: "1".to_owned(),
            cardinality: ServiceCardinality::One,
            core_owned: false,
        })
        .expect("define");
    services
        .provide("a", "project", "context", serde_json::json!("a"))
        .expect("provide");
    assert!(
        services
            .provide("b", "project", "context", serde_json::json!("b"))
            .is_err()
    );
    assert_eq!(
        compose_plugin_settings(&[
            serde_json::json!({"enabled":true,"nested":{"a":1,"list":[1,2]}}),
            serde_json::json!({"nested":{"b":2}}),
            serde_json::json!({"nested":{"list":[3]}}),
        ]),
        serde_json::json!({"enabled":true,"nested":{"a":1,"b":2,"list":[3]}})
    );
}

#[test]
fn malformed_plugin_manifest_is_isolated_from_valid_metadata() {
    let temporary = tempdir().expect("tempdir");
    copy_fixture(temporary.path(), "valid_plugin");
    let invalid = temporary.path().join("invalid_plugin");
    fs::create_dir_all(&invalid).expect("invalid dir");
    fs::write(invalid.join("plugin.toml"), "id = [broken").expect("invalid manifest");
    let manager = PluginManager::new("0.1.0", ToolRegistry::new()).expect("manager");
    let report = manager
        .discover_isolated(&[temporary.path().to_path_buf()])
        .expect("isolated discovery");
    assert_eq!(report.plugins.len(), 1);
    assert_eq!(report.plugins[0].id, "valid_plugin");
    assert_eq!(report.errors.len(), 1);
}
