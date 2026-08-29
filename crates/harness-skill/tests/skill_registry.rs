use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use harness_permission::PermissionAction;
use harness_skill::{SkillRegistry, SkillSource, SkillStatus};
use harness_tool::{
    ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};
use tempfile::tempdir;

struct NoopTool;

impl ToolProvider for NoopTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(value.clone())
    }

    fn permission_action(&self, _args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        Ok(PermissionAction::InternalCompute {
            capability: "test".to_owned(),
        })
    }

    fn execute(&self, _input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        Ok(serde_json::json!({}))
    }
}

#[test]
fn discovery_is_metadata_only_and_load_is_bounded_and_tool_aware() {
    let temporary = tempdir().expect("tempdir");
    let skill = temporary.path().join("review_skill");
    fs::create_dir_all(&skill).expect("skill dir");
    fs::write(skill.join("SKILL.md"), "old prompt").expect("prompt");
    fs::write(skill.join("rubric.md"), "review rubric").expect("reference");
    fs::write(
        skill.join("skill.toml"),
        r#"id = "review_skill"
name = "Review Skill"
version = "1.0.0"
description = "review code changes"
entry = "SKILL.md"
references = ["rubric.md"]
tags = ["review", "code"]
requiredTools = ["files.read"]
workflows = ["inspect", "report"]
agentTemplates = ["reviewer"]
"#,
    )
    .expect("manifest");
    let tools = ToolRegistry::new();
    let registry = SkillRegistry::new(tools.clone());
    let discovered = registry
        .discover(&[(temporary.path().to_path_buf(), SkillSource::Project)])
        .expect("discover");
    assert_eq!(discovered[0].status, SkillStatus::MetadataOnly);
    fs::write(skill.join("SKILL.md"), "new prompt loaded lazily").expect("mutate prompt");
    let missing = registry
        .load("review_skill", None)
        .expect_err("required tool missing");
    assert_eq!(missing.code, "skill-required-tools-missing");
    assert_eq!(
        registry.list().expect("list")[0].status,
        SkillStatus::Blocked
    );

    tools
        .register(
            ToolDescriptor {
                canonical_name: "files.read".to_owned(),
                version: "1".to_owned(),
                description: "read".to_owned(),
                effect_class: ToolEffectClass::ReadOnlyRetryable,
                source: ToolSource::Test,
                prompt_loading: ToolPromptLoading::Eager,
                keywords: vec![],
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
            },
            Arc::new(NoopTool),
        )
        .expect("register tool");
    let loaded = registry.load("review_skill", None).expect("load");
    assert_eq!(loaded.prompt, "new prompt loaded lazily");
    assert_eq!(
        loaded.references[&PathBuf::from("rubric.md")],
        "review rubric"
    );
    assert_eq!(loaded.view.status, SkillStatus::Loaded);
    assert!(loaded.view.content_hash.is_some());
    assert_eq!(
        registry.search("review code", 10).expect("search")[0].id,
        "review_skill"
    );
    assert_eq!(
        registry.unload("review_skill").expect("unload").status,
        SkillStatus::MetadataOnly
    );
}

#[test]
fn undeclared_reference_and_parent_path_are_rejected() {
    let temporary = tempdir().expect("tempdir");
    let skill = temporary.path().join("bad_skill");
    fs::create_dir_all(&skill).expect("skill dir");
    fs::write(skill.join("SKILL.md"), "prompt").expect("prompt");
    fs::write(skill.join("secret.md"), "secret").expect("secret");
    fs::write(
        skill.join("skill.toml"),
        r#"id = "bad_skill"
name = "Bad Skill"
version = "1.0.0"
description = "bad path test"
entry = "SKILL.md"
references = []
"#,
    )
    .expect("manifest");
    let registry = SkillRegistry::new(ToolRegistry::new());
    registry
        .discover(&[(temporary.path().to_path_buf(), SkillSource::Project)])
        .expect("discover");
    assert!(
        registry
            .load("bad_skill", Some(&[PathBuf::from("secret.md")]))
            .is_err()
    );

    fs::write(
        skill.join("bad.toml"),
        r#"id = "escape_skill"
name = "Escape"
version = "1.0.0"
description = "escape path"
entry = "../outside.md"
"#,
    )
    .expect("bad manifest");
    assert!(
        registry
            .install(&skill.join("bad.toml"), SkillSource::Project)
            .is_err()
    );
}

#[test]
fn malformed_skill_manifest_is_isolated_from_valid_metadata() {
    let temporary = tempdir().expect("tempdir");
    for (name, manifest) in [
        (
            "valid",
            r#"id = "valid_skill"
name = "Valid"
version = "1.0.0"
description = "valid skill"
entry = "SKILL.md"
"#,
        ),
        ("invalid", "id = [broken"),
    ] {
        let directory = temporary.path().join(name);
        fs::create_dir_all(&directory).expect("dir");
        fs::write(directory.join("SKILL.md"), "prompt").expect("prompt");
        fs::write(directory.join("skill.toml"), manifest).expect("manifest");
    }
    let registry = SkillRegistry::new(ToolRegistry::new());
    let report = registry
        .discover_isolated(&[(temporary.path().to_path_buf(), SkillSource::Project)])
        .expect("isolated discovery");
    assert_eq!(report.skills.len(), 1);
    assert_eq!(report.skills[0].id, "valid_skill");
    assert_eq!(report.errors.len(), 1);
}
