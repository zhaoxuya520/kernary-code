use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub const LATEST_STABLE_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_CATALOG_PAGES: usize = 64;
const MAX_CATALOG_ITEMS: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl McpError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl Display for McpError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for McpError {}

pub trait McpTransport: Send + Sync {
    fn kind(&self) -> &'static str;
    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError>;
    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), McpError>;
    fn set_protocol_version(&self, protocol_version: &str) -> Result<(), McpError>;
    fn poll_notifications(&self) -> Result<Vec<serde_json::Value>, McpError> {
        Ok(vec![])
    }
    fn close(&self) -> Result<(), McpError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTaskSupport {
    Forbidden,
    Optional,
    Required,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpToolDescriptor {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    pub annotations: McpToolAnnotations,
    pub task_support: Option<McpTaskSupport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpPromptDescriptor {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub arguments: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCallToolResult {
    pub content: Vec<serde_json::Value>,
    pub is_error: bool,
    pub structured_content: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    protocol_version: String,
    server_info: McpServerInfo,
    capabilities: serde_json::Value,
    instructions: Option<String>,
}

impl McpClient {
    pub fn initialize(transport: Arc<dyn McpTransport>) -> Result<Self, McpError> {
        let result = transport.request(
            "initialize",
            serde_json::json!({
                "protocolVersion":LATEST_STABLE_PROTOCOL_VERSION,
                "capabilities":{},
                "clientInfo":{
                    "name":"harness-terminal",
                    "title":"Harness Terminal",
                    "version":env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        let result = object(&result, "mcp-initialize-result")?;
        let protocol_version = string_field(result, "protocolVersion", "mcp-protocol-version")?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol_version.as_str()) {
            transport.close()?;
            return Err(McpError::new(
                "mcp-protocol-version-unsupported",
                protocol_version,
            ));
        }
        transport.set_protocol_version(&protocol_version)?;
        let server_info = object_field(result, "serverInfo", "mcp-server-info")?;
        let server_info = McpServerInfo {
            name: string_field(server_info, "name", "mcp-server-name")?,
            version: string_field(server_info, "version", "mcp-server-version")?,
            title: optional_string(server_info, "title")?,
        };
        let capabilities = result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        object(&capabilities, "mcp-server-capabilities")?;
        let instructions = optional_string(result, "instructions")?;
        transport.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(Self {
            transport,
            protocol_version,
            server_info,
            capabilities,
            instructions,
        })
    }

    #[must_use]
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    #[must_use]
    pub fn transport_kind(&self) -> &'static str {
        self.transport.kind()
    }

    #[must_use]
    pub fn server_info(&self) -> &McpServerInfo {
        &self.server_info
    }

    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    #[must_use]
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities
            .as_object()
            .is_some_and(|capabilities| capabilities.contains_key(capability))
    }

    pub fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpError> {
        if !self.has_capability("tools") {
            return Ok(vec![]);
        }
        let values = self.list_paginated("tools/list", "tools")?;
        values.into_iter().map(parse_tool).collect()
    }

    pub fn list_resources(&self) -> Result<Vec<McpResourceDescriptor>, McpError> {
        if !self.has_capability("resources") {
            return Ok(vec![]);
        }
        let values = self.list_paginated("resources/list", "resources")?;
        values.into_iter().map(parse_resource).collect()
    }

    pub fn list_prompts(&self) -> Result<Vec<McpPromptDescriptor>, McpError> {
        if !self.has_capability("prompts") {
            return Ok(vec![]);
        }
        let values = self.list_paginated("prompts/list", "prompts")?;
        values.into_iter().map(parse_prompt).collect()
    }

    pub fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpCallToolResult, McpError> {
        let result = self.transport.request(
            "tools/call",
            serde_json::json!({"name":name,"arguments":arguments}),
        )?;
        let result = object(&result, "mcp-call-tool-result")?;
        let content = result
            .get("content")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .ok_or_else(|| McpError::new("invalid-mcp-tool-content", name))?;
        Ok(McpCallToolResult {
            content,
            is_error: result
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            structured_content: result.get("structuredContent").cloned(),
        })
    }

    pub fn read_resource(&self, uri: &str) -> Result<Vec<serde_json::Value>, McpError> {
        let result = self
            .transport
            .request("resources/read", serde_json::json!({"uri":uri}))?;
        object(&result, "mcp-read-resource-result")?
            .get("contents")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .ok_or_else(|| McpError::new("invalid-mcp-resource-contents", uri))
    }

    pub fn close(&self) -> Result<(), McpError> {
        self.transport.close()
    }

    pub fn poll_notifications(&self) -> Result<Vec<serde_json::Value>, McpError> {
        self.transport.poll_notifications()
    }

    fn list_paginated(
        &self,
        method: &str,
        field: &str,
    ) -> Result<Vec<serde_json::Value>, McpError> {
        let mut cursor = None::<String>;
        let mut output = Vec::new();
        for _ in 0..MAX_CATALOG_PAGES {
            let params = cursor.as_ref().map_or_else(
                || serde_json::json!({}),
                |cursor| serde_json::json!({"cursor":cursor}),
            );
            let page = self.transport.request(method, params)?;
            let page = object(&page, "mcp-list-result")?;
            let values = page
                .get(field)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| McpError::new("invalid-mcp-list", field))?;
            if output.len().saturating_add(values.len()) > MAX_CATALOG_ITEMS {
                return Err(McpError::new("mcp-catalog-too-large", field));
            }
            output.extend(values.iter().cloned());
            cursor = optional_string(page, "nextCursor")?;
            if cursor.is_none() {
                return Ok(output);
            }
        }
        Err(McpError::new("mcp-catalog-page-limit", method))
    }
}

fn parse_tool(value: serde_json::Value) -> Result<McpToolDescriptor, McpError> {
    let tool = object(&value, "mcp-tool")?;
    let name = string_field(tool, "name", "mcp-tool-name")?;
    validate_capability_name(&name, "mcp-tool-name")?;
    let input_schema = tool
        .get("inputSchema")
        .cloned()
        .ok_or_else(|| McpError::new("invalid-mcp-tool-input-schema", &name))?;
    object(&input_schema, "mcp-tool-input-schema")?;
    let output_schema = tool.get("outputSchema").cloned();
    if let Some(schema) = &output_schema {
        object(schema, "mcp-tool-output-schema")?;
    }
    let annotations = tool
        .get("annotations")
        .map_or(Ok(Default::default()), |value| {
            serde_json::from_value(value.clone())
                .map_err(|error| McpError::new("invalid-mcp-tool-annotations", error.to_string()))
        })?;
    let task_support = tool
        .get("execution")
        .and_then(serde_json::Value::as_object)
        .and_then(|execution| execution.get("taskSupport"))
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|error| McpError::new("invalid-mcp-task-support", error.to_string()))
        })
        .transpose()?;
    Ok(McpToolDescriptor {
        name,
        title: optional_string(tool, "title")?,
        description: optional_string(tool, "description")?,
        input_schema,
        output_schema,
        annotations,
        task_support,
    })
}

