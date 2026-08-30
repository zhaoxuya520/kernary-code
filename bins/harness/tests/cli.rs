use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use harness_memory::{VectorCatalogConfig, VectorProviderConfig};
use rusqlite::OptionalExtension;
use tempfile::tempdir;

type CliFactory = fn() -> Command;
type DocumentationCase<'a> = (CliFactory, Vec<&'a str>, &'a str);

static VECTOR_CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static MODEL_CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROVIDER_CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn isolated_global_vector_config() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "kernary-cli-vector-{}-{}.toml",
        std::process::id(),
        VECTOR_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn isolated_global_model_config() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "kernary-cli-model-{}-{}.json",
        std::process::id(),
        MODEL_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn isolated_global_provider_config() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "kernary-cli-provider-{}-{}.toml",
        std::process::id(),
        PROVIDER_CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn harness() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harness"));
    command.env("KERNARY_ENABLE_TEST_MODEL", "1");
    command.env("KERNARY_ISOLATE_GLOBAL_CONFIG", "1");
    command.env(
        "KERNARY_GLOBAL_VECTOR_CONFIG",
        isolated_global_vector_config(),
    );
    command.env(
        "KERNARY_GLOBAL_MODEL_CONFIG",
        isolated_global_model_config(),
    );
    command.env("KERNARY_PROVIDER_CONFIG", isolated_global_provider_config());
    command
}

fn kernary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kernary"));
    command.env("KERNARY_ENABLE_TEST_MODEL", "1");
    command.env("KERNARY_ISOLATE_GLOBAL_CONFIG", "1");
    command.env(
        "KERNARY_GLOBAL_VECTOR_CONFIG",
        isolated_global_vector_config(),
    );
    command.env(
        "KERNARY_GLOBAL_MODEL_CONFIG",
        isolated_global_model_config(),
    );
    command.env("KERNARY_PROVIDER_CONFIG", isolated_global_provider_config());
    command
}

fn kernary_without_test_model() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kernary"));
    command.env_remove("KERNARY_ENABLE_TEST_MODEL");
    command.env_remove("HARNESS_ENABLE_TEST_MODEL");
    command.env("KERNARY_ISOLATE_GLOBAL_CONFIG", "1");
    command.env(
        "KERNARY_GLOBAL_VECTOR_CONFIG",
        isolated_global_vector_config(),
    );
    command.env(
        "KERNARY_GLOBAL_MODEL_CONFIG",
        isolated_global_model_config(),
    );
    command.env("KERNARY_PROVIDER_CONFIG", isolated_global_provider_config());
    command
}

#[test]
fn published_default_has_no_fake_model_and_refuses_unconfigured_work() {
    let temporary = tempdir().expect("tempdir");
    let output = kernary_without_test_model()
        .current_dir(temporary.path())
        .args(["run", "--headless", "do", "real", "work"])
        .output()
        .expect("unconfigured run");
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(stderr.contains("MODEL_NOT_CONFIGURED"), "stderr={stderr}");
    assert!(!stdout.contains("deterministic:"), "stdout={stdout}");
    assert!(!stdout.contains("Agent 已完成"), "stdout={stdout}");

    let doctor = kernary_without_test_model()
        .current_dir(temporary.path())
        .args(["doctor", "--json"])
        .output()
        .expect("doctor");
    assert!(doctor.status.success());
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).expect("doctor json");
    assert_eq!(report["model"], "unconfigured");
    assert!(report["provider"].is_null());

    let mut child = kernary_without_test_model()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("plain unconfigured");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/model\n/models\nreal work\n/exit\n")
        .expect("commands");
    let plain = child.wait_with_output().expect("plain output");
    assert!(plain.status.success());
    let stdout = String::from_utf8(plain.stdout).expect("plain stdout");
    assert!(stdout.contains("模型      未配置"), "stdout={stdout}");
    assert!(stdout.contains("MODEL_NOT_CONFIGURED"), "stdout={stdout}");
    assert!(!stdout.contains("fake/deterministic"), "stdout={stdout}");
    assert!(
        !stdout.contains("kernary-internal/unconfigured"),
        "stdout={stdout}"
    );
    assert!(!stdout.contains("deterministic:"), "stdout={stdout}");
}

#[test]
fn legacy_persisted_fake_selection_migrates_to_unconfigured_without_data_loss() {
    let temporary = tempdir().expect("tempdir");
    let mut legacy = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("legacy process");
    legacy
        .stdin
        .as_mut()
        .expect("legacy stdin")
        .write_all(b"/goal set preserve-me\n/exit\n")
        .expect("legacy commands");
    assert!(legacy.wait().expect("legacy wait").success());

    let mut migrated = kernary_without_test_model()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("migrated process");
    migrated
        .stdin
        .as_mut()
        .expect("migrated stdin")
        .write_all(b"/goal\n/status\n/exit\n")
        .expect("migrated commands");
    let output = migrated.wait_with_output().expect("migrated output");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Goal: preserve-me"), "stdout={stdout}");
    assert!(stdout.contains("模型  未配置"), "stdout={stdout}");
    assert!(!stdout.contains("fake/deterministic"), "stdout={stdout}");
}

fn git_executable() -> Option<String> {
    #[cfg(windows)]
    let output = Command::new("where.exe").arg("git.exe").output().ok()?;
    #[cfg(unix)]
    let output = Command::new("which").arg("git").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::to_owned)
}

#[test]
fn headless_run_outputs_completed_fake_plan() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .args(["run", "--headless", "finish", "stage", "four"])
        .output()
        .expect("harness run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(!stdout.contains("Agent 已完成"));
    assert!(stdout.contains("deterministic:finish stage four"));
    assert!(stdout.contains("1 accepted"));
}

#[test]
fn exec_json_is_one_noninteractive_automation_document() {
    let temporary = tempdir().expect("tempdir");
    let output = kernary()
        .current_dir(temporary.path())
        .args(["exec", "--json", "verify", "automation", "contract"])
        .output()
        .expect("kernary exec");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert_eq!(stdout.lines().count(), 1, "stdout={stdout}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("exec JSON");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["type"], "exec.result");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["exitCode"], 0);
    assert!(value["sessionId"].as_str().is_some());
    assert!(value["missionId"].as_str().is_some());
    assert!(
        value["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
    assert_eq!(value["plan"]["running"], 0);
    assert_eq!(value["plan"]["pending"], 0);
}

#[test]
fn exec_output_is_atomic_quiet_and_refuses_overwrite_without_force() {
    let temporary = tempdir().expect("tempdir");
    let output_path = temporary.path().join("result.txt");
    let first = kernary()
        .current_dir(temporary.path())
        .args([
            "exec",
            "--quiet",
            "--output",
            output_path.to_str().expect("path"),
            "first",
            "automation",
        ])
        .output()
        .expect("first exec");
    assert!(first.status.success());
    assert!(first.stdout.is_empty());
    let original = std::fs::read(&output_path).expect("output");
    assert!(String::from_utf8_lossy(&original).contains("Plan mission:"));

    let refused = kernary()
        .current_dir(temporary.path())
        .args([
            "exec",
            "--quiet",
            "--output",
            output_path.to_str().expect("path"),
            "refused",
            "overwrite",
        ])
        .output()
        .expect("refused exec");
    assert!(!refused.status.success());
    assert_eq!(std::fs::read(&output_path).expect("preserved"), original);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--force"),
        "stderr={}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let replaced = kernary()
        .current_dir(temporary.path())
        .args([
            "exec",
            "--json",
            "--quiet",
            "--output",
            output_path.to_str().expect("path"),
            "--force",
            "replace",
            "automation",
        ])
        .output()
        .expect("replace exec");
    assert!(replaced.status.success());
    assert!(replaced.stdout.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&output_path).expect("replaced"))
            .expect("result json");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["events"], serde_json::json!([]));
    let leftovers = std::fs::read_dir(temporary.path())
        .expect("list")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.contains("kernary-new") || name.contains("kernary-backup"))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "leftovers={leftovers:?}");
}

