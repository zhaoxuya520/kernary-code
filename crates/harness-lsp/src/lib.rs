#![forbid(unsafe_code)]

//! Lazy、只读的 Language Server Protocol 3.18 Bridge。

mod transport;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use transport::LspTransport;
use url::Url;

use harness_permission::PermissionAction;
use harness_tool::{
    ToolDescriptor, ToolEffectClass, ToolError, ToolExecutionInput, ToolPromptLoading,
    ToolProvider, ToolRegistry, ToolSource,
};

const CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl LspError {
    #[must_use]
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

impl Display for LspError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for LspError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct LspServerConfig {
    pub id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub language_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub inherit_env: Vec<String>,
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
    #[serde(default)]
    pub request_timeout_millis: Option<u64>,
    #[serde(default)]
    pub max_message_bytes: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct LspConfigFile {
    schema_version: u32,
    #[serde(default)]
    servers: Vec<LspServerConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspCapabilityView {
    pub document_symbols: bool,
    pub definition: bool,
    pub references: bool,
    pub diagnostics: bool,
    pub rename: bool,
    pub prepare_rename: bool,
    pub code_action: bool,
    pub code_action_resolve: bool,
    pub position_encoding: LspPositionEncoding,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LspPositionEncoding {
    #[serde(rename = "utf-8")]
    Utf8,
    #[default]
    #[serde(rename = "utf-16")]
    Utf16,
    #[serde(rename = "utf-32")]
    Utf32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspServerView {
    pub id: String,
    pub status: String,
    pub languages: Vec<String>,
    pub open_documents: usize,
    pub capabilities: Option<LspCapabilityView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspProcessSpec {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspDocumentSnapshot {
    pub path: String,
    pub file_hash: String,
    pub document_version: i32,
    pub position_encoding: LspPositionEncoding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspComputedFileEdit {
    pub path: String,
    pub before_hash: String,
    pub after_hash: String,
    pub after_text: String,
    pub edit_count: usize,
    pub added_bytes: usize,
    pub removed_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspComputedWorkspaceEdit {
    pub server_id: String,
    pub source_method: String,
    pub title: String,
    pub source_document: LspDocumentSnapshot,
    pub fingerprint: String,
    pub files: Vec<LspComputedFileEdit>,
    pub total_edits: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HumanPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HumanRange {
    pub start: HumanPosition,
    pub end: HumanPosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspLocation {
    pub uri: String,
    pub path: Option<String>,
    pub range: LspRange,
    pub human_range: Option<HumanRange>,
    pub position_encoding: LspPositionEncoding,
    pub external: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: u32,
    pub container_name: Option<String>,
    pub location: LspLocation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspDiagnostic {
    pub path: Option<String>,
    pub range: LspRange,
    pub human_range: Option<HumanRange>,
    pub position_encoding: LspPositionEncoding,
    pub severity: Option<u32>,
    pub code: Option<String>,
    pub source: Option<String>,
    pub message: String,
}

#[derive(Clone)]
pub struct LspManager {
    project_root: PathBuf,
    servers: Arc<BTreeMap<String, LspServerConfig>>,
    sessions: Arc<Mutex<BTreeMap<String, Arc<LspSession>>>>,
}

struct LspSession {
    config: LspServerConfig,
    project_root: PathBuf,
    transport: Arc<LspTransport>,
    capabilities: LspCapabilityView,
    position_encoding: LspPositionEncoding,
    documents: Mutex<BTreeMap<PathBuf, OpenDocument>>,
    diagnostics: Mutex<BTreeMap<String, Vec<LspDiagnostic>>>,
}

#[derive(Clone)]
struct OpenDocument {
    uri: String,
    version: i32,
    sha256: String,
    text: String,
}

impl LspManager {
    pub fn load(path: impl AsRef<Path>, project_root: impl AsRef<Path>) -> Result<Self, LspError> {
        let project_root = fs::canonicalize(project_root.as_ref())
            .map_err(|error| LspError::new("lsp-project-root", error.to_string()))?;
        let path = path.as_ref();
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| LspError::new("lsp-config-io", error.to_string()))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(LspError::new(
                "lsp-config-size-or-type",
                path.display().to_string(),
            ));
        }
        let source = fs::read_to_string(path)
            .map_err(|error| LspError::new("lsp-config-io", error.to_string()))?;
        let file: LspConfigFile = toml::from_str(&source)
            .map_err(|error| LspError::new("lsp-config-toml", error.to_string()))?;
        if file.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(LspError::new(
                "lsp-config-schema-unsupported",
                file.schema_version.to_string(),
            ));
        }
        Self::new(project_root, file.servers)
    }

    pub fn new(
        project_root: impl AsRef<Path>,
        servers: Vec<LspServerConfig>,
    ) -> Result<Self, LspError> {
        let project_root = fs::canonicalize(project_root.as_ref())
            .map_err(|error| LspError::new("lsp-project-root", error.to_string()))?;
        let mut indexed = BTreeMap::new();
        for server in servers {
            validate_server(&server)?;
            if indexed.insert(server.id.clone(), server).is_some() {
                return Err(LspError::new("lsp-server-id-conflict", "duplicate id"));
            }
        }
        Ok(Self {
            project_root,
            servers: Arc::new(indexed),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.servers.is_empty()
    }

    pub fn list(&self) -> Result<Vec<LspServerView>, LspError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| LspError::new("lsp-sessions-poisoned", "lock"))?;
        Ok(self
            .servers
            .values()
            .map(|server| {
                let session = sessions.get(&server.id);
                LspServerView {
                    id: server.id.clone(),
                    status: if session.is_some() {
                        "ready"
                    } else {
                        "sleeping"
                    }
                    .to_owned(),
                    languages: server.language_ids.values().cloned().collect(),
                    open_documents: session.map_or(0, |session| session.open_document_count()),
                    capabilities: session.map(|session| session.capabilities.clone()),
                }
            })
            .collect())
    }

    pub fn process_spec(&self, server_id: &str) -> Result<LspProcessSpec, LspError> {
        let server = self
            .servers
            .get(server_id)
            .ok_or_else(|| LspError::new("lsp-server-not-found", server_id))?;
        let executable = fs::canonicalize(&server.command)
            .map_err(|error| LspError::new("lsp-command", error.to_string()))?;
        if !executable.is_file() {
            return Err(LspError::new(
                "lsp-command-not-file",
                executable.display().to_string(),
            ));
        }
        let candidate = server.cwd.as_ref().map_or_else(
            || self.project_root.clone(),
            |cwd| {
                if cwd.is_absolute() {
                    cwd.clone()
                } else {
                    self.project_root.join(cwd)
                }
            },
        );
        let cwd = fs::canonicalize(candidate)
            .map_err(|error| LspError::new("lsp-cwd", error.to_string()))?;
        if !cwd.is_dir() || !is_inside(&self.project_root, &cwd) {
            return Err(LspError::new(
                "lsp-cwd-outside-project",
                cwd.display().to_string(),
            ));
        }
        Ok(LspProcessSpec {
            executable,
            arguments: server.args.clone(),
            cwd,
        })
    }

    pub fn process_specs(&self) -> Result<Vec<LspProcessSpec>, LspError> {
        self.servers
            .keys()
            .map(|server_id| self.process_spec(server_id))
            .collect()
    }

    pub fn start(&self, server_id: &str) -> Result<LspServerView, LspError> {
        let session = self.session(server_id)?;
        Ok(LspServerView {
            id: server_id.to_owned(),
            status: "ready".to_owned(),
            languages: session.config.language_ids.values().cloned().collect(),
            open_documents: session.open_document_count(),
            capabilities: Some(session.capabilities.clone()),
        })
    }

    pub fn stop(&self, server_id: &str) -> Result<bool, LspError> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| LspError::new("lsp-sessions-poisoned", "lock"))?
            .remove(server_id);
        if let Some(session) = session {
            session.shutdown()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn document_symbols(
        &self,
        server_id: &str,
        path: &Path,
    ) -> Result<Vec<LspSymbol>, LspError> {
        self.session(server_id)?.document_symbols(path)
    }

    pub fn definition(
        &self,
        server_id: &str,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        self.session(server_id)?
            .locations("textDocument/definition", path, line, character, false)
    }

    pub fn references(
        &self,
        server_id: &str,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        self.session(server_id)?
            .locations("textDocument/references", path, line, character, true)
    }

    pub fn diagnostics(
        &self,
        server_id: &str,
        path: &Path,
    ) -> Result<Vec<LspDiagnostic>, LspError> {
        self.session(server_id)?.diagnostics(path)
    }

    pub fn document_snapshot(
        &self,
        server_id: &str,
        path: &Path,
    ) -> Result<LspDocumentSnapshot, LspError> {
        self.session(server_id)?.document_snapshot(path)
    }

    pub fn rename_edit(
        &self,
        server_id: &str,
        path: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<LspComputedWorkspaceEdit, LspError> {
        self.session(server_id)?
            .rename_edit(path, line, character, new_name)
    }

    pub fn code_action_edit(
        &self,
        server_id: &str,
        path: &Path,
        range: HumanRange,
        action_index: usize,
        only: Option<&str>,
    ) -> Result<LspComputedWorkspaceEdit, LspError> {
        self.session(server_id)?
            .code_action_edit(path, range, action_index, only)
    }

    fn session(&self, server_id: &str) -> Result<Arc<LspSession>, LspError> {
        if let Some(session) = self
            .sessions
            .lock()
            .map_err(|_| LspError::new("lsp-sessions-poisoned", "lock"))?
            .get(server_id)
            .cloned()
        {
            return Ok(session);
        }
        let config = self
            .servers
            .get(server_id)
            .cloned()
            .ok_or_else(|| LspError::new("lsp-server-not-found", server_id))?;
        let session = Arc::new(LspSession::start(config, &self.project_root)?);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| LspError::new("lsp-sessions-poisoned", "lock"))?;
        if let Some(existing) = sessions.get(server_id) {
            let _ = session.shutdown();
            return Ok(existing.clone());
        }
        sessions.insert(server_id.to_owned(), session.clone());
        Ok(session)
    }
}

impl LspSession {
    fn start(config: LspServerConfig, project_root: &Path) -> Result<Self, LspError> {
        let root_uri = Url::from_directory_path(project_root)
            .map_err(|()| LspError::new("lsp-root-uri", project_root.display().to_string()))?
            .to_string();
        let transport = LspTransport::spawn(&config, project_root, &root_uri)?;
        let initialize = transport.request(
            "initialize",
            serde_json::json!({
                "processId":std::process::id(),
                "clientInfo":{"name":"Kernary Code","version":env!("CARGO_PKG_VERSION")},
                "locale":"zh-CN",
                "rootUri":root_uri,
                "workspaceFolders":[{"uri":root_uri,"name":project_root.file_name().and_then(|name|name.to_str()).unwrap_or("workspace")}],
                "capabilities":{
                    "general":{"positionEncodings":["utf-16","utf-8","utf-32"]},
                    "workspace":{
                        "workspaceFolders":true,
                        "configuration":true,
                        "applyEdit":false,
                        "workspaceEdit":{
                            "documentChanges":true,
                            "resourceOperations":[],
                            "failureHandling":"transactional",
                            "normalizesLineEndings":false
                        }
                    },
                    "textDocument":{
                        "synchronization":{"dynamicRegistration":false,"didSave":false},
                        "documentSymbol":{"dynamicRegistration":false,"hierarchicalDocumentSymbolSupport":true},
                        "definition":{"dynamicRegistration":false,"linkSupport":true},
                        "references":{"dynamicRegistration":false},
                        "rename":{"dynamicRegistration":false,"prepareSupport":true,"honorsChangeAnnotations":false},
                        "codeAction":{
                            "dynamicRegistration":false,
                            "dataSupport":true,
                            "isPreferredSupport":true,
                            "disabledSupport":true,
                            "resolveSupport":{"properties":["edit"]},
                            "honorsChangeAnnotations":false,
                            "codeActionLiteralSupport":{"codeActionKind":{"valueSet":["quickfix","refactor","refactor.extract","refactor.inline","refactor.rewrite","source"]}}
                        },
                        "publishDiagnostics":{"relatedInformation":true,"versionSupport":true}
                    }
                },
                "initializationOptions":config.initialization_options.clone().unwrap_or(serde_json::Value::Null),
                "trace":"off"
            }),
        )?;
        let capabilities = capability_view(initialize.get("capabilities"));
        let position_encoding = capabilities.position_encoding;
        transport.notify("initialized", serde_json::json!({}))?;
        Ok(Self {
            config,
            project_root: project_root.to_path_buf(),
            transport,
            capabilities,
            position_encoding,
            documents: Mutex::new(BTreeMap::new()),
            diagnostics: Mutex::new(BTreeMap::new()),
        })
    }

    fn open_document_count(&self) -> usize {
        self.documents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn document_snapshot(&self, requested: &Path) -> Result<LspDocumentSnapshot, LspError> {
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.project_root.join(requested)
        };
        let absolute = fs::canonicalize(absolute)
            .map_err(|error| LspError::new("lsp-document", error.to_string()))?;
        let document = self
            .documents
            .lock()
            .map_err(|_| LspError::new("lsp-documents-poisoned", "lock"))?
            .get(&absolute)
            .cloned()
            .ok_or_else(|| {
                LspError::new(
                    "lsp-document-not-synchronized",
                    absolute.display().to_string(),
                )
            })?;
        let path = absolute
            .strip_prefix(&self.project_root)
            .map_err(|_| {
                LspError::new(
                    "lsp-document-outside-project",
                    absolute.display().to_string(),
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        Ok(LspDocumentSnapshot {
            path,
            file_hash: document.sha256,
            document_version: document.version,
            position_encoding: self.position_encoding,
        })
    }

    fn sync_document(&self, requested: &Path) -> Result<OpenDocument, LspError> {
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.project_root.join(requested)
        };
        let absolute = fs::canonicalize(&absolute)
            .map_err(|error| LspError::new("lsp-document", error.to_string()))?;
        if !is_inside(&self.project_root, &absolute) || !absolute.is_file() {
            return Err(LspError::new(
                "lsp-document-outside-project",
                absolute.display().to_string(),
            ));
        }
        let metadata = fs::metadata(&absolute)
            .map_err(|error| LspError::new("lsp-document-metadata", error.to_string()))?;
        if metadata.len() > MAX_DOCUMENT_BYTES {
            return Err(LspError::new(
                "lsp-document-too-large",
                metadata.len().to_string(),
            ));
        }
        let bytes = fs::read(&absolute)
            .map_err(|error| LspError::new("lsp-document-read", error.to_string()))?;
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| LspError::new("lsp-document-not-utf8", absolute.display().to_string()))?;
        let extension = absolute
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let language_id = self.config.language_ids.get(&extension).ok_or_else(|| {
            LspError::new(
                "lsp-language-not-configured",
                format!("{}:.{extension}", self.config.id),
            )
        })?;
        let uri = Url::from_file_path(&absolute)
            .map_err(|()| LspError::new("lsp-document-uri", absolute.display().to_string()))?
            .to_string();
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let mut documents = self
            .documents
            .lock()
            .map_err(|_| LspError::new("lsp-documents-poisoned", "lock"))?;
        match documents.get_mut(&absolute) {
            None => {
                self.transport.notify(
                    "textDocument/didOpen",
                    serde_json::json!({
                        "textDocument":{
                            "uri":uri,
                            "languageId":language_id,
                            "version":1,
                            "text":text
                        }
                    }),
                )?;
                let document = OpenDocument {
                    uri,
                    version: 1,
                    sha256,
                    text,
                };
                documents.insert(absolute, document.clone());
                Ok(document)
            }
            Some(document) if document.sha256 == sha256 => Ok(document.clone()),
            Some(document) => {
                document.version = document.version.saturating_add(1);
                document.sha256 = sha256;
                document.text.clone_from(&text);
                self.transport.notify(
                    "textDocument/didChange",
                    serde_json::json!({
                        "textDocument":{"uri":document.uri,"version":document.version},
                        "contentChanges":[{"text":text}]
                    }),
                )?;
                Ok(document.clone())
            }
        }
    }

    fn document_symbols(&self, path: &Path) -> Result<Vec<LspSymbol>, LspError> {
        if !self.capabilities.document_symbols {
            return Err(LspError::new(
                "lsp-capability-unsupported",
                "documentSymbol",
            ));
        }
        let document = self.sync_document(path)?;
        let result = self.transport.request(
            "textDocument/documentSymbol",
            serde_json::json!({"textDocument":{"uri":document.uri}}),
        )?;
        self.drain_notifications()?;
        normalize_symbols(
            &self.project_root,
            &document.uri,
            result,
            self.position_encoding,
        )
    }

    fn locations(
        &self,
        method: &str,
        path: &Path,
        line: u32,
        character: u32,
        references: bool,
    ) -> Result<Vec<LspLocation>, LspError> {
        let supported = if references {
            self.capabilities.references
        } else {
            self.capabilities.definition
        };
        if !supported {
            return Err(LspError::new("lsp-capability-unsupported", method));
        }
        let document = self.sync_document(path)?;
        let position =
            human_to_protocol_position(&document.text, line, character, self.position_encoding)?;
        let mut params = serde_json::json!({
            "textDocument":{"uri":document.uri},
            "position":{"line":position.line,"character":position.character}
        });
        if references {
            params["context"] = serde_json::json!({"includeDeclaration":true});
        }
        let result = self.transport.request(method, params)?;
        self.drain_notifications()?;
        normalize_locations(&self.project_root, result, self.position_encoding)
    }

    fn diagnostics(&self, path: &Path) -> Result<Vec<LspDiagnostic>, LspError> {
        if !self.capabilities.diagnostics {
            return Err(LspError::new(
                "lsp-capability-unsupported",
                "publishDiagnostics",
            ));
        }
        let document = self.sync_document(path)?;
        let started = Instant::now();
        loop {
            self.drain_notifications()?;
            if let Some(diagnostics) = self
                .diagnostics
                .lock()
                .map_err(|_| LspError::new("lsp-diagnostics-poisoned", "lock"))?
                .get(&document.uri)
                .cloned()
            {
                return Ok(diagnostics);
            }
            if started.elapsed() >= Duration::from_millis(750) {
                return Ok(vec![]);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn rename_edit(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<LspComputedWorkspaceEdit, LspError> {
        if !self.capabilities.rename {
            return Err(LspError::new("lsp-capability-unsupported", "rename"));
        }
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.len() > 512 || new_name.contains(['\r', '\n', '\0']) {
            return Err(LspError::new("lsp-rename-name-invalid", new_name));
        }
        let document = self.sync_document(path)?;
        let position =
            human_to_protocol_position(&document.text, line, character, self.position_encoding)?;
        let position_params = serde_json::json!({
            "textDocument":{"uri":document.uri},
            "position":{"line":position.line,"character":position.character}
        });
        if self.capabilities.prepare_rename {
            let prepared = self
                .transport
                .request("textDocument/prepareRename", position_params.clone())?;
            if prepared.is_null() {
                return Err(LspError::new(
                    "lsp-rename-not-valid",
                    path.display().to_string(),
                ));
            }
        }
        let mut params = position_params;
        params["newName"] = serde_json::Value::String(new_name.to_owned());
        let edit = self.transport.request("textDocument/rename", params)?;
        self.compute_workspace_edit("rename", format!("Rename to {new_name}"), document, edit)
    }

    fn code_action_edit(
        &self,
        path: &Path,
        range: HumanRange,
        action_index: usize,
        only: Option<&str>,
    ) -> Result<LspComputedWorkspaceEdit, LspError> {
        if !self.capabilities.code_action {
            return Err(LspError::new("lsp-capability-unsupported", "codeAction"));
        }
        if action_index > 255 || only.is_some_and(|only| only.is_empty() || only.len() > 128) {
            return Err(LspError::new(
                "lsp-code-action-selection-invalid",
                action_index.to_string(),
            ));
        }
        let document = self.sync_document(path)?;
        let start = human_to_protocol_position(
            &document.text,
            range.start.line,
            range.start.character,
            self.position_encoding,
        )?;
        let end = human_to_protocol_position(
            &document.text,
            range.end.line,
            range.end.character,
            self.position_encoding,
        )?;
        let mut context = serde_json::json!({
            "diagnostics":[],
            "triggerKind":1
        });
        if let Some(only) = only {
            context["only"] = serde_json::json!([only]);
        }
        let result = self.transport.request(
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument":{"uri":document.uri},
                "range":{"start":start,"end":end},
                "context":context
            }),
        )?;
        let actions = result
            .as_array()
            .ok_or_else(|| LspError::new("lsp-code-action-result-invalid", "expected array"))?;
        if actions.len() > 256 {
            return Err(LspError::new(
                "lsp-code-action-count-limit",
                actions.len().to_string(),
            ));
        }
        let mut action = actions.get(action_index).cloned().ok_or_else(|| {
            LspError::new(
                "lsp-code-action-index-out-of-range",
                action_index.to_string(),
            )
        })?;
        if action.get("disabled").is_some() {
            return Err(LspError::new(
                "lsp-code-action-disabled",
                action
                    .pointer("/disabled/reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("disabled"),
            ));
        }
        let title = action
            .get("title")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                LspError::new("lsp-code-action-title-missing", action_index.to_string())
            })?
            .chars()
            .take(512)
            .collect::<String>();
        if action.get("edit").is_none()
            && self.capabilities.code_action_resolve
            && action.get("data").is_some()
        {
            action = self.transport.request("codeAction/resolve", action)?;
        }
        if action.get("command").is_some() {
            return Err(LspError::new("lsp-code-action-command-denied", title));
        }
        let edit = action
            .get("edit")
            .cloned()
            .ok_or_else(|| LspError::new("lsp-code-action-edit-missing", &title))?;
        self.compute_workspace_edit("code-action", title, document, edit)
    }

    fn compute_workspace_edit(
        &self,
        source_method: &str,
        title: String,
        source_document: OpenDocument,
        edit: serde_json::Value,
    ) -> Result<LspComputedWorkspaceEdit, LspError> {
        if edit.is_null() {
            return Err(LspError::new("lsp-workspace-edit-empty", source_method));
        }
        let source_path = Url::parse(&source_document.uri)
            .map_err(|error| LspError::new("lsp-document-uri", error.to_string()))?
            .to_file_path()
            .map_err(|()| LspError::new("lsp-document-uri", &source_document.uri))?;
        let source_snapshot = self.document_snapshot(&source_path)?;
        let files = compute_workspace_files(
            &self.project_root,
            &self.documents,
            &edit,
            self.position_encoding,
        )?;
        let total_edits = files.iter().map(|file| file.edit_count).sum();
        let mut fingerprint_parts = vec![
            self.config.id.clone(),
            source_method.to_owned(),
            title.clone(),
            source_snapshot.file_hash.clone(),
        ];
        for file in &files {
            fingerprint_parts.extend([
                file.path.clone(),
                file.before_hash.clone(),
                file.after_hash.clone(),
                file.edit_count.to_string(),
            ]);
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(fingerprint_parts.join("\n").as_bytes())
        );
        Ok(LspComputedWorkspaceEdit {
            server_id: self.config.id.clone(),
            source_method: source_method.to_owned(),
            title,
            source_document: source_snapshot,
            fingerprint,
            files,
            total_edits,
        })
    }

    fn drain_notifications(&self) -> Result<(), LspError> {
        for notification in self.transport.poll_notifications()? {
            if notification
                .get("method")
                .and_then(serde_json::Value::as_str)
                != Some("textDocument/publishDiagnostics")
            {
                continue;
            }
            let params = &notification["params"];
            let Some(uri) = params.get("uri").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let path = uri_to_relative(&self.project_root, uri).0;
            let diagnostics = params
                .get("diagnostics")
                .and_then(serde_json::Value::as_array)
                .map_or_else(Vec::new, |items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            normalize_diagnostic(
                                &self.project_root,
                                uri,
                                path.clone(),
                                item,
                                self.position_encoding,
                            )
                            .ok()
                        })
                        .collect()
                });
            self.diagnostics
                .lock()
                .map_err(|_| LspError::new("lsp-diagnostics-poisoned", "lock"))?
                .insert(uri.to_owned(), diagnostics);
        }
        Ok(())
    }

    fn shutdown(&self) -> Result<(), LspError> {
        let _ = self.transport.request("shutdown", serde_json::Value::Null);
        let _ = self.transport.notify("exit", serde_json::Value::Null);
        self.transport.close()
    }
}

#[derive(Default)]
struct RawDocumentEdits {
    version: Option<i32>,
    edits: Vec<serde_json::Value>,
}

struct ByteEdit {
    start: usize,
    end: usize,
    new_text: String,
}

fn compute_workspace_files(
    project_root: &Path,
    documents: &Mutex<BTreeMap<PathBuf, OpenDocument>>,
    workspace_edit: &serde_json::Value,
    position_encoding: LspPositionEncoding,
) -> Result<Vec<LspComputedFileEdit>, LspError> {
    let object = workspace_edit
        .as_object()
        .ok_or_else(|| LspError::new("lsp-workspace-edit-invalid", "expected object"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "changes" | "documentChanges" | "changeAnnotations"
        )
    }) || object.get("changeAnnotations").is_some_and(|annotations| {
        !annotations
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    }) {
        return Err(LspError::new(
            "lsp-workspace-edit-unsupported-metadata",
            "unknown fields or change annotations",
        ));
    }
    if object.get("changes").is_some() && object.get("documentChanges").is_some() {
        return Err(LspError::new(
            "lsp-workspace-edit-ambiguous",
            "changes and documentChanges both present",
        ));
    }
    let mut by_uri = BTreeMap::<String, RawDocumentEdits>::new();
    if let Some(changes) = object.get("changes") {
        let changes = changes
            .as_object()
            .ok_or_else(|| LspError::new("lsp-workspace-edit-changes-invalid", "object"))?;
        for (uri, edits) in changes {
            let edits = edits
                .as_array()
                .ok_or_else(|| LspError::new("lsp-workspace-edit-edits-invalid", uri))?;
            by_uri
                .entry(uri.clone())
                .or_default()
                .edits
                .extend(edits.clone());
        }
    } else if let Some(changes) = object.get("documentChanges") {
        let changes = changes
            .as_array()
            .ok_or_else(|| LspError::new("lsp-workspace-edit-document-changes-invalid", "array"))?;
        for change in changes {
            let change = change.as_object().ok_or_else(|| {
                LspError::new("lsp-workspace-edit-document-change-invalid", "object")
            })?;
            if change.get("kind").is_some() || change.get("oldUri").is_some() {
                return Err(LspError::new(
                    "lsp-workspace-resource-operation-denied",
                    "create/rename/delete",
                ));
            }
            if change
                .keys()
                .any(|key| !matches!(key.as_str(), "textDocument" | "edits"))
            {
                return Err(LspError::new(
                    "lsp-workspace-edit-document-change-fields",
                    "unsupported field",
                ));
            }
            let text_document = change
                .get("textDocument")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| LspError::new("lsp-workspace-edit-document-id-missing", "edit"))?;
            let uri = text_document
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| LspError::new("lsp-workspace-edit-uri-missing", "edit"))?;
            let version = match text_document.get("version") {
                None | Some(serde_json::Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_i64()
                        .and_then(|version| i32::try_from(version).ok())
                        .ok_or_else(|| LspError::new("lsp-workspace-edit-version-invalid", uri))?,
                ),
            };
            let edits = change
                .get("edits")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| LspError::new("lsp-workspace-edit-edits-invalid", uri))?;
            let entry = by_uri.entry(uri.to_owned()).or_default();
            if entry.version.is_some() && version.is_some() && entry.version != version {
                return Err(LspError::new("lsp-workspace-edit-version-conflict", uri));
            }
            entry.version = entry.version.or(version);
            entry.edits.extend(edits.clone());
        }
    }
    if by_uri.is_empty() || by_uri.len() > 64 {
        return Err(LspError::new(
            "lsp-workspace-edit-file-count",
            by_uri.len().to_string(),
        ));
    }
    let total_edits = by_uri
        .values()
        .map(|value| value.edits.len())
        .sum::<usize>();
    if total_edits == 0 || total_edits > 2048 {
        return Err(LspError::new(
            "lsp-workspace-edit-count",
            total_edits.to_string(),
        ));
    }
    let open_documents = documents
        .lock()
        .map_err(|_| LspError::new("lsp-documents-poisoned", "lock"))?;
    let root = fs::canonicalize(project_root)
        .map_err(|error| LspError::new("lsp-project-root", error.to_string()))?;
    let mut files = Vec::with_capacity(by_uri.len());
    let mut total_after_bytes = 0_usize;
    for (uri, raw) in by_uri {
        let url = Url::parse(&uri)
            .map_err(|error| LspError::new("lsp-workspace-edit-uri", error.to_string()))?;
        if url.scheme() != "file" {
            return Err(LspError::new("lsp-workspace-edit-non-file-uri", uri));
        }
        let path = url
            .to_file_path()
            .map_err(|()| LspError::new("lsp-workspace-edit-uri", &uri))?;
        let path = fs::canonicalize(path)
            .map_err(|error| LspError::new("lsp-workspace-edit-file", error.to_string()))?;
        if !path.is_file() || !is_inside(&root, &path) {
            return Err(LspError::new(
                "lsp-workspace-edit-path-outside-project",
                path.display().to_string(),
            ));
        }
        let metadata = fs::metadata(&path)
            .map_err(|error| LspError::new("lsp-workspace-edit-file", error.to_string()))?;
        if metadata.len() > MAX_DOCUMENT_BYTES {
            return Err(LspError::new(
                "lsp-workspace-edit-file-too-large",
                metadata.len().to_string(),
            ));
        }
        let before_text = fs::read_to_string(&path)
            .map_err(|error| LspError::new("lsp-workspace-edit-file", error.to_string()))?;
        if let Some(version) = raw.version {
            let open = open_documents.get(&path).ok_or_else(|| {
                LspError::new(
                    "lsp-workspace-edit-version-unverifiable",
                    path.display().to_string(),
                )
            })?;
            if open.version != version {
                return Err(LspError::new(
                    "lsp-workspace-edit-version-stale",
                    format!("expected={}, actual={}", version, open.version),
                ));
            }
        }
        let mut byte_edits = Vec::with_capacity(raw.edits.len());
        for edit in raw.edits {
            let edit = edit
                .as_object()
                .ok_or_else(|| LspError::new("lsp-text-edit-invalid", "object"))?;
            if edit.len() != 2 || !edit.contains_key("range") || !edit.contains_key("newText") {
                return Err(LspError::new(
                    "lsp-text-edit-unsupported-shape",
                    "annotation/snippet/unknown field",
                ));
            }
            let new_text = edit
                .get("newText")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| LspError::new("lsp-text-edit-new-text-invalid", "string"))?;
            if new_text.len() > 1024 * 1024 {
                return Err(LspError::new(
                    "lsp-text-edit-new-text-too-large",
                    new_text.len().to_string(),
                ));
            }
            let range = parse_range(
                edit.get("range")
                    .ok_or_else(|| LspError::new("lsp-text-edit-range-missing", "edit"))?,
            )?;
            let start = protocol_position_to_byte(&before_text, range.start, position_encoding)?;
            let end = protocol_position_to_byte(&before_text, range.end, position_encoding)?;
            if start > end {
                return Err(LspError::new(
                    "lsp-text-edit-range-reversed",
                    format!("{start}>{end}"),
                ));
            }
            if before_text.get(start..end).is_none() {
                return Err(LspError::new(
                    "lsp-text-edit-range-not-boundary",
                    format!("{start}..{end}"),
                ));
            }
            if before_text.get(start..end) == Some(new_text) {
                continue;
            }
            byte_edits.push(ByteEdit {
                start,
                end,
                new_text: new_text.to_owned(),
            });
        }
        byte_edits.sort_by_key(|edit| (edit.start, edit.end));
        for pair in byte_edits.windows(2) {
            if pair[1].start < pair[0].end || pair[1].start == pair[0].start {
                return Err(LspError::new(
                    "lsp-text-edit-overlap",
                    path.display().to_string(),
                ));
            }
        }
        if byte_edits.is_empty() {
            continue;
        }
        let added_bytes = byte_edits.iter().map(|edit| edit.new_text.len()).sum();
        let removed_bytes = byte_edits.iter().map(|edit| edit.end - edit.start).sum();
        let mut after_text = before_text.clone();
        for edit in byte_edits.iter().rev() {
            after_text.replace_range(edit.start..edit.end, &edit.new_text);
        }
        total_after_bytes = total_after_bytes.saturating_add(after_text.len());
        if total_after_bytes > 16 * 1024 * 1024 {
            return Err(LspError::new(
                "lsp-workspace-edit-after-bytes-limit",
                total_after_bytes.to_string(),
            ));
        }
        files.push(LspComputedFileEdit {
            path: path
                .strip_prefix(&root)
                .map_err(|_| LspError::new("lsp-workspace-edit-relative-path", &uri))?
                .to_string_lossy()
                .replace('\\', "/"),
            before_hash: format!("{:x}", Sha256::digest(before_text.as_bytes())),
            after_hash: format!("{:x}", Sha256::digest(after_text.as_bytes())),
            after_text,
            edit_count: byte_edits.len(),
            added_bytes,
            removed_bytes,
        });
    }
    if files.is_empty() {
        return Err(LspError::new(
            "lsp-workspace-edit-no-op",
            "no effective edits",
        ));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn protocol_position_to_byte(
    text: &str,
    position: LspPosition,
    encoding: LspPositionEncoding,
) -> Result<usize, LspError> {
    let human = protocol_to_human_position(text, position, encoding)?;
    let target_line = usize::try_from(human.line - 1)
        .map_err(|_| LspError::new("lsp-byte-line-overflow", human.line.to_string()))?;
    let target_scalar = usize::try_from(human.character - 1)
        .map_err(|_| LspError::new("lsp-byte-character-overflow", human.character.to_string()))?;
    let mut line_start = 0_usize;
    for (line_index, raw) in text.split('\n').enumerate() {
        if line_index == target_line {
            let content = raw.strip_suffix('\r').unwrap_or(raw);
            let in_line = if target_scalar == content.chars().count() {
                content.len()
            } else {
                content
                    .char_indices()
                    .nth(target_scalar)
                    .map(|(offset, _)| offset)
                    .ok_or_else(|| {
                        LspError::new(
                            "lsp-byte-character-out-of-range",
                            human.character.to_string(),
                        )
                    })?
            };
            return Ok(line_start + in_line);
        }
        line_start = line_start.saturating_add(raw.len()).saturating_add(1);
    }
    Err(LspError::new(
        "lsp-byte-line-out-of-range",
        human.line.to_string(),
    ))
}

fn validate_server(server: &LspServerConfig) -> Result<(), LspError> {
    let id_valid = !server.id.is_empty()
        && server.id.len() <= 96
        && server.id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        });
    if !id_valid {
        return Err(LspError::new("lsp-server-id-invalid", &server.id));
    }
    if !server.command.is_absolute() {
        return Err(LspError::new(
            "lsp-command-not-absolute",
            server.command.display().to_string(),
        ));
    }
    if server.language_ids.is_empty()
        || server.language_ids.iter().any(|(extension, language)| {
            extension.is_empty()
                || extension.starts_with('.')
                || extension.len() > 16
                || !extension
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                || language.trim().is_empty()
                || language.len() > 64
        })
    {
        return Err(LspError::new("lsp-language-map-invalid", &server.id));
    }
    if server
        .args
        .iter()
        .any(|arg| arg.len() > 4096 || arg.contains('\0'))
        || server.inherit_env.iter().any(|name| !valid_env_name(name))
    {
        return Err(LspError::new("lsp-process-config-invalid", &server.id));
    }
    if let Some(cwd) = &server.cwd
        && cwd
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(LspError::new(
            "lsp-cwd-parent-denied",
            cwd.display().to_string(),
        ));
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic() || byte == b'_' || (index > 0 && byte.is_ascii_digit())
        })
}

fn capability_view(value: Option<&serde_json::Value>) -> LspCapabilityView {
    let value = value.unwrap_or(&serde_json::Value::Null);
    LspCapabilityView {
        document_symbols: capability_enabled(value.get("documentSymbolProvider")),
        definition: capability_enabled(value.get("definitionProvider")),
        references: capability_enabled(value.get("referencesProvider")),
        diagnostics: true,
        rename: capability_enabled(value.get("renameProvider")),
        prepare_rename: value
            .get("renameProvider")
            .and_then(|provider| provider.get("prepareProvider"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        code_action: capability_enabled(value.get("codeActionProvider")),
        code_action_resolve: value
            .get("codeActionProvider")
            .and_then(|provider| provider.get("resolveProvider"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        position_encoding: match value
            .get("positionEncoding")
            .and_then(serde_json::Value::as_str)
        {
            Some("utf-8") => LspPositionEncoding::Utf8,
            Some("utf-32") => LspPositionEncoding::Utf32,
            _ => LspPositionEncoding::Utf16,
        },
    }
}

fn capability_enabled(value: Option<&serde_json::Value>) -> bool {
    matches!(
        value,
        Some(serde_json::Value::Bool(true) | serde_json::Value::Object(_))
    )
}

fn normalize_symbols(
    project_root: &Path,
    document_uri: &str,
    value: serde_json::Value,
    position_encoding: LspPositionEncoding,
) -> Result<Vec<LspSymbol>, LspError> {
    let items = match value {
        serde_json::Value::Null => return Ok(vec![]),
        serde_json::Value::Array(items) => items,
        _ => return Err(LspError::new("lsp-symbol-result-invalid", "expected array")),
    };
    let mut output = Vec::new();
    for item in &items {
        flatten_symbol(
            project_root,
            document_uri,
            None,
            item,
            position_encoding,
            &mut output,
        )?;
    }
    Ok(output)
}

fn flatten_symbol(
    project_root: &Path,
    document_uri: &str,
    parent: Option<&str>,
    value: &serde_json::Value,
    position_encoding: LspPositionEncoding,
    output: &mut Vec<LspSymbol>,
) -> Result<(), LspError> {
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| LspError::new("lsp-symbol-name-missing", "symbol"))?;
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_u64)
        .and_then(|kind| u32::try_from(kind).ok())
        .unwrap_or(0);
    let (uri, range_value) = if let Some(location) = value.get("location") {
        (
            location
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(document_uri),
            location.get("range"),
        )
    } else {
        (
            document_uri,
            value.get("selectionRange").or_else(|| value.get("range")),
        )
    };
    let range =
        parse_range(range_value.ok_or_else(|| LspError::new("lsp-symbol-range-missing", name))?)?;
    let (path, external) = uri_to_relative(project_root, uri);
    let human_range = human_range_for_uri(project_root, uri, range, position_encoding);
    let container_name = value
        .get("containerName")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| parent.map(str::to_owned));
    output.push(LspSymbol {
        name: name.to_owned(),
        detail: value
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        kind,
        container_name,
        location: LspLocation {
            uri: uri.to_owned(),
            path,
            range,
            human_range,
            position_encoding,
            external,
        },
    });
    if let Some(children) = value.get("children").and_then(serde_json::Value::as_array) {
        for child in children {
            flatten_symbol(
                project_root,
                document_uri,
                Some(name),
                child,
                position_encoding,
                output,
            )?;
        }
    }
    Ok(())
}

fn normalize_locations(
    project_root: &Path,
    value: serde_json::Value,
    position_encoding: LspPositionEncoding,
) -> Result<Vec<LspLocation>, LspError> {
    let items = match value {
        serde_json::Value::Null => return Ok(vec![]),
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(_) => vec![value],
        _ => return Err(LspError::new("lsp-location-result-invalid", "result")),
    };
    items
        .iter()
        .map(|item| {
            let uri = item
                .get("uri")
                .or_else(|| item.get("targetUri"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| LspError::new("lsp-location-uri-missing", "location"))?;
            let range = item
                .get("range")
                .or_else(|| item.get("targetSelectionRange"))
                .or_else(|| item.get("targetRange"))
                .ok_or_else(|| LspError::new("lsp-location-range-missing", uri))?;
            let (path, external) = uri_to_relative(project_root, uri);
            let range = parse_range(range)?;
            Ok(LspLocation {
                uri: uri.to_owned(),
                path,
                range,
                human_range: human_range_for_uri(project_root, uri, range, position_encoding),
                position_encoding,
                external,
            })
        })
        .collect()
}

fn normalize_diagnostic(
    project_root: &Path,
    uri: &str,
    path: Option<String>,
    value: &serde_json::Value,
    position_encoding: LspPositionEncoding,
) -> Result<LspDiagnostic, LspError> {
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| LspError::new("lsp-diagnostic-message-missing", "diagnostic"))?;
    let code = value.get("code").and_then(|code| match code {
        serde_json::Value::String(code) => Some(code.clone()),
        serde_json::Value::Number(code) => Some(code.to_string()),
        _ => None,
    });
    let range = parse_range(
        value
            .get("range")
            .ok_or_else(|| LspError::new("lsp-diagnostic-range-missing", message))?,
    )?;
    Ok(LspDiagnostic {
        path,
        range,
        human_range: human_range_for_uri(project_root, uri, range, position_encoding),
        position_encoding,
        severity: value
            .get("severity")
            .and_then(serde_json::Value::as_u64)
            .and_then(|severity| u32::try_from(severity).ok()),
        code,
        source: value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        message: message.chars().take(4096).collect(),
    })
}

fn parse_range(value: &serde_json::Value) -> Result<LspRange, LspError> {
    Ok(LspRange {
        start: parse_position(
            value
                .get("start")
                .ok_or_else(|| LspError::new("lsp-range-start-missing", "range"))?,
        )?,
        end: parse_position(
            value
                .get("end")
                .ok_or_else(|| LspError::new("lsp-range-end-missing", "range"))?,
        )?,
    })
}

fn parse_position(value: &serde_json::Value) -> Result<LspPosition, LspError> {
    let line = value
        .get("line")
        .and_then(serde_json::Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
        .ok_or_else(|| LspError::new("lsp-position-line-invalid", "position"))?;
    let character = value
        .get("character")
        .and_then(serde_json::Value::as_u64)
        .and_then(|character| u32::try_from(character).ok())
        .ok_or_else(|| LspError::new("lsp-position-character-invalid", "position"))?;
    Ok(LspPosition { line, character })
}

/// 把用户看到的 1-based Unicode scalar 列转换为 Server 协商的 LSP code units。
pub fn human_to_protocol_position(
    text: &str,
    line: u32,
    character: u32,
    encoding: LspPositionEncoding,
) -> Result<LspPosition, LspError> {
    if line == 0 || character == 0 {
        return Err(LspError::new(
            "lsp-human-position-zero",
            format!("line={line}, character={character}"),
        ));
    }
    let line_index = usize::try_from(line - 1)
        .map_err(|_| LspError::new("lsp-human-line-overflow", line.to_string()))?;
    let content = text
        .split('\n')
        .nth(line_index)
        .ok_or_else(|| LspError::new("lsp-human-line-out-of-range", line.to_string()))?
        .strip_suffix('\r')
        .unwrap_or_else(|| text.split('\n').nth(line_index).unwrap_or_default());
    let scalar_index = usize::try_from(character - 1)
        .map_err(|_| LspError::new("lsp-human-character-overflow", character.to_string()))?;
    if scalar_index > content.chars().count() {
        return Err(LspError::new(
            "lsp-human-character-out-of-range",
            format!("line={line}, character={character}"),
        ));
    }
    let units = content
        .chars()
        .take(scalar_index)
        .map(|value| encoded_units(value, encoding))
        .sum::<usize>();
    Ok(LspPosition {
        line: line - 1,
        character: u32::try_from(units)
            .map_err(|_| LspError::new("lsp-protocol-character-overflow", units.to_string()))?,
    })
}

/// 把 Server 的 0-based code units 转回 1-based Unicode scalar 列。
pub fn protocol_to_human_position(
    text: &str,
    position: LspPosition,
    encoding: LspPositionEncoding,
) -> Result<HumanPosition, LspError> {
    let line_index = usize::try_from(position.line)
        .map_err(|_| LspError::new("lsp-protocol-line-overflow", position.line.to_string()))?;
    let raw = text.split('\n').nth(line_index).ok_or_else(|| {
        LspError::new("lsp-protocol-line-out-of-range", position.line.to_string())
    })?;
    let content = raw.strip_suffix('\r').unwrap_or(raw);
    let target = usize::try_from(position.character).map_err(|_| {
        LspError::new(
            "lsp-protocol-character-overflow",
            position.character.to_string(),
        )
    })?;
    let mut consumed = 0_usize;
    let mut scalars = 0_usize;
    if target == 0 {
        return Ok(HumanPosition {
            line: position.line + 1,
            character: 1,
        });
    }
    for value in content.chars() {
        let next = consumed.saturating_add(encoded_units(value, encoding));
        if target < next {
            return Err(LspError::new(
                "lsp-protocol-character-mid-codepoint",
                format!("line={}, character={}", position.line, position.character),
            ));
        }
        consumed = next;
        scalars += 1;
        if consumed == target {
            return Ok(HumanPosition {
                line: position.line + 1,
                character: u32::try_from(scalars + 1).map_err(|_| {
                    LspError::new("lsp-human-character-overflow", scalars.to_string())
                })?,
            });
        }
    }
    Err(LspError::new(
        "lsp-protocol-character-out-of-range",
        format!("line={}, character={}", position.line, position.character),
    ))
}

const fn encoded_units(value: char, encoding: LspPositionEncoding) -> usize {
    match encoding {
        LspPositionEncoding::Utf8 => value.len_utf8(),
        LspPositionEncoding::Utf16 => value.len_utf16(),
        LspPositionEncoding::Utf32 => 1,
    }
}

fn human_range_for_uri(
    project_root: &Path,
    uri: &str,
    range: LspRange,
    encoding: LspPositionEncoding,
) -> Option<HumanRange> {
    let path = Url::parse(uri).ok()?.to_file_path().ok()?;
    let path = fs::canonicalize(path).ok()?;
    let root = fs::canonicalize(project_root).ok()?;
    if !is_inside(&root, &path) || fs::metadata(&path).ok()?.len() > MAX_DOCUMENT_BYTES {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    Some(HumanRange {
        start: protocol_to_human_position(&text, range.start, encoding).ok()?,
        end: protocol_to_human_position(&text, range.end, encoding).ok()?,
    })
}

fn uri_to_relative(project_root: &Path, uri: &str) -> (Option<String>, bool) {
    let Ok(url) = Url::parse(uri) else {
        return (None, true);
    };
    let Ok(path) = url.to_file_path() else {
        return (None, true);
    };
    let path = fs::canonicalize(&path).unwrap_or(path);
    let project_root =
        fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    if is_inside(&project_root, &path) {
        let relative = path
            .strip_prefix(&project_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        (Some(relative), false)
    } else {
        (Some(path.to_string_lossy().into_owned()), true)
    }
}

fn is_inside(root: &Path, target: &Path) -> bool {
    target == root || target.starts_with(root)
}

#[derive(Clone, Copy)]
enum LspToolKind {
    Symbols,
    Definition,
    References,
    Diagnostics,
}

struct LspToolProvider {
    manager: LspManager,
    kind: LspToolKind,
}

impl ToolProvider for LspToolProvider {
    fn validate_args(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let object = value
            .as_object()
            .ok_or_else(|| ToolError::new("lsp-tool-args-invalid", "expected object"))?;
        let server_id = object
            .get("serverId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ToolError::new("lsp-tool-server-missing", "serverId"))?;
        let path = object
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ToolError::new("lsp-tool-path-missing", "path"))?;
        if server_id.len() > 96 || path.len() > 4096 || path.contains('\0') {
            return Err(ToolError::new(
                "lsp-tool-args-too-large",
                server_id.to_owned(),
            ));
        }
        self.manager
            .process_spec(server_id)
            .map_err(lsp_tool_error)?;
        if matches!(self.kind, LspToolKind::Definition | LspToolKind::References) {
            for field in ["line", "character"] {
                if object
                    .get(field)
                    .and_then(serde_json::Value::as_u64)
                    .filter(|value| *value > 0 && *value <= u64::from(u32::MAX))
                    .is_none()
                {
                    return Err(ToolError::new(
                        "lsp-tool-position-invalid",
                        format!("{field} must be 1-based u32"),
                    ));
                }
            }
        }
        Ok(value.clone())
    }

    fn validate_result(&self, value: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if value
            .get("serverId")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || value.get("source").and_then(serde_json::Value::as_str) != Some("lsp")
            || value.get("integrity").and_then(serde_json::Value::as_str) != Some("untrusted")
            || value
                .get("fileHash")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|hash| hash.len() != 64)
            || value
                .get("documentVersion")
                .and_then(serde_json::Value::as_i64)
                .is_none()
            || value
                .get("positionEncoding")
                .and_then(serde_json::Value::as_str)
                .is_none()
            || value
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || value
                .get("returned")
                .and_then(serde_json::Value::as_u64)
                .is_none()
            || value
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .is_none()
            || value
                .get("facts")
                .and_then(serde_json::Value::as_array)
                .is_none()
        {
            return Err(ToolError::new(
                "lsp-tool-result-invalid",
                "missing normalized fields",
            ));
        }
        Ok(value.clone())
    }

    fn permission_action(&self, args: &serde_json::Value) -> Result<PermissionAction, ToolError> {
        let server_id = args["serverId"].as_str().expect("validated serverId");
        let spec = self
            .manager
            .process_spec(server_id)
            .map_err(lsp_tool_error)?;
        Ok(PermissionAction::ProcessSpawn {
            executable: spec.executable,
            arguments: spec.arguments,
            cwd: spec.cwd,
        })
    }

    fn execute(&self, input: ToolExecutionInput) -> Result<serde_json::Value, ToolError> {
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new("tool-cancelled", "LSP query cancelled"));
        }
        let server_id = input.args["serverId"].as_str().expect("validated serverId");
        let path = Path::new(input.args["path"].as_str().expect("validated path"));
        let (mut facts, limit) = match self.kind {
            LspToolKind::Symbols => (
                serde_json::to_value(
                    self.manager
                        .document_symbols(server_id, path)
                        .map_err(lsp_tool_error)?,
                )
                .map_err(json_tool_error)?,
                256,
            ),
            LspToolKind::Definition => (
                serde_json::to_value(
                    self.manager
                        .definition(
                            server_id,
                            path,
                            position_value(&input.args, "line")?,
                            position_value(&input.args, "character")?,
                        )
                        .map_err(lsp_tool_error)?,
                )
                .map_err(json_tool_error)?,
                64,
            ),
            LspToolKind::References => (
                serde_json::to_value(
                    self.manager
                        .references(
                            server_id,
                            path,
                            position_value(&input.args, "line")?,
                            position_value(&input.args, "character")?,
                        )
                        .map_err(lsp_tool_error)?,
                )
                .map_err(json_tool_error)?,
                512,
            ),
            LspToolKind::Diagnostics => (
                serde_json::to_value(
                    self.manager
                        .diagnostics(server_id, path)
                        .map_err(lsp_tool_error)?,
                )
                .map_err(json_tool_error)?,
                256,
            ),
        };
        if input.cancellation.is_cancelled() {
            return Err(ToolError::new("tool-cancelled", "LSP query cancelled"));
        }
        let snapshot = self
            .manager
            .document_snapshot(server_id, path)
            .map_err(lsp_tool_error)?;
        let array = facts
            .as_array_mut()
            .expect("LSP normalized result serializes as array");
        let count = array.len();
        array.truncate(limit);
        Ok(serde_json::json!({
            "serverId":server_id,
            "path":snapshot.path,
            "source":"lsp",
            "integrity":"untrusted",
            "fileHash":snapshot.file_hash,
            "documentVersion":snapshot.document_version,
            "positionEncoding":snapshot.position_encoding,
            "count":count,
            "returned":array.len(),
            "truncated":count > array.len(),
            "facts":facts
        }))
    }
}

fn position_value(args: &serde_json::Value, field: &str) -> Result<u32, ToolError> {
    args[field]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| ToolError::new("lsp-tool-position-invalid", field))
}

fn lsp_tool_error(error: LspError) -> ToolError {
    ToolError::new(error.code, error.message)
}

fn json_tool_error(error: serde_json::Error) -> ToolError {
    ToolError::new("lsp-tool-json", error.to_string())
}

/// 注册四个 metadata-only、on-demand、read-only Tool；注册本身不会启动 LSP。
pub fn register_lsp_tools(registry: &ToolRegistry, manager: LspManager) -> Result<(), ToolError> {
    for (name, kind, description, keywords, position) in [
        (
            "lsp.symbols",
            LspToolKind::Symbols,
            "通过配置的 Language Server 获取文档符号；只读且按需启动",
            vec!["symbol", "符号", "outline", "结构"],
            false,
        ),
        (
            "lsp.definition",
            LspToolKind::Definition,
            "查询指定位置的精确定义；行列使用 1-based",
            vec!["definition", "定义", "goto", "类型"],
            true,
        ),
        (
            "lsp.references",
            LspToolKind::References,
            "查询指定位置的精确引用；行列使用 1-based",
            vec!["references", "引用", "usages", "调用"],
            true,
        ),
        (
            "lsp.diagnostics",
            LspToolKind::Diagnostics,
            "获取 Language Server 发布的错误与警告；只读",
            vec!["diagnostics", "诊断", "error", "warning", "编译"],
            false,
        ),
    ] {
        let mut properties = serde_json::json!({
            "serverId":{"type":"string","description":"kernary.lsp.toml 中的 server id"},
            "path":{"type":"string","description":"workspace 内相对或绝对文件路径"}
        });
        let mut required = vec!["serverId", "path"];
        if position {
            properties["line"] = serde_json::json!({"type":"integer","minimum":1});
            properties["character"] = serde_json::json!({"type":"integer","minimum":1});
            required.extend(["line", "character"]);
        }
        registry.register(
            ToolDescriptor {
                canonical_name: name.to_owned(),
                version: "1".to_owned(),
                description: description.to_owned(),
                effect_class: ToolEffectClass::ReadOnlyRetryable,
                source: ToolSource::Builtin,
                prompt_loading: ToolPromptLoading::OnDemand,
                keywords: keywords.into_iter().map(str::to_owned).collect(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "properties":properties,
                    "required":required,
                    "additionalProperties":false
                }),
                output_schema: serde_json::json!({
                    "type":"object",
                    "properties":{
                        "serverId":{"type":"string"},
                        "path":{"type":"string"},
                        "source":{"const":"lsp"},
                        "integrity":{"const":"untrusted"},
                        "fileHash":{"type":"string","minLength":64,"maxLength":64},
                        "documentVersion":{"type":"integer","minimum":1},
                        "positionEncoding":{"enum":["utf-8","utf-16","utf-32"]},
                        "count":{"type":"integer"},
                        "returned":{"type":"integer"},
                        "truncated":{"type":"boolean"},
                        "facts":{"type":"array"}
                    },
                    "required":["serverId","path","source","integrity","fileHash","documentVersion","positionEncoding","count","returned","truncated","facts"],
                    "additionalProperties":false
                }),
            },
            Arc::new(LspToolProvider {
                manager: manager.clone(),
                kind,
            }),
        )?;
    }
    Ok(())
}

