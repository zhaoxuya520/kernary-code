use std::collections::BTreeMap;
use std::sync::Arc;

use harness_builtin_tools::{PatchStatus, PatchStore, WorkspacePathGuard, WorkspaceSandbox};
use harness_lsp::{
    LspComputedFileEdit, LspComputedWorkspaceEdit, LspDocumentSnapshot, LspManager,
    LspPositionEncoding, LspServerConfig,
};
use harness_lsp_patch::{
    LspPatchCoordinator, LspPatchStatus, LspPatchStore, register_lsp_patch_tools,
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
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn sha(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn computed(files: &[(&str, &str, &str)], fingerprint_seed: &str) -> LspComputedWorkspaceEdit {
    let files = files
        .iter()
        .map(|(path, before, after)| LspComputedFileEdit {
            path: (*path).to_owned(),
            before_hash: sha(before),
            after_hash: sha(after),
            after_text: (*after).to_owned(),
            edit_count: 1,
            added_bytes: after.len(),
            removed_bytes: before.len(),
        })
        .collect::<Vec<_>>();
    LspComputedWorkspaceEdit {
        server_id: "fixture".to_owned(),
        source_method: "rename".to_owned(),
        title: "Fixture rename".to_owned(),
        source_document: LspDocumentSnapshot {
            path: files[0].path.clone(),
            file_hash: files[0].before_hash.clone(),
            document_version: 1,
            position_encoding: LspPositionEncoding::Utf16,
        },
        fingerprint: sha(fingerprint_seed),
        total_edits: files.len(),
        files,
    }
}

fn envelope() -> ExecutionEnvelope {
    ExecutionEnvelope {
        project_id: ProjectId::from("project:test"),
        mission_id: MissionId::from("mission:test"),
        run_id: Some(RunId::from("run:test")),
        actor_id: ActorId::from("agent:coder"),
        origin: InvocationOrigin::Agent,
        information_flow: InformationFlowLabel {
            integrity: IntegrityLabel::Trusted,
            confidentiality: ConfidentialityLabel::ProjectPrivate,
        },
    }
}

#[test]
fn preview_is_read_only_apply_and_undo_are_separately_approved_leased_and_journaled() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("project");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/a.rs"), "old-a\n").expect("a");
    std::fs::write(root.join("src/b.rs"), "old-b\n").expect("b");
    let guard = WorkspacePathGuard::new(&root).expect("guard");
    let previews = Arc::new(LspPatchStore::new(
        root.join(".harness/lsp-previews"),
        guard.clone(),
    ));
    let patches =
        Arc::new(PatchStore::open(root.join(".harness/patches"), guard.clone()).expect("patches"));
    let coordinator = Arc::new(
        LspPatchCoordinator::new(
            &root,
            root.join(".harness/leases.sqlite"),
            previews.clone(),
            patches.clone(),
        )
        .expect("coordinator"),
    );
    let preview = previews
        .save_computed(
            computed(
                &[
                    ("src/a.rs", "old-a\n", "new-a\n"),
                    ("src/b.rs", "old-b\n", "new-b\n"),
                ],
                "apply-undo",
            ),
            1,
        )
        .expect("preview");
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "old-a\n"
    );
    assert_eq!(
        previews
            .save_computed(
                computed(
                    &[
                        ("src/a.rs", "old-a\n", "new-a\n"),
                        ("src/b.rs", "old-b\n", "new-b\n"),
                    ],
                    "apply-undo",
                ),
                2,
            )
            .expect("idempotent")
            .id,
        preview.id
    );

    let executable = std::env::current_exe().expect("current exe");
    let manager = LspManager::new(
        &root,
        vec![LspServerConfig {
            id: "fixture".to_owned(),
            command: executable.clone(),
            args: vec![],
            cwd: None,
            language_ids: BTreeMap::from([("rs".to_owned(), "rust".to_owned())]),
            inherit_env: vec![],
            initialization_options: None,
            request_timeout_millis: Some(1_000),
            max_message_bytes: Some(1024 * 1024),
        }],
    )
    .expect("manager");
    let registry = ToolRegistry::new();
    register_lsp_patch_tools(&registry, manager, previews.clone(), coordinator.clone())
        .expect("tools");
    let mut profile = workspace_write_profile(root.clone());
    profile.subprocess.allowed_executables = vec![executable.clone()];
    let runtime = ToolRuntime::new(
        registry,
        PermissionEngine::new(profile, ApprovalPolicy::OnRequest),
        Arc::new(MemoryToolJournal::new()),
        Arc::new(WorkspaceSandbox::with_processes(guard, vec![executable]).expect("sandbox")),
    );
    let apply = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("tool:apply"),
            approval_request_id: PermissionRequestId::from("approval:apply"),
            idempotency_key: "apply-once".to_owned(),
            envelope: envelope(),
            tool_name: "lsp.patch.apply".to_owned(),
            args: serde_json::json!({"previewId":preview.id}),
            now_millis: 3,
        })
        .expect("apply request");
    assert_eq!(
        apply.invocation.status,
        ToolInvocationStatus::WaitingApproval
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "old-a\n"
    );
    let applied = runtime
        .resume_after_approval(
            &apply.invocation.id,
            &envelope(),
            GrantScope::Project,
            PermissionGrantId::from("grant:apply"),
            PermissionRequestId::from("approval:next"),
            4,
        )
        .expect("apply");
    assert_eq!(applied.invocation.status, ToolInvocationStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "new-a\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/b.rs")).unwrap(),
        "new-b\n"
    );
    assert_eq!(
        previews.load(&preview.id).expect("preview").status,
        LspPatchStatus::Applied
    );

    let undo = runtime
        .invoke(ToolInvokeRequest {
            invocation_id: ToolInvocationId::from("tool:undo"),
            approval_request_id: PermissionRequestId::from("approval:undo"),
            idempotency_key: "undo-once".to_owned(),
            envelope: envelope(),
            tool_name: "lsp.patch.undo".to_owned(),
            args: serde_json::json!({"previewId":preview.id}),
            now_millis: 5,
        })
        .expect("undo request");
    assert_eq!(
        undo.invocation.status,
        ToolInvocationStatus::WaitingApproval
    );
    let undone = runtime
        .resume_after_approval(
            &undo.invocation.id,
            &envelope(),
            GrantScope::Once,
            PermissionGrantId::from("grant:undo"),
            PermissionRequestId::from("approval:unused"),
            6,
        )
        .expect("undo");
    assert_eq!(undone.invocation.status, ToolInvocationStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "old-a\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/b.rs")).unwrap(),
        "old-b\n"
    );
    assert_eq!(
        previews.load(&preview.id).expect("preview").status,
        LspPatchStatus::Undone
    );
    assert!(
        patches
            .list()
            .expect("patch records")
            .iter()
            .all(|patch| patch.status == PatchStatus::Undone)
    );
}