#[test]
fn headless_run_persists_isolated_agent_session_lifecycle() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .args(["run", "--headless", "inspect", "agent", "session"])
        .output()
        .expect("harness run");
    assert!(output.status.success());
    let connection = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agent db");
    let state_json: String = connection
        .query_row("SELECT state_json FROM agent_sessions LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("agent session");
    let state: serde_json::Value = serde_json::from_str(&state_json).expect("state json");
    assert_eq!(state["status"], "completed");
    assert_eq!(state["role"], "coder");
    assert_eq!(state["agentDefinitionId"], "agent:coder");
    assert!(
        state["contextFingerprint"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let endpoint_count: usize = connection
        .query_row("SELECT COUNT(*) FROM agent_endpoints", [], |row| row.get(0))
        .expect("endpoint count");
    assert_eq!(endpoint_count, 30);
    let result_json: String = connection
        .query_row("SELECT result_json FROM agent_results LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("compressed result");
    let result: serde_json::Value = serde_json::from_str(&result_json).expect("result json");
    assert_eq!(result["status"], "completed");
    assert!(
        result["summary"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(result.get("metrics").is_some());
    assert!(result.get("followUp").is_some());
    let budget_status: String = connection
        .query_row(
            "SELECT status FROM agent_budget_escrows LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("budget escrow");
    assert_eq!(budget_status, "completed");
}

#[test]
fn json_run_emits_only_valid_json_lines() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .args(["run", "--json", "json", "task"])
        .output()
        .expect("harness json run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert!(lines.len() >= 4);
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("不是合法 JSONL: {line}: {error}"));
    }
    let last: serde_json::Value =
        serde_json::from_str(lines.last().expect("last line")).expect("result JSON");
    assert_eq!(last["type"], "command.result");
}

#[test]
fn doctor_json_reports_storage_and_terminal_capabilities() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .args(["doctor", "--json"])
        .output()
        .expect("harness doctor");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(value["status"], "ok");
    assert_eq!(value["product"]["name"], "Kernary Code");
    assert_eq!(value["product"]["mascot"], "Kern");
    assert_eq!(value["command"]["primary"], "kernary");
    assert_eq!(value["command"]["invokedAs"], "harness");
    assert_eq!(value["compatibility"]["stateDirectory"], ".harness");
    assert_eq!(
        value["compatibility"]["credentialService"],
        "dev.openai.harness"
    );
    assert!(value["providers"]["catalogCount"].as_u64().unwrap_or(0) >= 10);
    assert_eq!(value["providers"]["customConfigPresent"], false);
    assert_eq!(
        value["providers"]["scope"],
        "global-user-with-legacy-project-import"
    );
    assert_eq!(value["defaultModel"]["scope"], "global-user");
    assert_eq!(value["defaultModel"]["configured"], false);
    assert_eq!(value["storageSchema"], 3);
    assert_eq!(value["stage"], 38);
    assert_eq!(value["stageTrack"], "42-responsive-streaming-tui");
    assert_eq!(value["sandbox"]["mode"], "workspace-write");
    assert!(value["sandbox"]["available"].as_bool().is_some());
    assert!(value["sandbox"]["backend"].as_str().is_some());
    assert!(
        value["providers"]["discovery"]["configured"]
            .as_u64()
            .unwrap_or(0)
            >= 4
    );
    assert_eq!(
        value["providers"]["discovery"]["activationRule"],
        "explicit-single-provider-refresh"
    );
    assert_eq!(value["lsp"]["configured"], false);
    assert_eq!(
        value["lsp"]["activationRule"],
        "explicit-start-query-or-approved-on-demand-tool"
    );
    assert_eq!(
        value["lsp"]["toolBridge"],
        "read-only-process-spawn-permission"
    );
    assert_eq!(
        value["lsp"]["positionModel"],
        "human-scalar-1-based-to-negotiated-protocol-units"
    );
    assert_eq!(
        value["lsp"]["patchPreview"],
        "rename-codeaction-preview-second-approval-recoverable-set"
    );
    assert_eq!(
        value["extensions"]["mcp"],
        "oauth-pkce-cimd-scope-step-up-private-key-jwt-cross-app-streamable-http-sse-lazy"
    );
    assert_eq!(value["vector"]["configured"], false);
    assert_eq!(value["browser"]["configured"], false);
    assert!(
        !temporary.path().join(".harness").exists(),
        "doctor 必须是无项目写入的诊断命令"
    );
}

#[test]
fn maintenance_backup_verify_and_restore_are_pre_kernel_and_recoverable() {
    let temporary = tempdir().expect("tempdir");
    let first = harness()
        .current_dir(temporary.path())
        .args(["run", "--headless", "create", "durable", "state"])
        .output()
        .expect("initial run");
    assert!(first.status.success());
    let backup = temporary.path().join("backup-one");
    let output = harness()
        .current_dir(temporary.path())
        .args(["maintenance", "backup", "--output"])
        .arg(&backup)
        .output()
        .expect("backup");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(backup.join("manifest.json").is_file());
    assert!(backup.join("manifest.sha256").is_file());
    let verified = harness()
        .current_dir(temporary.path())
        .args(["maintenance", "verify"])
        .arg(&backup)
        .output()
        .expect("verify");
    assert!(verified.status.success());

    let kernel = temporary.path().join(".harness/kernel.sqlite");
    let connection = rusqlite::Connection::open(&kernel).expect("kernel");
    connection
        .execute_batch(
            "CREATE TABLE restore_probe(value TEXT); INSERT INTO restore_probe VALUES('new');",
        )
        .expect("mutate after backup");
    drop(connection);
    let denied = harness()
        .current_dir(temporary.path())
        .args(["maintenance", "restore"])
        .arg(&backup)
        .output()
        .expect("restore denied");
    assert!(!denied.status.success());
    let restored = harness()
        .current_dir(temporary.path())
        .args(["maintenance", "restore"])
        .arg(&backup)
        .arg("--force")
        .output()
        .expect("restore");
    assert!(
        restored.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let connection = rusqlite::Connection::open(&kernel).expect("restored kernel");
    let probe: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='restore_probe'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("probe query");
    assert!(probe.is_none());
    assert!(temporary.path().join(".harness/backups").is_dir());
}

#[test]
fn help_does_not_create_project_state() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .arg("--help")
        .output()
        .expect("harness help");
    assert!(output.status.success());
    assert!(!temporary.path().join(".harness").exists());
}

#[test]
fn kernary_is_primary_harness_is_alias_and_both_share_durable_session() {
    for (mut command, expected) in [(kernary(), "Usage: kernary"), (harness(), "Usage: harness")] {
        let output = command.arg("--help").output().expect("help");
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .expect("stdout")
                .contains(expected)
        );
    }

    let temporary = tempdir().expect("tempdir");
    let mut legacy = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("legacy");
    legacy
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/goal set shared-brand-session\n/reasoning high\n/exit\n")
        .expect("commands");
    assert!(legacy.wait().expect("wait").success());

    let mut primary = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("primary");
    primary
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/goal\n/status\n/exit\n")
        .expect("commands");
    let output = primary.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("Goal: shared-brand-session"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("推理 high"), "stdout={stdout}");
    assert!(temporary.path().join(".harness/kernel.sqlite").is_file());
    assert!(!temporary.path().join(".kernary").exists());
}

#[test]
fn interactive_launch_creates_project_local_sessions_and_resume_never_crosses_folders() {
    let project_a = tempdir().expect("project a");
    for prompt in ["build auth feature", "fix cache regression"] {
        let mut child = kernary()
            .current_dir(project_a.path())
            .args(["--ui", "plain", "--ascii"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn session");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(format!("{prompt}\n/exit\n").as_bytes())
            .expect("write session");
        assert!(
            child
                .wait_with_output()
                .expect("session output")
                .status
                .success()
        );
    }

    let mut list = kernary()
        .current_dir(project_a.path())
        .args(["--ui", "plain", "--ascii", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("continue latest");
    list.stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/sessions\n/exit\n")
        .expect("list");
    let output = list.wait_with_output().expect("list output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Sessions | 2"), "stdout={stdout}");
    assert!(stdout.contains("build auth feature"), "stdout={stdout}");
    assert!(stdout.contains("fix cache regression"), "stdout={stdout}");
    let first_id = stdout
        .lines()
        .find(|line| line.contains("build auth feature | #"))
        .and_then(|line| line.split(" | ").nth(1))
        .and_then(|tail| tail.split_whitespace().next())
        .expect("first session id")
        .to_owned();

    let mut resumed = kernary()
        .current_dir(project_a.path())
        .args(["--ui", "plain", "--ascii", "-r", &first_id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("resume by id");
    resumed
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/status\n/exit\n")
        .expect("resume commands");
    let resumed = resumed.wait_with_output().expect("resume output");
    assert!(resumed.status.success());
    assert!(
        String::from_utf8_lossy(&resumed.stdout).contains("build auth feature"),
        "stdout={}",
        String::from_utf8_lossy(&resumed.stdout)
    );

    let project_b = tempdir().expect("project b");
    let isolated = kernary()
        .current_dir(project_b.path())
        .args(["--ui", "plain", "--ascii", "-r", &first_id])
        .output()
        .expect("cross-project resume");
    assert!(!isolated.status.success());
    assert!(
        String::from_utf8_lossy(&isolated.stderr).contains("当前项目没有可恢复的 Session")
            || String::from_utf8_lossy(&isolated.stderr).contains("不存在 Session"),
        "stderr={}",
        String::from_utf8_lossy(&isolated.stderr)
    );
}

#[test]
fn kernary_environment_prefix_overrides_legacy_without_breaking_vector_off_gate() {
    for (primary, legacy, expected) in [
        (Some("primary/model"), Some(""), true),
        (Some(""), Some("legacy/model"), false),
        (None, Some("legacy/model"), true),
        (None, None, false),
    ] {
        let temporary = tempdir().expect("tempdir");
        let mut command = kernary();
        command
            .current_dir(temporary.path())
            .args(["doctor", "--json"]);
        match primary {
            Some(value) => {
                command.env("KERNARY_EMBEDDING_MODEL", value);
            }
            None => {
                command.env_remove("KERNARY_EMBEDDING_MODEL");
            }
        }
        match legacy {
            Some(value) => {
                command.env("HARNESS_EMBEDDING_MODEL", value);
            }
            None => {
                command.env_remove("HARNESS_EMBEDDING_MODEL");
            }
        }
        let output = command.output().expect("doctor");
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(value["vector"]["configured"], expected);
        assert!(!temporary.path().join(".harness").exists());
        assert!(!temporary.path().join(".kernary").exists());
    }
}

#[test]
fn completion_and_man_generation_are_pre_kernel_and_side_effect_free() {
    let cases: [DocumentationCase<'_>; 5] = [
        (
            harness,
            vec!["completions", "powershell"],
            "Register-ArgumentCompleter",
        ),
        (harness, vec!["completions", "bash"], "_harness"),
        (harness, vec!["man"], ".TH HARNESS 1"),
        (kernary, vec!["completions", "bash"], "_kernary"),
        (kernary, vec!["man"], ".TH KERNARY 1"),
    ];
    for (factory, arguments, marker) in cases {
        let temporary = tempdir().expect("tempdir");
        let output = factory()
            .current_dir(temporary.path())
            .args(arguments)
            .output()
            .expect("generate docs");
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf8");
        assert!(stdout.contains(marker), "stdout={stdout}");
        assert!(!temporary.path().join(".harness").exists());
    }
}

#[test]
fn provider_catalog_and_custom_local_relay_are_lazy_and_model_routable() {
    let temporary = tempdir().expect("tempdir");
    std::fs::write(
        temporary.path().join("kernary.providers.toml"),
        r#"
schema_version = 1

[[providers]]
id = "local-relay"
display_name = "Local Relay"
credential_required = false

[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://127.0.0.1:45678/v1/chat/completions"
models = ["local-coder"]
"#,
    )
    .expect("provider config");
    let listed = harness()
        .current_dir(temporary.path())
        .arg("providers")
        .output()
        .expect("providers");
    assert!(listed.status.success());
    let stdout = String::from_utf8(listed.stdout).expect("stdout");
    assert!(
        stdout.contains("opencode-go | OpenCode Go"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("local-relay | Local Relay"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("discovery=not-configured"),
        "stdout={stdout}"
    );
    assert!(!temporary.path().join(".harness").exists());

    let mut child = harness()
        .current_dir(temporary.path())
        .args([
            "--model",
            "local-relay/local-coder",
            "--ui",
            "plain",
            "--ascii",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/models\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("local-relay/local-coder"),
        "stdout={stdout}"
    );
}

#[test]
fn models_read_is_zero_network_and_does_not_create_discovery_or_vector_state() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .env_remove("HARNESS_EMBEDDING_MODEL")
        .args(["models", "--provider", "opencode-go", "--json"])
        .output()
        .expect("models");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let models: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("models json");
    assert!(!models.is_empty());
    assert!(
        models
            .iter()
            .all(|model| model["providerId"] == "opencode-go")
    );
    assert!(
        !temporary
            .path()
            .join(".harness/provider-models-v1.json")
            .exists()
    );
    let connection = rusqlite::Connection::open(temporary.path().join(".harness/memory.sqlite"))
        .expect("memory");
    let vector_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("vector schema");
    assert!(vector_table.is_none());
    assert!(!temporary.path().join(".harness/vector").exists());
}

#[test]
fn custom_openai_chat_relay_executes_through_catalog_protocol_route() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("read request");
            assert!(read > 0, "request closed before headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
            })
            .unwrap_or(0);
        while request.len() - header_end < content_length {
            let read = stream.read(&mut buffer).expect("read body");
            assert!(read > 0, "request closed before body");
            request.extend_from_slice(&buffer[..read]);
        }
        let body = String::from_utf8_lossy(&request[header_end..]);
        assert!(body.contains("local-coder"));
        let events = [
            serde_json::json!({"id":"chat_relay","model":"local-coder","choices":[{"delta":{"content":"relay-ok"},"finish_reason":"stop"}]}),
            serde_json::json!({"id":"chat_relay","model":"local-coder","choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
        ]
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            events.len(),
            events
        );
        stream.write_all(response.as_bytes()).expect("response");
    });
    let temporary = tempdir().expect("tempdir");
    std::fs::write(
        temporary.path().join("kernary.providers.toml"),
        format!(
            r#"
schema_version = 1
[[providers]]
id = "local-relay-e2e"
display_name = "Local Relay E2E"
credential_required = false
[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://{address}/v1/chat/completions"
models = ["local-coder"]
"#
        ),
    )
    .expect("provider config");
    let output = harness()
        .current_dir(temporary.path())
        .args([
            "--model",
            "local-relay-e2e/local-coder",
            "run",
            "--headless",
            "use",
            "relay",
        ])
        .output()
        .expect("relay run");
    server.join().expect("server");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("relay-ok"), "stdout={stdout}");
}

#[test]
fn dynamic_model_refresh_selects_and_calls_discovered_model_in_same_process() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buffer).expect("read request");
                assert!(read > 0, "request closed before headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let request_line = headers.lines().next().unwrap_or_default().to_owned();
            if request_line.starts_with("GET /v1/models ") {
                let body = r#"{"object":"list","data":[{"id":"snapshot-coder"},{"id":"discovered-coder"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("models response");
                continue;
            }
            assert!(
                request_line.starts_with("POST /v1/chat/completions "),
                "request={request_line}"
            );
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            while request.len() - header_end < content_length {
                let read = stream.read(&mut buffer).expect("read body");
                assert!(read > 0, "request closed before body");
                request.extend_from_slice(&buffer[..read]);
            }
            let body = String::from_utf8_lossy(&request[header_end..]);
            assert!(body.contains("discovered-coder"), "body={body}");
            let events = [
                serde_json::json!({"id":"dynamic","model":"discovered-coder","choices":[{"delta":{"content":"dynamic-relay-ok"},"finish_reason":"stop"}]}),
                serde_json::json!({"id":"dynamic","model":"discovered-coder","choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
            ]
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            stream
                .write_all(response.as_bytes())
                .expect("chat response");
        }
    });

    let temporary = tempdir().expect("tempdir");
    std::fs::write(
        temporary.path().join("kernary.providers.toml"),
        format!(
            r#"
schema_version = 1
[[providers]]
id = "dynamic-relay"
display_name = "Dynamic Relay"
credential_required = false
[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://{address}/v1/chat/completions"
models = ["snapshot-coder"]
[providers.discovery]
format = "openai-models"
endpoint = "http://{address}/v1/models"
auth = "none"
routing = "single-route-additive"
"#
        ),
    )
    .expect("provider config");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/models refresh dynamic-relay\n/model dynamic-relay/discovered-coder\ncall discovered model\n/exit\n",
        )
        .expect("commands");
    let output = child.wait_with_output().expect("wait");
    server.join().expect("server");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("dynamic-relay/discovered-coder"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("dynamic-relay-ok"), "stdout={stdout}");
    let cache_path = temporary.path().join(".harness/provider-models-v1.json");
    assert!(cache_path.is_file());
    let cache = std::fs::read_to_string(cache_path).expect("cache");
    assert!(cache.contains("discovered-coder"));
    assert!(!cache.to_ascii_lowercase().contains("authorization"));
}

#[test]
fn non_tty_model_selection_requires_explicit_secure_connect_without_state_write() {
    let temporary = tempdir().expect("tempdir");
    std::fs::write(
        temporary.path().join("kernary.providers.toml"),
        r#"
schema_version = 1

[[providers]]
id = "secure-relay-test"
display_name = "Secure Relay Test"
credential_required = true

[[providers.routes]]
protocol = "anthropic-messages"
endpoint = "https://relay.example.com/v1/messages"
models = ["secure-coder"]
"#,
    )
    .expect("provider config");
    let output = harness()
        .current_dir(temporary.path())
        .args(["--model", "secure-relay-test/secure-coder", "--ui", "plain"])
        .output()
        .expect("model selection");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(stderr.contains("CredentialRequired"), "stderr={stderr}");
    assert!(
        stderr.contains("kernary connect secure-relay-test"),
        "stderr={stderr}"
    );
    assert!(!temporary.path().join(".harness").exists());
}

#[test]
fn slash_connect_uses_secure_lane_and_cancels_without_tty() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/connect opencode-go\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Kernary secure lane"), "stdout={stdout}");
    assert!(
        stdout.contains("secure input requires TTY"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("credential 输入已取消"), "stdout={stdout}");
}

#[test]
fn every_setup_wizard_accepts_cancel_and_returns_to_normal_chat() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn setup cancellation test");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"keep this conversation visible\n/provider add\n/cancel\n/status\n/provider key\n/cancel\n/status\n/provider add\nExample Relay\nresponses\nhttps://example.com/v1\n/cancel\n/status\n/vector setup\n/cancel\n/status\n/vector setup\nvoyage\n/cancel\n/status\n/vector clear\n/cancel\n/status\n/permissions bypass\n/cancel\n/status\n/exit\n",
        )
        .expect("commands");
    let output = child.wait_with_output().expect("output");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.matches("设置已取消").count() >= 6, "stdout={stdout}");
    assert!(
        stdout.matches("模式  balanced").count() >= 6,
        "stdout={stdout}"
    );
    assert!(
        stdout.matches("keep this conversation visible").count() >= 2,
        "stdout={stdout}"
    );
    assert!(!temporary.path().join("kernary.providers.toml").exists());
}

#[test]
fn ascii_headless_has_no_decorative_unicode() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .args(["--ascii", "run", "--headless", "ascii", "task"])
        .output()
        .expect("ascii run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    for decorative in ['·', '◈', '❯', '│', '┌', '┐', '└', '┘'] {
        assert!(
            !stdout.contains(decorative),
            "发现装饰性 Unicode：{decorative}"
        );
    }
}

#[test]
fn plain_mode_accepts_slash_commands_from_stdin() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn plain harness");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(
            b"/status\n/model\n/models\n/reasoning max\n/provider\n/tools\n/permissions\n/sandbox\n/goal set terminal goal\n/context\n/compact auto\n/checkpoint smoke\n/pin keep-this\n/focus src/main.rs\n/cache\n/goal\n/exit\n",
        )
        .expect("write slash commands");
    let output = child.wait_with_output().expect("wait plain harness");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("会话  未命名会话 | #"));
    assert!(stdout.contains("模型      未配置"));
    assert!(
        stdout.contains("模型  未配置 | 推理 max"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Provider: 未配置"));
    assert!(stdout.contains("files.read@1"));
    assert!(stdout.contains("Pending approvals: 0"));
    assert!(stdout.contains("Sandbox mode: workspace-write"));
    assert!(stdout.contains("Platform backend:"));
    assert!(stdout.contains("Filesystem boundary:"));
    assert!(stdout.contains("Goal: terminal goal"));
    assert!(stdout.contains("Context  series:"));
    assert!(stdout.contains("Auto compact active"));
    assert!(stdout.contains("Checkpoint checkpoint:"));
    assert!(stdout.contains("Cache hit rate"));
    assert!(stdout.contains("[DONE] Shutdown | user-exit"));
}

#[test]
fn clear_and_cls_are_local_aliases_even_without_a_configured_model() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary_without_test_model()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn plain kernary");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"clear\ncls\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(!stdout.contains("MODEL_NOT_CONFIGURED"), "stdout={stdout}");
}