fn parse_resource(value: serde_json::Value) -> Result<McpResourceDescriptor, McpError> {
    let resource = object(&value, "mcp-resource")?;
    Ok(McpResourceDescriptor {
        uri: string_field(resource, "uri", "mcp-resource-uri")?,
        name: string_field(resource, "name", "mcp-resource-name")?,
        title: optional_string(resource, "title")?,
        description: optional_string(resource, "description")?,
        mime_type: optional_string(resource, "mimeType")?,
        size: resource
            .get("size")
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| McpError::new("invalid-mcp-resource-size", value.to_string()))
            })
            .transpose()?,
    })
}

fn parse_prompt(value: serde_json::Value) -> Result<McpPromptDescriptor, McpError> {
    let prompt = object(&value, "mcp-prompt")?;
    let name = string_field(prompt, "name", "mcp-prompt-name")?;
    validate_capability_name(&name, "mcp-prompt-name")?;
    Ok(McpPromptDescriptor {
        name,
        title: optional_string(prompt, "title")?,
        description: optional_string(prompt, "description")?,
        arguments: prompt
            .get("arguments")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
}

fn validate_capability_name(value: &str, code: &'static str) -> Result<(), McpError> {
    if value.is_empty()
        || value.len() > 128
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(McpError::new(code, value));
    }
    Ok(())
}

fn object<'a>(
    value: &'a serde_json::Value,
    context: &'static str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, McpError> {
    value
        .as_object()
        .ok_or_else(|| McpError::new(format!("invalid-{context}"), "expected object"))
}

fn object_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &'static str,
    context: &'static str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, McpError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| McpError::new(format!("invalid-{context}"), field))
}

fn string_field(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
    context: &'static str,
) -> Result<String, McpError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| McpError::new(format!("invalid-{context}"), field))
}

fn optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<Option<String>, McpError> {
    object.get(field).map_or(Ok(None), |value| {
        value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| McpError::new("invalid-mcp-string", field))
    })
}
