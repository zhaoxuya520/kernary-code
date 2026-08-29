use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{ContentHash, PromptSegmentId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Prompt message role。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// Segment 在 Prompt Cache ABI 中的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptCacheability {
    Static,
    SemiStable,
    DynamicTail,
}

/// Prompt 内容的来源。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptSource {
    pub kind: String,
    pub reference: String,
}

/// PromptRegistry 中的版本化段落。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptSegment {
    pub id: PromptSegmentId,
    pub version: String,
    pub role: PromptRole,
    pub priority: i32,
    pub cacheability: PromptCacheability,
    pub order: i32,
    pub source: PromptSource,
    pub content: String,
}

/// 按需加载到本轮 Prompt 的 Tool Schema。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolPromptSchema {
    pub canonical_name: String,
    pub version: String,
    pub schema: Value,
}

/// Prompt Registry 错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptError {
    pub code: &'static str,
    pub message: String,
}

impl PromptError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for PromptError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for PromptError {}

/// 版本化 Prompt Segment Registry。
#[derive(Clone, Debug, Default)]
pub struct PromptRegistry {
    segments: BTreeMap<PromptSegmentId, PromptSegment>,
}

impl PromptRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, segment: PromptSegment) -> Result<(), PromptError> {
        if let Some(existing) = self.segments.get(&segment.id)
            && existing != &segment
        {
            return Err(PromptError::new(
                "prompt-segment-conflict",
                format!("Segment {} 已注册不同内容", segment.id),
            ));
        }
        self.segments.insert(segment.id.clone(), segment);
        Ok(())
    }

    #[must_use]
    pub fn segments(&self) -> Vec<PromptSegment> {
        self.segments.values().cloned().collect()
    }
}

/// Canonical Prompt 和各层 fingerprint。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalPrompt {
    pub text: String,
    pub stable_prefix_hash: ContentHash,
    pub semi_stable_hash: ContentHash,
    pub dynamic_tail_hash: ContentHash,
    pub tool_schema_hash: ContentHash,
    pub full_hash: ContentHash,
    pub segment_hashes: BTreeMap<PromptSegmentId, ContentHash>,
}

/// JSON object key 递归排序；数组顺序保持业务语义。
#[must_use]
pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            let mut canonical = Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json(&object[key]));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn normalize_text(content: &str) -> String {
    let nfc = content.nfc().collect::<String>();
    let unix = nfc.replace("\r\n", "\n").replace('\r', "\n");
    unix.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_owned()
}

fn sha256(value: &str) -> ContentHash {
    ContentHash::from(format!("{:x}", Sha256::digest(value.as_bytes())))
}

fn role_name(role: PromptRole) -> &'static str {
    match role {
        PromptRole::System => "system",
        PromptRole::Developer => "developer",
        PromptRole::User => "user",
        PromptRole::Assistant => "assistant",
        PromptRole::Tool => "tool",
    }
}

fn canonical_segment(segment: &PromptSegment) -> String {
    format!(
        "<segment id=\"{}\" version=\"{}\" role=\"{}\" source=\"{}:{}\">\n{}\n</segment>",
        segment.id,
        segment.version,
        role_name(segment.role),
        segment.source.kind,
        segment.source.reference,
        normalize_text(&segment.content)
    )
}

fn canonical_tool(tool: &ToolPromptSchema) -> Result<String, PromptError> {
    let schema = serde_json::to_string(&canonical_json(&tool.schema))
        .map_err(|error| PromptError::new("tool-schema-json", error.to_string()))?;
    Ok(format!(
        "<tool name=\"{}\" version=\"{}\">{schema}</tool>",
        tool.canonical_name, tool.version
    ))
}

/// 固定 Segment/Tool 顺序和字节表示。
#[derive(Clone, Copy, Debug, Default)]
pub struct PromptCanonicalizer;