#[test]
fn stale_preview_is_rejected_and_interrupted_apply_reconciles_by_undoing_children() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("project");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    let file = root.join("src/a.rs");
    let second = root.join("src/b.rs");
    std::fs::write(&file, "old\n").expect("file");
    std::fs::write(&second, "old-b\n").expect("second");
    let guard = WorkspacePathGuard::new(&root).expect("guard");
    let previews = Arc::new(LspPatchStore::new(
        root.join(".harness/lsp-previews"),
        guard.clone(),
    ));
    let patches =
        Arc::new(PatchStore::open(root.join(".harness/patches"), guard).expect("patches"));
    let coordinator = LspPatchCoordinator::new(
        &root,
        root.join(".harness/leases.sqlite"),
        previews.clone(),
        patches.clone(),
    )
    .expect("coordinator");
    let stale = previews
        .save_computed(computed(&[("src/a.rs", "old\n", "new\n")], "stale"), 1)
        .expect("stale preview");
    std::fs::write(&file, "external\n").expect("external change");
    assert_eq!(
        coordinator
            .apply(
                &stale.id,
                &ToolInvocationId::from("tool:stale"),
                &RunId::from("run:stale"),
                2,
            )
            .expect_err("stale")
            .code,
        "lsp-patch-stale-before"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "external\n");

    std::fs::write(&file, "old\n").expect("restore");
    let interrupted = previews
        .save_computed(
            computed(
                &[
                    ("src/a.rs", "old\n", "after\n"),
                    ("src/b.rs", "old-b\n", "after-b\n"),
                ],
                "interrupted",
            ),
            3,
        )
        .expect("preview");
    let after = previews.after_bytes(&interrupted.files[0]).expect("blob");
    let child = format!(
        "tool:crash:lsp-file:0:{}",
        &interrupted.files[0].after_hash[..12]
    );
    let missing_child = format!(
        "tool:crash:lsp-file:1:{}",
        &interrupted.files[1].after_hash[..12]
    );
    patches
        .prepare_with_id(child.clone(), &file, Some(b"old\n"), &after, 4)
        .expect("prepare");
    patches
        .apply_prepared(&child, &after, 5)
        .expect("apply child");
    previews
        .transition(
            &interrupted.id,
            &[LspPatchStatus::Ready],
            LspPatchStatus::Applying,
            Some(vec![child.clone(), missing_child]),
            4,
        )
        .expect("applying journal");
    let recovered = coordinator.reconcile(6).expect("reconcile");
    assert_eq!(recovered[0].status, LspPatchStatus::RolledBack);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "old-b\n");
    assert_eq!(
        patches.load(&child).expect("child").status,
        PatchStatus::Undone
    );
    let retried = coordinator
        .apply(
            &interrupted.id,
            &ToolInvocationId::from("tool:crash"),
            &RunId::from("run:retry"),
            7,
        )
        .expect("same child ids safely re-prepared");
    assert_eq!(retried.status, LspPatchStatus::Applied);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "after-b\n");
}
