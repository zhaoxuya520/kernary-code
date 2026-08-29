//! 把 BrowserRuntime 映射为按需加载的 typed Tool；这里不暴露原始 CDP。

use std::path::PathBuf;
use std::sync::Arc;

use harness_permission::{BrowserAction, PermissionAction};
use harness_tool::{
    ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};
use harness_types::{BrowserActionId, ConfidentialityLabel};

use crate::{BrowserCommand, BrowserResult, BrowserRuntime, BrowserWait, origin_from_url};

#[derive(Clone, Copy)]
enum BrowserToolKind {
    Navigate,
    Snapshot,
    Click,
    Type,
    Read,
    Inspect,
    Wait,
    Screenshot,
    Upload,
    Download,
}

pub fn register_browser_tools(
    registry: &mut ToolRegistry,
    runtime: Arc<BrowserRuntime>,
) -> Result<(), ToolError> {
    // 只有 Browser 配置完整时组合根才调用本函数，因此未配置时 Tool Catalog 也是零污染。
    for (name, description, effect, keywords, kind, schema) in definitions() {
        registry.register(
            ToolDescriptor {
                canonical_name: name.to_owned(),
                version: "1".to_owned(),
                description: description.to_owned(),
                effect_class: effect,
                source: ToolSource::Internal,
                prompt_loading: ToolPromptLoading::OnDemand,
                keywords: keywords.iter().map(|value| (*value).to_owned()).collect(),
                input_schema: schema,
                output_schema: serde_json::json!({"type":"object"}),
            },
            Arc::new(BrowserTool {
                runtime: runtime.clone(),
                kind,
            }),
        )?;
    }
    Ok(())
}

type BrowserDefinition = (
    &'static str,
    &'static str,
    ToolEffectClass,
    &'static [&'static str],
    BrowserToolKind,
    serde_json::Value,
);

fn definitions() -> Vec<BrowserDefinition> {
    vec![
        (
            "browser.navigate",
            "导航到 Browser Session allowlist 内的 URL",
            ToolEffectClass::VerifiableEffect,
            &["browser", "navigate", "open", "url"],
            BrowserToolKind::Navigate,
            object_schema(&["url"]),
        ),
        (
            "browser.snapshot",
            "读取最多 500 个节点的结构化页面快照并刷新 refs",
            ToolEffectClass::ReadOnlyRetryable,
            &["browser", "snapshot", "accessibility", "elements"],
            BrowserToolKind::Snapshot,
            object_schema(&[]),
        ),
        (
            "browser.click",
            "点击最新 Snapshot 中的元素 ref",
            ToolEffectClass::VerifiableEffect,
            &["browser", "click", "button", "link"],
            BrowserToolKind::Click,
            object_schema(&["ref"]),
        ),
        (
            "browser.type",
            "向最新 Snapshot ref 输入非 UserSecret 文本",
            ToolEffectClass::VerifiableEffect,
            &["browser", "type", "fill", "input"],
            BrowserToolKind::Type,
            serde_json::json!({
                "type":"object",
                "properties":{
                    "ref":{"type":"string"},
                    "text":{"type":"string"},
                    "classification":{"type":"string","enum":["public","project-private"]}
                },
                "required":["ref","text"],
                "additionalProperties":false
            }),
        ),
        (
            "browser.read",
            "读取指定 ref 的可见文本或表单值",
            ToolEffectClass::ReadOnlyRetryable,
            &["browser", "read", "text", "value"],
            BrowserToolKind::Read,
            object_schema(&["ref"]),
        ),
        (
            "browser.inspect",
            "检查指定 ref 的角色、标签、过滤属性与边界",
            ToolEffectClass::ReadOnlyRetryable,
            &["browser", "inspect", "element", "attributes"],
            BrowserToolKind::Inspect,
            object_schema(&["ref"]),
        ),
        (
            "browser.wait",
            "等待毫秒、元素 ref 或页面 load state",
            ToolEffectClass::ReadOnlyRetryable,
            &["browser", "wait", "load", "element"],
            BrowserToolKind::Wait,
            serde_json::json!({
                "type":"object",
                "properties":{
                    "millis":{"type":"integer","minimum":1},
                    "ref":{"type":"string"},
                    "load":{"type":"string","enum":["load","domcontentloaded","networkidle"]}
                },
                "additionalProperties":false
            }),
        ),
        (
            "browser.screenshot",
            "保存当前视口 PNG Artifact",
            ToolEffectClass::ReadOnlyRetryable,
            &["browser", "screenshot", "image", "artifact"],
            BrowserToolKind::Screenshot,
            object_schema(&[]),
        ),
        (
            "browser.upload",
            "从显式 upload roots 选择文件上传到 ref",
            ToolEffectClass::VerifiableEffect,
            &["browser", "upload", "file"],
            BrowserToolKind::Upload,
            object_schema(&["ref", "path"]),
        ),
        (
            "browser.download",
            "点击 ref 并把下载保存到专属 download directory",
            ToolEffectClass::VerifiableEffect,
            &["browser", "download", "file"],
            BrowserToolKind::Download,
            object_schema(&["ref"]),
        ),
    ]
}