impl PromptCanonicalizer {
    pub fn compile(
        &self,
        mut segments: Vec<PromptSegment>,
        mut tools: Vec<ToolPromptSchema>,
    ) -> Result<CanonicalPrompt, PromptError> {
        let mut ids = BTreeSet::new();
        for segment in &segments {
            if !ids.insert(segment.id.clone()) {
                return Err(PromptError::new(
                    "duplicate-prompt-segment",
                    segment.id.to_string(),
                ));
            }
        }
        segments.sort_by_key(|segment| {
            (
                segment.cacheability,
                segment.order,
                Reverse(segment.priority),
                segment.id.clone(),
            )
        });
        tools.sort_by(|left, right| {
            left.canonical_name
                .cmp(&right.canonical_name)
                .then(left.version.cmp(&right.version))
        });

        let mut segment_hashes = BTreeMap::new();
        let mut stable = Vec::new();
        let mut semi = Vec::new();
        let mut dynamic = Vec::new();
        for segment in &segments {
            let canonical = canonical_segment(segment);
            segment_hashes.insert(segment.id.clone(), sha256(&canonical));
            match segment.cacheability {
                PromptCacheability::Static => stable.push(canonical),
                PromptCacheability::SemiStable => semi.push(canonical),
                PromptCacheability::DynamicTail => dynamic.push(canonical),
            }
        }
        let tool_lines = tools
            .iter()
            .map(canonical_tool)
            .collect::<Result<Vec<_>, _>>()?;
        let tool_text = tool_lines.join("\n");
        if !tool_text.is_empty() {
            stable.push(format!("<tools>\n{tool_text}\n</tools>"));
        }
        let stable_text = stable.join("\n");
        let semi_text = semi.join("\n");
        let dynamic_text = dynamic.join("\n");
        let text = [
            stable_text.as_str(),
            semi_text.as_str(),
            dynamic_text.as_str(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        Ok(CanonicalPrompt {
            stable_prefix_hash: sha256(&stable_text),
            semi_stable_hash: sha256(&semi_text),
            dynamic_tail_hash: sha256(&dynamic_text),
            tool_schema_hash: sha256(&tool_text),
            full_hash: sha256(&text),
            text,
            segment_hashes,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn segment(id: &str, class: PromptCacheability, content: &str) -> PromptSegment {
        PromptSegment {
            id: PromptSegmentId::from(id),
            version: "1".to_owned(),
            role: PromptRole::Developer,
            priority: 10,
            cacheability: class,
            order: 0,
            source: PromptSource {
                kind: "test".to_owned(),
                reference: id.to_owned(),
            },
            content: content.to_owned(),
        }
    }

    #[test]
    fn order_line_endings_and_json_keys_do_not_change_prompt() {
        let first = PromptCanonicalizer
            .compile(
                vec![
                    segment("dynamic", PromptCacheability::DynamicTail, "tail\r\n"),
                    segment("static", PromptCacheability::Static, "core  \r\nrule"),
                ],
                vec![
                    ToolPromptSchema {
                        canonical_name: "z.tool".to_owned(),
                        version: "1".to_owned(),
                        schema: json!({"b":2,"a":1}),
                    },
                    ToolPromptSchema {
                        canonical_name: "a.tool".to_owned(),
                        version: "1".to_owned(),
                        schema: json!({"type":"object"}),
                    },
                ],
            )
            .expect("first prompt");
        let second = PromptCanonicalizer
            .compile(
                vec![
                    segment("static", PromptCacheability::Static, "core\nrule"),
                    segment("dynamic", PromptCacheability::DynamicTail, "tail"),
                ],
                vec![
                    ToolPromptSchema {
                        canonical_name: "a.tool".to_owned(),
                        version: "1".to_owned(),
                        schema: json!({"type":"object"}),
                    },
                    ToolPromptSchema {
                        canonical_name: "z.tool".to_owned(),
                        version: "1".to_owned(),
                        schema: json!({"a":1,"b":2}),
                    },
                ],
            )
            .expect("second prompt");
        assert_eq!(first, second);
    }

    #[test]
    fn dynamic_change_preserves_stable_prefix_only() {
        let first = PromptCanonicalizer
            .compile(
                vec![
                    segment("core", PromptCacheability::Static, "same"),
                    segment("tail", PromptCacheability::DynamicTail, "one"),
                ],
                vec![],
            )
            .expect("first");
        let second = PromptCanonicalizer
            .compile(
                vec![
                    segment("core", PromptCacheability::Static, "same"),
                    segment("tail", PromptCacheability::DynamicTail, "two"),
                ],
                vec![],
            )
            .expect("second");
        assert_eq!(first.stable_prefix_hash, second.stable_prefix_hash);
        assert_ne!(first.dynamic_tail_hash, second.dynamic_tail_hash);
        assert_ne!(first.full_hash, second.full_hash);
    }

    #[test]
    fn registry_rejects_conflicting_segment() {
        let mut registry = PromptRegistry::new();
        registry
            .register(segment("core", PromptCacheability::Static, "one"))
            .expect("first");
        let error = registry
            .register(segment("core", PromptCacheability::Static, "two"))
            .expect_err("conflict");
        assert_eq!(error.code, "prompt-segment-conflict");
    }
}