#[must_use]
pub fn default_lsp_config_path(project_root: &Path) -> PathBuf {
    project_root.join("kernary.lsp.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_absolute_command_and_strict_language_map() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let error = match LspManager::new(
            temporary.path(),
            vec![LspServerConfig {
                id: "rust".to_owned(),
                command: PathBuf::from("rust-analyzer"),
                args: vec![],
                cwd: None,
                language_ids: [("rs".to_owned(), "rust".to_owned())].into_iter().collect(),
                inherit_env: vec![],
                initialization_options: None,
                request_timeout_millis: None,
                max_message_bytes: None,
            }],
        ) {
            Err(error) => error,
            Ok(_) => panic!("relative command denied"),
        };
        assert_eq!(error.code, "lsp-command-not-absolute");
    }

    #[test]
    fn location_normalization_marks_external_files() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path();
        let inside = root.join("src/main.rs");
        fs::create_dir_all(inside.parent().expect("parent")).expect("mkdir");
        fs::write(&inside, "fn main() {}\n").expect("write");
        let uri = Url::from_file_path(&inside).expect("uri").to_string();
        let locations = normalize_locations(
            root,
            serde_json::json!({
                "uri":uri,
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}
            }),
            LspPositionEncoding::Utf16,
        )
        .expect("locations");
        assert_eq!(locations[0].path.as_deref(), Some("src/main.rs"));
        assert!(!locations[0].external);
    }

    #[test]
    fn unicode_scalar_and_utf8_utf16_utf32_positions_round_trip_exactly() {
        let text = "a😀中z\r\ne\u{301}x\n";
        for (encoding, expected) in [
            (LspPositionEncoding::Utf8, 5),
            (LspPositionEncoding::Utf16, 3),
            (LspPositionEncoding::Utf32, 2),
        ] {
            let protocol = human_to_protocol_position(text, 1, 3, encoding).expect("to protocol");
            assert_eq!(
                protocol,
                LspPosition {
                    line: 0,
                    character: expected
                }
            );
            assert_eq!(
                protocol_to_human_position(text, protocol, encoding).expect("to human"),
                HumanPosition {
                    line: 1,
                    character: 3
                }
            );
        }
        assert_eq!(
            human_to_protocol_position(text, 2, 3, LspPositionEncoding::Utf16)
                .expect("combining mark is a distinct scalar"),
            LspPosition {
                line: 1,
                character: 2
            }
        );
        assert_eq!(
            protocol_to_human_position(
                text,
                LspPosition {
                    line: 0,
                    character: 2
                },
                LspPositionEncoding::Utf16,
            )
            .expect_err("middle of surrogate pair")
            .code,
            "lsp-protocol-character-mid-codepoint"
        );
        assert_eq!(
            protocol_to_human_position(
                text,
                LspPosition {
                    line: 0,
                    character: 2
                },
                LspPositionEncoding::Utf8,
            )
            .expect_err("middle of utf8 code point")
            .code,
            "lsp-protocol-character-mid-codepoint"
        );
        assert_eq!(
            human_to_protocol_position(text, 1, 99, LspPositionEncoding::Utf16)
                .expect_err("out of range")
                .code,
            "lsp-human-character-out-of-range"
        );
    }

    #[test]
    fn workspace_edit_is_multifile_versioned_and_rejects_overlap_resources_and_snippets() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path();
        let first = root.join("a.rs");
        let second = root.join("b.rs");
        fs::write(&first, "a😀z\n").expect("first");
        fs::write(&second, "hello\n").expect("second");
        let first = fs::canonicalize(first).expect("canonical first");
        let first_uri = Url::from_file_path(&first).expect("uri").to_string();
        let second_uri = Url::from_file_path(&second).expect("uri").to_string();
        let documents = Mutex::new(BTreeMap::from([(
            first.clone(),
            OpenDocument {
                uri: first_uri.clone(),
                version: 1,
                sha256: format!("{:x}", Sha256::digest("a😀z\n".as_bytes())),
                text: "a😀z\n".to_owned(),
            },
        )]));
        let edit = serde_json::json!({
            "documentChanges":[{
                "textDocument":{"uri":first_uri,"version":1},
                "edits":[{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"X"}]
            },{
                "textDocument":{"uri":second_uri,"version":null},
                "edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"newText":"world"}]
            }]
        });
        let files = compute_workspace_files(root, &documents, &edit, LspPositionEncoding::Utf16)
            .expect("computed");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].after_text, "aXz\n");
        assert_eq!(files[1].after_text, "world\n");

        let overlap = serde_json::json!({"changes":{
            first_uri.clone():[
                {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"x"},
                {"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"y"}
            ]
        }});
        assert_eq!(
            compute_workspace_files(root, &documents, &overlap, LspPositionEncoding::Utf16)
                .expect_err("overlap")
                .code,
            "lsp-text-edit-overlap"
        );
        let resource = serde_json::json!({"documentChanges":[{
            "kind":"create",
            "uri":second_uri
        }]});
        assert_eq!(
            compute_workspace_files(root, &documents, &resource, LspPositionEncoding::Utf16)
                .expect_err("resource denied")
                .code,
            "lsp-workspace-resource-operation-denied"
        );
        let stale = serde_json::json!({"documentChanges":[{
            "textDocument":{"uri":first_uri.clone(),"version":99},
            "edits":[{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"x"}]
        }]});
        assert_eq!(
            compute_workspace_files(root, &documents, &stale, LspPositionEncoding::Utf16)
                .expect_err("stale")
                .code,
            "lsp-workspace-edit-version-stale"
        );
        let snippet = serde_json::json!({"changes":{
            first_uri:[{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"x","insertTextFormat":2}]
        }});
        assert_eq!(
            compute_workspace_files(root, &documents, &snippet, LspPositionEncoding::Utf16)
                .expect_err("snippet denied")
                .code,
            "lsp-text-edit-unsupported-shape"
        );
    }
}