fn object_schema(required: &[&str]) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    for key in required {
        properties.insert((*key).to_owned(), serde_json::json!({"type":"string"}));
    }
    serde_json::json!({
        "type":"object",
        "properties":properties,
        "required":required,
        "additionalProperties":false
    })
}

struct BrowserTool {
    runtime: Arc<BrowserRuntime>,
    kind: BrowserToolKind,
}

impl ToolProvider for BrowserTool {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        parse_command(self.kind, value).map_err(browser_error)?;
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        serde_json::from_value::<BrowserResult>(value.clone())
            .map_err(|error| ToolError::new("browser-tool-result", error.to_string()))?;
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        let command = parse_command(self.kind, args).map_err(browser_error)?;
        let origin = match &command {
            BrowserCommand::Navigate { url } => origin_from_url(url).map_err(browser_error)?,
            _ => self.runtime.current_origin().map_err(browser_error)?,
        };
        Ok(match command {
            BrowserCommand::Navigate { .. } => PermissionAction::BrowserOpen { origin },
            BrowserCommand::Click { .. } => PermissionAction::BrowserAct {
                origin,
                action: BrowserAction::Click,
            },
            BrowserCommand::Type { .. } => PermissionAction::BrowserAct {
                origin,
                action: BrowserAction::Type,
            },
            BrowserCommand::Upload { path, .. } => PermissionAction::BrowserUpload { origin, path },
            BrowserCommand::Download { .. } => PermissionAction::BrowserDownload { origin },
            _ => PermissionAction::BrowserSnapshot { origin },
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new(
                "browser-tool-cancelled",
                input.invocation_id.to_string(),
            ));
        }
        let command = parse_command(self.kind, &input.args).map_err(browser_error)?;
        self.runtime
            .execute(
                BrowserActionId::from(input.invocation_id.to_string()),
                command,
                input.now_millis,
            )
            .map_err(browser_error)
            .and_then(|result| {
                serde_json::to_value(result)
                    .map_err(|error| ToolError::new("browser-tool-json", error.to_string()))
            })
    }
}

fn parse_command(
    kind: BrowserToolKind,
    value: &serde_json::Value,
) -> Result<BrowserCommand, crate::BrowserError> {
    let object = value
        .as_object()
        .ok_or_else(|| crate::BrowserError::new("browser-tool-args", "object required"))?;
    let string = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| crate::BrowserError::new("browser-tool-arg-missing", key))
    };
    Ok(match kind {
        BrowserToolKind::Navigate => BrowserCommand::Navigate {
            url: string("url")?,
        },
        BrowserToolKind::Snapshot => BrowserCommand::Snapshot,
        BrowserToolKind::Click => BrowserCommand::Click {
            ref_id: string("ref")?,
        },
        BrowserToolKind::Type => BrowserCommand::Type {
            ref_id: string("ref")?,
            text: string("text")?,
            classification: match object
                .get("classification")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("project-private")
            {
                "public" => ConfidentialityLabel::Public,
                "project-private" => ConfidentialityLabel::ProjectPrivate,
                _ => {
                    return Err(crate::BrowserError::new(
                        "browser-text-classification",
                        "public/project-private only",
                    ));
                }
            },
        },
        BrowserToolKind::Read => BrowserCommand::Read {
            ref_id: string("ref")?,
        },
        BrowserToolKind::Inspect => BrowserCommand::Inspect {
            ref_id: string("ref")?,
        },
        BrowserToolKind::Wait => {
            let wait =
                if let Some(millis) = object.get("millis").and_then(serde_json::Value::as_u64) {
                    BrowserWait::Millis { millis }
                } else if object.contains_key("ref") {
                    BrowserWait::Ref {
                        ref_id: string("ref")?,
                    }
                } else if object.contains_key("load") {
                    BrowserWait::Load {
                        state: string("load")?,
                    }
                } else {
                    return Err(crate::BrowserError::new(
                        "browser-wait-target-missing",
                        "millis/ref/load",
                    ));
                };
            BrowserCommand::Wait { wait }
        }
        BrowserToolKind::Screenshot => BrowserCommand::Screenshot,
        BrowserToolKind::Upload => BrowserCommand::Upload {
            ref_id: string("ref")?,
            path: PathBuf::from(string("path")?),
        },
        BrowserToolKind::Download => BrowserCommand::Download {
            ref_id: string("ref")?,
        },
    })
}

fn browser_error(error: crate::BrowserError) -> ToolError {
    ToolError::new(error.code, error.message)
}