#[test]
fn language_switch_persists_and_localizes_command_help() {
    let temporary = tempdir().expect("tempdir");
    let mut first = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("first");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/language en\n/help provider\n/exit\n")
        .expect("commands");
    let output = first.wait_with_output().expect("output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Interface language: en"), "stdout={stdout}");
    assert!(
        stdout.contains("Show, add, switch, re-key, or remove text-model providers"),
        "stdout={stdout}"
    );

    let mut second = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("second");
    second
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/language\n/help vector\n/exit\n")
        .expect("commands");
    let output = second.wait_with_output().expect("output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Language: en"), "stdout={stdout}");
    assert!(
        stdout.contains("Configure the global embedding provider and project-local retrieval"),
        "stdout={stdout}"
    );
}

#[test]
fn provider_switch_uses_default_and_model_switch_stays_within_provider() {
    let temporary = tempdir().expect("tempdir");
    std::fs::write(
        temporary.path().join("kernary.providers.toml"),
        r#"schema_version = 1

[[providers]]
id = "custom-local"
display_name = "Custom Local"
credential_required = false
default_model = "model-b"

[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://127.0.0.1:18888/v1/chat/completions"
models = ["model-a", "model-b"]
"#,
    )
    .expect("provider config");
    let mut child = kernary_without_test_model()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("process");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/provider switch\ncustom-local\n/status\n/model model-a\n/status\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("output");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("已切换提供商: custom-local/model-b"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("模型  custom-local/model-a"),
        "stdout={stdout}"
    );
}

#[test]
fn plain_mode_can_checkpoint_and_compact_real_context() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn plain harness");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(
            format!(
                "{}\n{}\n{}\n/checkpoint before-compact\n/fork before-compact session:child\n/pin temporary\n/rollback before-compact\n/compact safe\n/context\n/exit\n",
                "first durable task ".repeat(120),
                "second observation ".repeat(120),
                "third observation ".repeat(120)
            )
            .as_bytes(),
        )
        .expect("write context flow");
    let output = child.wait_with_output().expect("wait plain harness");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("Checkpoint checkpoint:"));
    assert!(stdout.contains("Forked child #"));
    assert!(stdout.contains("parent #") && stdout.contains(" unchanged"));
    assert!(stdout.contains("Rolled back into new series"));
    assert!(stdout.contains("Context compacted Safe"));
    assert!(stdout.contains("Recovery 2 checkpoint(s)"));
}

