#![forbid(unsafe_code)]

//! Harness 跨模块共享的稳定原语。
//!
//! 本 crate 不包含业务状态机、I/O、数据库、模型或终端依赖。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

/// JSON 边界统一使用 serde_json 的值类型。
pub type JsonValue = serde_json::Value;

macro_rules! string_id {
    ($name:ident) => {
        #[doc = concat!(stringify!($name), " 的稳定字符串标识。")]
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// 读取底层字符串。
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(ProjectId);
string_id!(SessionId);
string_id!(MissionId);
string_id!(TaskId);
string_id!(RunId);
string_id!(AgentDefinitionId);
string_id!(AgentEndpointId);
string_id!(AgentInstanceId);
string_id!(AgentSessionId);
string_id!(BrowserSessionId);
string_id!(BrowserActionId);
string_id!(ApprovalId);
string_id!(ArtifactId);
string_id!(TraceId);
string_id!(ContextSeriesId);
string_id!(CheckpointId);
string_id!(GoalRevisionId);
string_id!(EffectId);
string_id!(ClaimToken);
string_id!(ActorId);
string_id!(ContextItemId);
string_id!(ContentHash);
string_id!(PromptSegmentId);
string_id!(ProviderId);
string_id!(ModelId);
string_id!(ResponseId);
string_id!(ToolCallId);
string_id!(AccountId);
string_id!(PermissionRequestId);
string_id!(PermissionGrantId);
string_id!(ToolInvocationId);

/// 聚合的单调版本号。
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct AggregateVersion(pub u64);

/// 事件的全局或作用域内单调序号。
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Sequence(pub u64);

/// 数据完整性标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrityLabel {
    Trusted,
    Untrusted,
}

/// 数据机密性标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfidentialityLabel {
    Public,
    ProjectPrivate,
    UserSecret,
}

/// 贯穿 Context、Tool、MCP、Plugin 和 Provider 的信息流标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InformationFlowLabel {
    pub integrity: IntegrityLabel,
    pub confidentiality: ConfidentialityLabel,
}

/// 顶层错误类别；各 crate 保留更细的内部错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessErrorKind {
    Model,
    Auth,
    Tool,
    Mcp,
    Plugin,
    Context,
    Storage,
    Permission,
    Sandbox,
    Agent,
    Session,
    Config,
    Kernel,
}

/// 对调用者公开的重试建议。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetryAdvice {
    Never,
    Immediate,
    Backoff,
    Reconcile,
    UserAction,
}

/// 错误影响范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImpactScope {
    Request,
    Run,
    Mission,
    Session,
    Project,
    Process,
}

/// Interface/Application 边界使用的统一脱敏错误。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessError {
    pub kind: HarnessErrorKind,
    pub code: String,
    pub message: String,
    pub retry: RetryAdvice,
    pub impact: ImpactScope,
    pub trace_id: Option<TraceId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub redacted_context: BTreeMap<String, String>,
}

impl Display for HarnessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for HarnessError {}

/// 时间来源 Port；生产实现使用单调/系统时钟组合，测试使用固定时钟。
pub trait Clock: Send + Sync {
    fn now_unix_millis(&self) -> i64;
}

/// ID 来源 Port；生成值作为 Command 输入，Reducer 不主动生成 ID。
pub trait IdGenerator: Send + Sync {
    fn next_id(&self, prefix: &str) -> String;
}

/// 跨 Session/Model/Terminal 共用的统一 reasoning 等级。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_id_round_trips_as_json_string() {
        let mission_id = MissionId::from("mission:1");
        let json = serde_json::to_string(&mission_id).expect("MissionId 应可序列化");
        assert_eq!(json, "\"mission:1\"");
        let decoded: MissionId = serde_json::from_str(&json).expect("MissionId 应可反序列化");
        assert_eq!(decoded, mission_id);
    }

    #[test]
    fn information_flow_uses_stable_kebab_case_values() {
        let label = InformationFlowLabel {
            integrity: IntegrityLabel::Untrusted,
            confidentiality: ConfidentialityLabel::ProjectPrivate,
        };
        let value = serde_json::to_value(label).expect("标签应可序列化");
        assert_eq!(value["integrity"], "untrusted");
        assert_eq!(value["confidentiality"], "project-private");
    }
}