#[test]
fn non_tty_openai_login_rejects_empty_stdin_without_echo() {
    let temporary = tempdir().expect("tempdir");
    let output = harness()
        .current_dir(temporary.path())
        .args(["login", "openai"])
        .stdin(Stdio::null())
        .output()
        .expect("login");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(stderr.contains("API key 不能为空"));
    assert!(!temporary.path().join(".harness").exists());
}

#[test]
fn model_and_reasoning_selection_resume_from_session_events() {
    let temporary = tempdir().expect("tempdir");
    let first = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("first process");
    let mut first = first;
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/reasoning max\n/exit\n")
        .expect("write");
    assert!(first.wait().expect("wait first").success());

    let mut second = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("second process");
    second
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/status\n/exit\n")
        .expect("write");
    let output = second.wait_with_output().expect("wait second");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("推理 max"), "stdout={stdout}");
}

#[test]
fn default_text_provider_and_model_are_global_across_projects_and_windows() {
    let temporary = tempdir().expect("tempdir");
    let first_project = temporary.path().join("first-project");
    let second_project = temporary.path().join("second-project");
    std::fs::create_dir_all(&first_project).expect("first project");
    std::fs::create_dir_all(&second_project).expect("second project");
    std::fs::write(
        first_project.join("kernary.providers.toml"),
        r#"
schema_version = 1

[[providers]]
id = "local-relay"
display_name = "Local Relay"
credential_required = false

[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://127.0.0.1:45678/v1/chat/completions"
models = ["local-coder"]
"#,
    )
    .expect("legacy project provider");
    let global_model = temporary.path().join("global/model.json");
    let global_providers = temporary.path().join("global/providers.toml");

    let mut first = kernary_without_test_model();
    first
        .current_dir(&first_project)
        .env("KERNARY_GLOBAL_MODEL_CONFIG", &global_model)
        .env("KERNARY_PROVIDER_CONFIG", &global_providers)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut first = first.spawn().expect("first window");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/model local-relay/local-coder\n/exit\n")
        .expect("select global model");
    let first_output = first.wait_with_output().expect("first output");
    assert!(first_output.status.success());
    let first_stdout = String::from_utf8(first_output.stdout).expect("stdout");
    assert!(
        first_stdout.contains("Global default  local-relay/local-coder"),
        "stdout={first_stdout}"
    );
    assert!(global_model.is_file());
    assert!(global_providers.is_file());

    let mut second = kernary_without_test_model();
    second
        .current_dir(&second_project)
        .env("KERNARY_GLOBAL_MODEL_CONFIG", &global_model)
        .env("KERNARY_PROVIDER_CONFIG", &global_providers)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut second = second.spawn().expect("second window");
    second
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/status\n/exit\n")
        .expect("read inherited model");
    let second_output = second.wait_with_output().expect("second output");
    assert!(second_output.status.success());
    let second_stdout = String::from_utf8(second_output.stdout).expect("stdout");
    assert!(
        second_stdout.contains("local-relay/local-coder"),
        "stdout={second_stdout}"
    );
    assert!(!second_stdout.contains("MODEL_NOT_CONFIGURED"));
}

#[test]
fn provider_switch_reloads_late_global_custom_provider_and_registers_it_lazily() {
    let temporary = tempdir().expect("tempdir");
    let project = temporary.path().join("project");
    let global_provider = temporary.path().join("global/providers.toml");
    let global_model = temporary.path().join("global/model.json");
    std::fs::create_dir_all(&project).expect("project");

    let mut command = kernary_without_test_model();
    command
        .current_dir(&project)
        .env("KERNARY_PROVIDER_CONFIG", &global_provider)
        .env("KERNARY_GLOBAL_MODEL_CONFIG", &global_model)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command.spawn().expect("window before provider exists");
    thread::sleep(Duration::from_millis(400));
    std::fs::create_dir_all(global_provider.parent().expect("global parent"))
        .expect("global parent");
    std::fs::write(
        &global_provider,
        r#"
schema_version = 1

[[providers]]
id = "late-relay"
display_name = "Late Relay"
credential_required = false
default_model = "late-coder"

[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://127.0.0.1:45679/v1/chat/completions"
models = ["late-coder"]
"#,
    )
    .expect("late global provider");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/provider switch\nlate-relay\n/status\n/exit\n")
        .expect("switch late provider");
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("late-relay/late-coder"), "stdout={stdout}");
    assert!(global_model.is_file());
}

#[test]
fn canonical_global_provider_and_model_survive_empty_process_path_overrides() {
    let temporary = tempdir().expect("tempdir");
    let project = temporary.path().join("project");
    let config_home = temporary.path().join("config-home");
    let canonical_directory = if cfg!(windows) {
        config_home.join("Kernary")
    } else {
        config_home.join("kernary")
    };
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&canonical_directory).expect("canonical directory");
    std::fs::write(
        canonical_directory.join("providers.toml"),
        r#"
schema_version = 1

[[providers]]
id = "canonical-relay"
display_name = "Canonical Relay"
credential_required = false
default_model = "canonical-coder"

[[providers.routes]]
protocol = "openai-chat"
endpoint = "http://127.0.0.1:45680/v1/chat/completions"
models = ["canonical-coder"]
"#,
    )
    .expect("canonical providers");
    std::fs::write(
        canonical_directory.join("model.json"),
        r#"{"schemaVersion":1,"providerId":"canonical-relay","modelId":"canonical-coder"}"#,
    )
    .expect("canonical model");

    let mut command = kernary_without_test_model();
    command
        .current_dir(&project)
        .env_remove("KERNARY_ISOLATE_GLOBAL_CONFIG")
        .env(
            "KERNARY_PROVIDER_CONFIG",
            temporary.path().join("override/missing-providers.toml"),
        )
        .env(
            "KERNARY_GLOBAL_MODEL_CONFIG",
            temporary.path().join("override/missing-model.json"),
        );
    if cfg!(windows) {
        command.env("APPDATA", &config_home);
    } else {
        command.env("XDG_CONFIG_HOME", &config_home);
    }
    command
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command.spawn().expect("canonical fallback process");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/provider switch\ncanonical-relay\n/status\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("canonical-relay/canonical-coder"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("15 providers | 1 custom"),
        "stdout={stdout}"
    );
}

#[test]
fn configured_remote_or_local_provider_is_lazy_until_user_text() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--model", "ollama/local-model", "--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/models\n/model\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("ollama/local-model"));
    assert!(stdout.contains("模型      ollama/local-model"));
    assert!(!stdout.contains("http-transport"));
}

#[test]
fn git_status_runs_through_allowlisted_process_tool() {
    let Some(git) = git_executable() else {
        return;
    };
    let temporary = tempdir().expect("tempdir");
    let initialized = Command::new(&git)
        .args(["init", "--quiet"])
        .current_dir(temporary.path())
        .status()
        .expect("git init");
    assert!(initialized.success());
    let mut child = harness()
        .current_dir(temporary.path())
        .env("HARNESS_GIT_EXECUTABLE", &git)
        .args([
            "--ui",
            "plain",
            "--ascii",
            "--permission-mode",
            "full",
            "--sandbox",
            "danger-full-access",
            "--confirm-dangerous-sandbox",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/git status\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("## No commits yet")
            || stdout.contains("## master")
            || stdout.contains("## main"),
        "stdout={stdout}"
    );
}

#[test]
fn review_command_creates_real_reviewer_agent_and_evidence() {
    let Some(git) = git_executable() else {
        return;
    };
    let temporary = tempdir().expect("tempdir");
    for arguments in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "harness@example.invalid"],
        vec!["config", "user.name", "Harness Test"],
    ] {
        assert!(
            Command::new(&git)
                .args(arguments)
                .current_dir(temporary.path())
                .status()
                .expect("git setup")
                .success()
        );
    }
    let file = temporary.path().join("review.txt");
    std::fs::write(&file, "before\n").expect("before");
    assert!(
        Command::new(&git)
            .args(["add", "review.txt"])
            .current_dir(temporary.path())
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new(&git)
            .args(["commit", "--quiet", "-m", "baseline"])
            .current_dir(temporary.path())
            .status()
            .expect("git commit")
            .success()
    );
    std::fs::write(&file, "after\n").expect("after");
    let mut child = harness()
        .current_dir(temporary.path())
        .env("HARNESS_GIT_EXECUTABLE", &git)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/review\n")
        .expect("review");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Review Agent created"), "stdout={stdout}");
    assert!(stdout.contains("1 accepted"), "stdout={stdout}");
    let connection = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agents");
    let evidence: String = connection
        .query_row(
            "SELECT json_extract(result_json,'$.evidence[0].kind') FROM agent_results LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("review evidence");
    assert_eq!(evidence, "review");
}

#[test]
fn mcp_and_skill_metadata_are_lazy_in_plain_terminal() {
    let temporary = tempdir().expect("tempdir");
    let mcp_config = temporary.path().join("mcp.toml");
    let missing_server = temporary.path().join("missing-mcp-server");
    let escaped_command = missing_server.display().to_string().replace('\\', "\\\\");
    std::fs::write(
        &mcp_config,
        format!(
            r#"[[servers]]
id = "broken"
name = "Lazy Broken"
enabled = true
trustAnnotations = false

[servers.transport]
kind = "stdio"
command = "{escaped_command}"
args = []
"#
        ),
    )
    .expect("mcp config");
    let skills_root = temporary.path().join("skills");
    let skill = skills_root.join("demo");
    std::fs::create_dir_all(&skill).expect("skill dir");
    std::fs::write(skill.join("SKILL.md"), "lazy terminal skill").expect("prompt");
    std::fs::write(
        skill.join("skill.toml"),
        r#"id = "terminal_skill"
name = "Terminal Skill"
version = "1.0.0"
description = "terminal lazy skill"
entry = "SKILL.md"
"#,
    )
    .expect("skill manifest");
    let mut child = harness()
        .current_dir(temporary.path())
        .env("HARNESS_MCP_CONFIG", &mcp_config)
        .env("HARNESS_SKILL_DIRS", &skills_root)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/mcp\n/skills\n/skills load terminal_skill\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("broken | Disconnected | stdio"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("terminal_skill | 1.0.0 | MetadataOnly"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Skill terminal_skill | loaded"),
        "stdout={stdout}"
    );
    assert!(
        !missing_server.exists(),
        "listing MCP metadata must not spawn or create executable"
    );
}

#[test]
fn agent_md_project_override_and_private_git_excludes_are_enforced() {
    let project = tempdir().expect("project");
    let global = tempdir().expect("global");
    std::fs::write(global.path().join("agent.md"), "GLOBAL-INSTRUCTION").expect("global agent md");
    if let Some(git) = git_executable() {
        let initialized = Command::new(git)
            .args(["init", "--quiet"])
            .current_dir(project.path())
            .status()
            .expect("git init");
        assert!(initialized.success());
    }

    let run = |expected: &str| {
        let mut child = kernary()
            .current_dir(project.path())
            .env("KERNARY_HOME", global.path())
            .args(["--ui", "plain", "--ascii"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"/agentmd status\n/agentmd show\n/exit\n")
            .expect("commands");
        let output = child.wait_with_output().expect("output");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("stdout");
        assert!(stdout.contains(expected), "stdout={stdout}");
        stdout
    };

    let global_output = run("GLOBAL-INSTRUCTION");
    assert!(global_output.contains("scope=global"));
    std::fs::write(
        project.path().join(".harness/agent.md"),
        "PROJECT-INSTRUCTION",
    )
    .expect("project agent md");
    let project_output = run("PROJECT-INSTRUCTION");
    assert!(project_output.contains("scope=project"));
    assert!(!project_output.contains("GLOBAL-INSTRUCTION"));
    assert!(!project.path().join("agent.md").exists());

    let exclude = project.path().join(".git/info/exclude");
    if exclude.is_file() {
        let exclude = std::fs::read_to_string(exclude).expect("exclude");
        assert!(exclude.lines().any(|line| line.trim() == "/.harness/"));
        assert!(
            exclude
                .lines()
                .any(|line| line.trim() == "/kernary.vector.toml")
        );
        assert!(exclude.lines().any(|line| line.trim() == "/agent.md"));
    }
}

#[test]
fn memory_repository_and_vector_off_commands_work_without_embedding_model() {
    let temporary = tempdir().expect("tempdir");
    std::fs::create_dir_all(temporary.path().join("src")).expect("src");
    std::fs::write(
        temporary.path().join("src/auth.rs"),
        "pub struct ApprovalGate;\npub fn approve() {}\n",
    )
    .expect("source");
    let mut child = harness()
        .current_dir(temporary.path())
        .env_remove("HARNESS_EMBEDDING_MODEL")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/memory add decision | Approval policy | External effects need approval | security\n/memory search lexical Approval policy\n/vector status\n/index update\n/index search ApprovalGate\n/index status\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Memory added memory:"), "stdout={stdout}");
    assert!(
        stdout.contains("Memory Lexical -> Lexical"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Vector Absent"), "stdout={stdout}");
    assert!(stdout.contains("Vector Schema    false"), "stdout={stdout}");
    assert!(stdout.contains("src/auth.rs"), "stdout={stdout}");
    let connection = rusqlite::Connection::open(temporary.path().join(".harness/memory.sqlite"))
        .expect("memory db");
    let vector_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("schema query");
    assert!(vector_table.is_none());
    assert!(!temporary.path().join(".harness/vector").exists());
}

#[test]
fn legacy_project_vector_config_migrates_to_global_and_project_data_stays_local() {
    let temporary = tempdir().expect("tempdir");
    let state = temporary.path().join(".harness");
    let global_config = temporary.path().join("global/vector.toml");
    std::fs::create_dir_all(&state).expect("state");
    VectorProviderConfig::new("http://127.0.0.1:9/v1/embeddings", "embed-private", 16, 42)
        .expect("config")
        .save(state.join("vector.toml"))
        .expect("save private vector config");
    let mut child = kernary()
        .current_dir(temporary.path())
        .env("KERNARY_GLOBAL_VECTOR_CONFIG", &global_config)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/vector status\n/session new\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Startup Health"), "stdout={stdout}");
    assert!(stdout.contains("Vector global | 不可用"), "stdout={stdout}");
    assert!(global_config.is_file());
    assert!(state.join("vector.toml").is_file());
    assert!(state.join("memory.sqlite").is_file());
    assert!(!temporary.path().join("kernary.vector.toml").exists());

    let second_project = tempdir().expect("second project");
    let second_state = second_project.path().join(".harness");
    std::fs::create_dir_all(&second_state).expect("second state");
    VectorProviderConfig::new(
        "http://127.0.0.1:9/v1/embeddings",
        "must-not-replace-global",
        32,
        43,
    )
    .expect("second config")
    .save(second_state.join("vector.toml"))
    .expect("save second legacy config");
    let mut second = kernary()
        .current_dir(second_project.path())
        .env("KERNARY_GLOBAL_VECTOR_CONFIG", &global_config)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("second project");
    second
        .stdin
        .as_mut()
        .expect("second stdin")
        .write_all(b"/exit\n")
        .expect("second exit");
    assert!(
        second
            .wait_with_output()
            .expect("second output")
            .status
            .success()
    );
    assert_eq!(
        VectorCatalogConfig::load(&global_config)
            .expect("load global")
            .resolved_active()
            .expect("resolve global")
            .expect("global config")
            .1
            .model,
        "embed-private"
    );
}

#[test]
fn layered_config_mode_settings_and_vector_preference_persist_without_breaking_hard_gate() {
    let temporary = tempdir().expect("tempdir");
    std::fs::write(
        temporary.path().join("kernary.toml"),
        "schema_version = 1\n[settings]\nmode = \"lite\"\n[settings.ui]\nstatusbar = true\n",
    )
    .expect("project config");
    let mut child = kernary()
        .current_dir(temporary.path())
        .env_remove("KERNARY_EMBEDDING_MODEL")
        .env_remove("HARNESS_EMBEDDING_MODEL")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/config\n/mode full\n/settings set ui.statusbar false runtime\n/vector on\n/settings vector\n/vector status\n/exit\n",
        )
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("mode=lite | source=project"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("mode=full | source=session"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("ui.statusbar=false | source=runtime"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("vector.mode=on | source=session"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Vector Absent"), "stdout={stdout}");

    let connection = rusqlite::Connection::open(temporary.path().join(".harness/memory.sqlite"))
        .expect("memory db");
    let vector_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("schema query");
    assert!(vector_table.is_none());
    drop(connection);

    let mut recovered = kernary()
        .current_dir(temporary.path())
        .env_remove("KERNARY_EMBEDDING_MODEL")
        .env_remove("HARNESS_EMBEDDING_MODEL")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("recover");
    recovered
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/mode\n/settings ui.statusbar\n/settings vector.mode\n/exit\n")
        .expect("write");
    let output = recovered.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("mode=full | source=session"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("ui.statusbar=true | source=project"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("vector.mode=on | source=session"),
        "stdout={stdout}"
    );
    assert!(!temporary.path().join(".harness/vector").exists());
}

#[test]
fn permission_modes_are_durable_visible_and_keep_sandbox_hard_boundaries() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/permissions safe\n/permissions\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("permissions.mode=safe | source=session"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Approval policy: Always"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Sandbox hard denies")
            && stdout.contains("only bypass removes the WorkspacePatch confirmation"),
        "stdout={stdout}"
    );

    let mut recovered = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("recover");
    recovered
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/permissions\n/exit\n")
        .expect("write");
    let output = recovered.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("permissions.mode=safe | source=session"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Approval policy: Always"),
        "stdout={stdout}"
    );
}

#[test]
fn cli_permission_levels_require_explicit_bypass_confirmation() {
    let temporary = tempdir().expect("tempdir");
    let denied = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--permission-mode", "bypass"])
        .output()
        .expect("unconfirmed bypass");
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--confirm-bypass"));

    let prompt_project = tempdir().expect("prompt project");
    let mut prompted = kernary()
        .current_dir(prompt_project.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("prompted bypass");
    prompted
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/permissions bypass\nI UNDERSTAND BYPASS\n/permissions\n/exit\n")
        .expect("bypass confirmation");
    let prompted = prompted.wait_with_output().expect("prompted output");
    assert!(prompted.status.success());
    let prompted_stdout = String::from_utf8(prompted.stdout).expect("stdout");
    assert!(prompted_stdout.contains("Setup: 确认最高权限模式"));
    assert!(prompted_stdout.contains("BypassWithinSandbox"));

    for (mode, policy, extra) in [
        ("manual", "Always", false),
        ("edit", "CommandsOnly", false),
        ("auto", "OnRequest", false),
        ("full", "NeverWithinSandbox", false),
        ("bypass", "BypassWithinSandbox", true),
    ] {
        let mode_project = tempdir().expect("mode project");
        let mut command = kernary();
        command
            .current_dir(mode_project.path())
            .args(["--ui", "plain", "--ascii", "--permission-mode", mode])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if extra {
            command.arg("--confirm-bypass");
        }
        let mut child = command.spawn().expect("mode process");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"/permissions\n/exit\n")
            .expect("commands");
        let output = child.wait_with_output().expect("mode output");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("stdout");
        assert!(stdout.contains(policy), "mode={mode} stdout={stdout}");
    }
}

#[test]
fn sandbox_danger_and_network_access_require_explicit_confirmation() {
    let dangerous = tempdir().expect("danger project");
    let denied = kernary()
        .current_dir(dangerous.path())
        .args(["--sandbox", "danger-full-access"])
        .output()
        .expect("unconfirmed danger");
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--confirm-dangerous-sandbox"));

    let mut confirmed = kernary()
        .current_dir(dangerous.path())
        .args([
            "--ui",
            "plain",
            "--ascii",
            "--sandbox",
            "danger-full-access",
            "--confirm-dangerous-sandbox",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("confirmed danger");
    confirmed
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/sandbox\n/exit\n")
        .expect("commands");
    let output = confirmed.wait_with_output().expect("danger output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Sandbox mode: danger-full-access"));
    assert!(stdout.contains("Filesystem boundary: unrestricted"));

    let network = tempdir().expect("network project");
    let mut child = kernary()
        .current_dir(network.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("network prompt");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/settings set sandbox.network-access true session\n/sandbox network-on\nI UNDERSTAND NETWORK ACCESS\n/sandbox\n/exit\n",
        )
        .expect("network commands");
    let output = child.wait_with_output().expect("network output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("请使用 `/sandbox network-on`"));
    assert!(stdout.contains("sandbox.network-access=true"));
    if cfg!(any(windows, target_os = "linux")) {
        assert!(stdout.contains("Network boundary: allowed"));
    } else {
        assert!(stdout.contains("Network boundary: unavailable"));
    }

    let resumed = kernary()
        .current_dir(network.path())
        .arg("-c")
        .output()
        .expect("resume without network confirmation");
    assert!(!resumed.status.success());
    assert!(String::from_utf8_lossy(&resumed.stderr).contains("--sandbox-network-access"));
}

#[test]
fn permission_rules_are_strict_persistent_listable_and_removable() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/permissions rule add deny read ./secret/**\n/permissions rules\n/permissions\n/exit\n",
        )
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    let rule_id = stdout
        .lines()
        .find_map(|line| line.split_once("Permission rule ").map(|(_, tail)| tail))
        .and_then(|tail| tail.split_whitespace().next())
        .expect("rule id")
        .to_owned();
    assert!(stdout.contains("Deny Read ./secret/**"), "stdout={stdout}");
    assert!(stdout.contains("Permission rules: 1"), "stdout={stdout}");
    let config_path = temporary.path().join("kernary.permissions.toml");
    let config = std::fs::read_to_string(&config_path).expect("rules config");
    assert!(config.contains(&rule_id));
    assert!(config.contains("effect = \"deny\""));

    let mut recovered = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("recover");
    recovered
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            format!(
                "/permissions rules\n/permissions rule remove {rule_id}\n/permissions rules\n/exit\n"
            )
            .as_bytes(),
        )
        .expect("write");
    let output = recovered.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains(&rule_id), "stdout={stdout}");
    assert!(stdout.contains("removed=true"), "stdout={stdout}");
    assert!(
        stdout.contains("No custom Permission rules"),
        "stdout={stdout}"
    );
    let config = std::fs::read_to_string(config_path).expect("updated rules");
    assert!(!config.contains(&rule_id));
}

#[test]
fn cost_budget_and_explicit_failover_control_are_real_and_cost_gated() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/budget cost 1\n/team create 2 constrained research\n/failover on --confirm-cost fake/deterministic\n/failover\n/failover off\n/exit\n",
        )
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("costUnits=1"), "stdout={stdout}");
    assert!(
        stdout.contains("agent-cost-budget-exhausted"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("failover.enabled=true | source=runtime"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Failover enabled=true costConfirmed=true targets=fake/deterministic"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Failover enabled=false"), "stdout={stdout}");
}

#[test]
fn mcp_add_enable_disable_remove_is_lazy_persistent_and_atomic() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/mcp add-http docs https://example.com/mcp\n/mcp disable docs\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("MCP docs | added | enabled=true | streamable-http | persisted="),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("MCP docs | enabled=false"),
        "stdout={stdout}"
    );
    let config_path = temporary.path().join("kernary.mcp.toml");
    let config = std::fs::read_to_string(&config_path).expect("persisted MCP config");
    assert!(config.contains("id = \"docs\""));
    assert!(config.contains("enabled = false"));
    assert!(!temporary.path().join(".harness/vector").exists());

    let mut recovered = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("recover");
    recovered
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/mcp\n/mcp enable docs\n/mcp remove docs\n/mcp\n/exit\n")
        .expect("write");
    let output = recovered.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("docs | Disconnected | streamable-http | enabled=false"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("MCP docs | enabled=true"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("MCP docs | removed=true"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("No configured MCP servers"),
        "stdout={stdout}"
    );
    let config = std::fs::read_to_string(config_path).expect("updated MCP config");
    assert!(!config.contains("id = \"docs\""));
}

#[test]
fn observability_trace_profile_why_inspect_and_runtime_doctor_are_real_and_read_only() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .env_remove("KERNARY_EMBEDDING_MODEL")
        .env_remove("HARNESS_EMBEDDING_MODEL")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/trace on\n/goal set explain observability\n/why\n/profile\n/debug\n/inspect context\n/settings model\n/settings vector\n/settings mcp\n/settings plugin\n/settings permissions\n/settings sandbox\n/settings browser\n/settings performance\n/doctor\n/logs 20\n/trace last 20\n/exit\n",
        )
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("trace.enabled=true | source=runtime"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Why | auditable evidence summary"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Profile uptime="), "stdout={stdout}");
    assert!(stdout.contains("startup | count=1"), "stdout={stdout}");
    assert!(stdout.contains("context-build | count="), "stdout={stdout}");
    assert!(
        stdout.contains("Debug snapshot | read-only"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Doctor | runtime diagnostics"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Embedding Model  <none>"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Backend          disabled"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Index Chat       off"), "stdout={stdout}");
    assert!(stdout.contains("MCP servers=0"), "stdout={stdout}");
    assert!(stdout.contains("Plugins discovered="), "stdout={stdout}");
    assert!(
        stdout.contains("Sandbox mode: workspace-write"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Platform backend:"), "stdout={stdout}");
    assert!(stdout.contains("Vector semantic=Absent"), "stdout={stdout}");
    assert!(stdout.contains("Cache l1="), "stdout={stdout}");
    assert!(
        stdout.contains("#") && stdout.contains("goal.changed"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("trace=trace:"), "stdout={stdout}");
    assert!(!stdout.to_lowercase().contains("chain-of-thought:"));

    let connection = rusqlite::Connection::open(temporary.path().join(".harness/memory.sqlite"))
        .expect("memory db");
    let vector_table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("schema query");
    assert!(vector_table.is_none());
}

#[test]
fn session_goal_reset_and_forget_control_plane_is_durable_and_recoverable() {
    let temporary = tempdir().expect("tempdir");
    let mut child = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"/goal set first goal\n/goal edit second goal\n/goal history\nrun one task\n/pin preserve this\n/reset\n/checkpoint fork-list\n/fork fork-list session:child-control\n/sessions\n/goal clear\n/goal history\n/memory add lesson | Forget me | temporary lesson | test\n/exit\n",
        )
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("Goal history | 2 revision(s)"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Context reset | checkpoint="),
        "stdout={stdout}"
    );
    assert!(stdout.contains("retained="), "stdout={stdout}");
    assert!(stdout.contains("Sessions | 2"), "stdout={stdout}");
    assert!(stdout.contains("[current]"), "stdout={stdout}");
    assert!(!stdout.contains("session:child-control"), "stdout={stdout}");
    assert!(stdout.contains("未命名会话 | #"), "stdout={stdout}");
    assert!(stdout.contains("目标  <empty>"), "stdout={stdout}");
    assert!(
        stdout.contains("Goal history | 2 revision(s)"),
        "stdout={stdout}"
    );
    let memory_id = stdout
        .lines()
        .find_map(|line| line.split_once("Memory added ").map(|(_, tail)| tail))
        .and_then(|tail| tail.split_whitespace().next())
        .expect("memory id")
        .to_owned();

    let mut forget = kernary()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("forget spawn");
    forget
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("/forget {memory_id}\n/memory stats\n/exit\n").as_bytes())
        .expect("write");
    let output = forget.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains(&format!("Memory {memory_id} deleted=true")),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Memory records=0"), "stdout={stdout}");
}

#[test]
fn agent_control_plane_is_sleeping_inspectable_and_project_local() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/team\n/agent agent:coordinator\n/queue\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Team 30 | sleeping 30"), "stdout={stdout}");
    assert!(
        stdout.contains("messages=true | leases=true"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("agent:coordinator"), "stdout={stdout}");
    assert!(
        stdout.contains("Profile    coordinator-v1"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("SOP"), "stdout={stdout}");
    assert!(stdout.contains("Evidence Contract"), "stdout={stdout}");
    assert!(stdout.contains("Failure Policy"), "stdout={stdout}");
    assert!(stdout.contains("Completion Gate"), "stdout={stdout}");
    for specialist in [
        "agent:requirements",
        "agent:explorer",
        "agent:architect",
        "agent:security",
        "agent:performance",
        "agent:release",
    ] {
        assert!(
            stdout.contains(specialist),
            "missing {specialist}: {stdout}"
        );
    }
    assert!(
        stdout.contains("control-plane / 禁止编码"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("Agent queue: no active mission"),
        "stdout={stdout}"
    );
    assert!(temporary.path().join(".harness/agents.sqlite").is_file());
    assert!(
        temporary
            .path()
            .join(".harness/file-leases.sqlite")
            .is_file()
    );
}

#[test]
fn browser_unconfigured_is_lazy_and_starts_no_process_or_browser_store() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .env_remove("HARNESS_BROWSER_PYTHON")
        .env_remove("HARNESS_BROWSER_EXECUTABLE")
        .env_remove("HARNESS_BROWSER_ALLOWED_ORIGINS")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/browser status\n/tools\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Browser unavailable | no process started"));
    assert!(!stdout.contains("browser.snapshot@1"));
    assert!(!temporary.path().join(".harness/browser.sqlite").exists());
    assert!(!temporary.path().join(".harness/browser").exists());
}

#[test]
fn lsp_unconfigured_or_invalid_is_isolated_and_starts_no_process() {
    for config in [
        None,
        Some(
            "schema_version = 1\n[[servers]]\nid = \"rust\"\ncommand = \"rust-analyzer\"\nlanguage_ids = { rs = \"rust\" }\n",
        ),
    ] {
        let temporary = tempdir().expect("tempdir");
        if let Some(config) = config {
            std::fs::write(temporary.path().join("kernary.lsp.toml"), config).expect("lsp config");
        }
        let mut child = harness()
            .current_dir(temporary.path())
            .args(["--ui", "plain", "--ascii"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"/lsp\n/exit\n")
            .expect("commands");
        let output = child.wait_with_output().expect("wait");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("stdout");
        assert!(stdout.contains("lsp-runtime-missing"), "stdout={stdout}");
        assert!(!temporary.path().join("started.marker").exists());
        assert!(!temporary.path().join(".harness/lsp").exists());
    }
}

#[test]
fn lsp_tool_metadata_is_on_demand_and_server_remains_sleeping() {
    let temporary = tempdir().expect("tempdir");
    let executable = env!("CARGO_BIN_EXE_harness").replace('\\', "/");
    std::fs::write(
        temporary.path().join("kernary.lsp.toml"),
        format!(
            "schema_version = 1\n[[servers]]\nid = \"self-test\"\ncommand = \"{executable}\"\nargs = [\"--version\"]\nlanguage_ids = {{ rs = \"rust\" }}\n"
        ),
    )
    .expect("config");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/tools\n/lsp\n/exit\n")
        .expect("commands");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    for tool in [
        "lsp.symbols@1",
        "lsp.definition@1",
        "lsp.references@1",
        "lsp.diagnostics@1",
        "lsp.rename.preview@1",
        "lsp.code-action.preview@1",
        "lsp.patch.apply@1",
        "lsp.patch.undo@1",
    ] {
        assert!(stdout.contains(tool), "tool={tool}, stdout={stdout}");
    }
    assert!(
        !stdout.contains("process.exec@1"),
        "LSP 配置不能顺带暴露通用进程工具：stdout={stdout}"
    );
    assert!(stdout.contains("self-test | sleeping"), "stdout={stdout}");
    assert!(!temporary.path().join(".harness/lsp-previews").exists());
}

#[test]
fn budget_steering_and_queue_priority_are_wired_to_durable_runtime() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/budget\n/budget parallel 2\nimplement queue controls\n/steer do not change schema\n/queue priority task:main 70\n/queue\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("parallel=3"), "stdout={stdout}");
    assert!(stdout.contains("parallel=2"), "stdout={stdout}");
    assert!(
        stdout.contains("Steering agent-message:"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("priority=70"), "stdout={stdout}");
    let connection = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agent db");
    let (kind, payload): (String, String) = connection
        .query_row(
            "SELECT kind_json,payload_json FROM agent_messages WHERE recipient='kernel:supervisor:steering' LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("steering message");
    assert_eq!(kind, "\"steering\"");
    assert!(payload.contains("do not change schema"));
    let priority: i32 = connection
        .query_row(
            "SELECT priority FROM agent_task_controls WHERE task_id='task:main'",
            [],
            |row| row.get(0),
        )
        .expect("durable priority");
    assert_eq!(priority, 70);
}

#[test]
fn two_four_and_eight_agent_teams_complete_through_kernel_outbox_and_agent_store() {
    for count in [2, 4, 8] {
        let temporary = tempdir().expect("tempdir");
        let mut child = harness()
            .current_dir(temporary.path())
            .env_remove("HARNESS_EMBEDDING_MODEL")
            .args(["--ui", "plain", "--ascii"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(
                format!("/team create {count} analyze auth architecture\n/queue\n").as_bytes(),
            )
            .expect("write");
        let output = child.wait_with_output().expect("wait");
        assert!(
            output.status.success(),
            "count={count}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("stdout");
        assert!(
            stdout.contains(&format!("{count} accepted")),
            "count={count}, stdout={stdout}"
        );
        let connection =
            rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
                .expect("agent db");
        for (table, status) in [
            ("agent_sessions", "completed"),
            ("agent_budget_escrows", "completed"),
        ] {
            let actual: usize = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE status=?1"),
                    [status],
                    |row| row.get(0),
                )
                .expect("count status");
            assert_eq!(actual, count, "table={table}");
        }
        let results: usize = connection
            .query_row("SELECT COUNT(*) FROM agent_results", [], |row| row.get(0))
            .expect("result count");
        assert_eq!(results, count);
        let endpoint_json: String = connection
            .query_row(
                "SELECT state_json FROM agent_endpoints WHERE id='endpoint:agent:researcher'",
                [],
                |row| row.get(0),
            )
            .expect("researcher endpoint");
        let endpoint: serde_json::Value =
            serde_json::from_str(&endpoint_json).expect("endpoint json");
        assert_eq!(endpoint["status"], "idle");
        assert_eq!(endpoint["activeRuns"], 0);
    }
}

#[test]
fn background_team_accepts_inflight_steering_and_task_cancellation() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .env("HARNESS_FAKE_EVENT_DELAY_MILLIS", "25")
        .env_remove("HARNESS_EMBEDDING_MODEL")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/team create 4 analyze auth concurrently\n/steer do not change database schema\n/queue cancel task:research:03\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    let started = stdout.find("Background Team started").expect("started");
    let steered = stdout.find("Steering agent-message:").expect("steering");
    let cancelled = stdout
        .find("Cancellation requested for task:research:03")
        .expect("cancel requested");
    let finished = stdout
        .find("Background Team paused")
        .expect("paused after task cancellation");
    assert!(started < steered && steered < finished);
    assert!(started < cancelled && cancelled < finished);

    let connection = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agent db");
    let completed_sessions: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_sessions WHERE status='completed'",
            [],
            |row| row.get(0),
        )
        .expect("completed sessions");
    let cancelled_sessions: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_sessions WHERE status='cancelled'",
            [],
            |row| row.get(0),
        )
        .expect("cancelled sessions");
    assert_eq!(completed_sessions, 3);
    assert_eq!(cancelled_sessions, 1);
    let cancelled_budgets: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_budget_escrows WHERE status='cancelled'",
            [],
            |row| row.get(0),
        )
        .expect("cancelled budgets");
    assert_eq!(cancelled_budgets, 1);
    let steering_messages: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE kind_json='\"steering\"' AND acknowledged_at_millis IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("steering message");
    assert_eq!(steering_messages, 1);
}

#[test]
fn background_team_cancel_reconciles_all_runs_before_plain_exit() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .env("HARNESS_FAKE_EVENT_DELAY_MILLIS", "25")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/team create 4 cancel safely\n/exit\n")
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("Cancellation requested for active Team"));
    assert!(stdout.contains("Background Team paused"));
    assert!(stdout.contains("cancelled"));

    let agents = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agent db");
    for table in ["agent_sessions", "agent_budget_escrows"] {
        let cancelled: usize = agents
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE status='cancelled'"),
                [],
                |row| row.get(0),
            )
            .expect("cancelled count");
        assert_eq!(cancelled, 4, "table={table}");
    }
    let kernel = rusqlite::Connection::open(temporary.path().join(".harness/kernel.sqlite"))
        .expect("kernel db");
    let cancelled_events: usize = kernel
        .query_row(
            "SELECT COUNT(*) FROM aggregate_events WHERE aggregate_kind='mission' AND event_type='mission.cancelled'",
            [],
            |row| row.get(0),
        )
        .expect("mission cancelled event");
    assert_eq!(cancelled_events, 1);
}

#[test]
fn crashed_background_team_resumes_after_start_effect_lease_expires() {
    let temporary = tempdir().expect("tempdir");
    let mut crashed = harness()
        .current_dir(temporary.path())
        .env("HARNESS_FAKE_EVENT_DELAY_MILLIS", "1000")
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn crash target");
    crashed
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/team create 2 recover after crash\n")
        .expect("start team");
    crashed
        .stdin
        .as_mut()
        .expect("stdin")
        .flush()
        .expect("flush");

    let kernel_path = temporary.path().join(".harness/kernel.sqlite");
    let agent_path = temporary.path().join(".harness/agents.sqlite");
    let mut prepared = false;
    for _ in 0..150 {
        if kernel_path.is_file()
            && agent_path.is_file()
            && let Ok(connection) = rusqlite::Connection::open(&agent_path)
            && let Ok(count) = connection.query_row(
                "SELECT COUNT(*) FROM agent_sessions WHERE status='running'",
                [],
                |row| row.get::<_, usize>(0),
            )
            && count == 2
        {
            prepared = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        prepared,
        "background Team did not reach durable Running state"
    );
    crashed.kill().expect("kill crash target");
    let _ = crashed.wait();

    let kernel = rusqlite::Connection::open(&kernel_path).expect("kernel db");
    let changed = kernel
        .execute(
            "UPDATE outbox SET lease_expires_at_millis=0 WHERE status='claimed'",
            [],
        )
        .expect("expire abandoned leases");
    assert_eq!(changed, 2);
    drop(kernel);

    let mut resumed = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn resume");
    resumed
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/resume\n")
        .expect("resume command");
    let output = resumed.wait_with_output().expect("wait resume");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("recovered=true"), "stdout={stdout}");
    assert!(
        stdout.contains("Background Team finished"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("2 accepted"), "stdout={stdout}");
    let agents = rusqlite::Connection::open(agent_path).expect("agent db");
    let completed: usize = agents
        .query_row(
            "SELECT COUNT(*) FROM agent_sessions WHERE status='completed'",
            [],
            |row| row.get(0),
        )
        .expect("completed sessions");
    assert_eq!(completed, 2);
}

#[test]
fn role_workflow_runs_planner_parallel_coders_reviewer_and_tester_with_evidence() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/team workflow 2 implement auth safely\n")
        .expect("workflow");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("workflow-workers=2"), "stdout={stdout}");
    assert!(stdout.contains("next-wave=true"), "stdout={stdout}");
    assert!(stdout.contains("5 accepted"), "stdout={stdout}");

    let connection = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agent db");
    for (role, expected) in [("planner", 1), ("coder", 2), ("reviewer", 1), ("tester", 1)] {
        let actual: usize = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_sessions WHERE status='completed' AND json_extract(state_json,'$.role')=?1",
                [role],
                |row| row.get(0),
            )
            .expect("role count");
        assert_eq!(actual, expected, "role={role}");
    }
    for (role, evidence_kind) in [("reviewer", "review"), ("tester", "test")] {
        let actual: String = connection
            .query_row(
                "SELECT json_extract(r.result_json,'$.evidence[0].kind') FROM agent_results r JOIN agent_sessions s ON s.id='agent-session:' || r.run_id WHERE json_extract(s.state_json,'$.role')=?1",
                [role],
                |row| row.get(0),
            )
            .expect("evidence kind");
        assert_eq!(actual, evidence_kind);
    }
    let completed_budgets: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_budget_escrows WHERE status='completed'",
            [],
            |row| row.get(0),
        )
        .expect("budgets");
    assert_eq!(completed_budgets, 5);
}

#[test]
fn adaptive_workflow_routes_specialists_and_persists_all_evidence_gates() {
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"/team adaptive 2 release secure auth service with performance benchmark\n")
        .expect("adaptive workflow");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(stdout.contains("adaptive-workers=2"), "stdout={stdout}");
    assert!(stdout.contains("next-wave=true"), "stdout={stdout}");
    assert!(stdout.contains("11 accepted"), "stdout={stdout}");

    let connection = rusqlite::Connection::open(temporary.path().join(".harness/agents.sqlite"))
        .expect("agent db");
    for (role, expected) in [
        ("requirements-analyst", 1),
        ("explorer", 1),
        ("architect", 1),
        ("planner", 1),
        ("coder", 2),
        ("reviewer", 1),
        ("security-auditor", 1),
        ("performance-engineer", 1),
        ("tester", 1),
        ("release-manager", 1),
    ] {
        let actual: usize = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_sessions WHERE status='completed' AND json_extract(state_json,'$.role')=?1",
                [role],
                |row| row.get(0),
            )
            .expect("role count");
        assert_eq!(actual, expected, "role={role}");
    }
    for evidence_kind in [
        "requirements",
        "exploration",
        "architecture",
        "review",
        "security",
        "performance",
        "test",
        "release",
    ] {
        let actual: usize = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_results WHERE EXISTS (SELECT 1 FROM json_each(agent_results.result_json, '$.evidence') WHERE json_extract(value, '$.kind')=?1)",
                [evidence_kind],
                |row| row.get(0),
            )
            .expect("evidence count");
        assert!(actual >= 1, "missing evidence={evidence_kind}");
    }
}

#[test]
#[ignore = "设置 HARNESS_BROWSER_E2E=1 后运行本机 Playwright/Chrome CLI E2E"]
fn browser_cli_is_lazy_then_opens_navigates_journals_and_closes() {
    if std::env::var("HARNESS_BROWSER_E2E").ok().as_deref() != Some("1") {
        return;
    }
    let python = std::env::var_os("HARNESS_BROWSER_PYTHON").expect("python");
    let browser = std::env::var_os("HARNESS_BROWSER_EXECUTABLE").expect("browser");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    let running = Arc::new(AtomicBool::new(true));
    let server_running = running.clone();
    let server = thread::spawn(move || {
        let html = b"<!doctype html><title>CLI Browser</title><h1>ready</h1>";
        while server_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 2048];
                    let _ = stream.read(&mut request);
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        html.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(html);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    let temporary = tempdir().expect("tempdir");
    let mut child = harness()
        .current_dir(temporary.path())
        .env("HARNESS_BROWSER_PYTHON", python)
        .env("HARNESS_BROWSER_EXECUTABLE", browser)
        .env("HARNESS_BROWSER_ALLOWED_ORIGINS", &origin)
        .args(["--ui", "plain", "--ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            format!(
                "/browser status\n/tools\n/browser open\n/browser navigate {origin}/\n/browser actions\n/browser close\n/exit\n"
            )
            .as_bytes(),
        )
        .expect("commands");
    let output = child.wait_with_output().expect("wait");
    running.store(false, Ordering::SeqCst);
    server.join().expect("server");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("Browser browser:default | Closed | alive=false"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("browser.snapshot@1"), "stdout={stdout}");
    assert!(
        stdout.contains("Browser browser:default | Ready | alive=true"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains(&format!("Origin {origin}")),
        "stdout={stdout}"
    );
    assert!(stdout.contains("Navigate | Completed"), "stdout={stdout}");
    assert!(stdout.contains("Browser browser:default | Closed | alive=false"));
    let connection = rusqlite::Connection::open(temporary.path().join(".harness/browser.sqlite"))
        .expect("browser db");
    let completed: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM browser_actions WHERE status='completed'",
            [],
            |row| row.get(0),
        )
        .expect("actions");
    assert_eq!(completed, 1);
}
