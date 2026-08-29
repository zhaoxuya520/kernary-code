#![forbid(unsafe_code)]

//! Skill 是可加载的工作流/能力说明，不是可执行 Plugin。发现阶段只读 `skill.toml`。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use harness_tool::ToolRegistry;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_PROMPT_BYTES: usize = 512 * 1024;
const MAX_REFERENCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_REFERENCE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillError {
    pub code: String,
    pub message: String,
}

impl SkillError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for SkillError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for SkillError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SkillSource {
    Project,
    User,
    Plugin { plugin_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillStatus {
    MetadataOnly,
    Loaded,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub entry: PathBuf,
    #[serde(default)]
    pub references: Vec<PathBuf>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub required_tools: Vec<String>,
    #[serde(default)]
    pub workflows: Vec<String>,
    #[serde(default)]
    pub agent_templates: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub source: SkillSource,
    pub status: SkillStatus,
    pub tags: Vec<String>,
    pub required_tools: Vec<String>,
    pub reference_count: usize,
    pub metadata_hash: String,
    pub content_hash: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiscoveryError {
    pub manifest_path: PathBuf,
    pub error: SkillError,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillDiscoveryReport {
    pub skills: Vec<SkillView>,
    pub errors: Vec<SkillDiscoveryError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSkill {
    pub view: SkillView,
    pub prompt: String,
    pub references: BTreeMap<PathBuf, String>,
    pub workflows: Vec<String>,
    pub agent_templates: Vec<String>,
}

struct InstalledSkill {
    root: PathBuf,
    manifest: SkillManifest,
    source: SkillSource,
    metadata_hash: String,
    loaded: Option<LoadedSkill>,
    last_error: Option<String>,
}

pub struct SkillRegistry {
    tools: ToolRegistry,
    skills: Mutex<BTreeMap<String, InstalledSkill>>,
}

impl SkillRegistry {
    #[must_use]
    pub fn new(tools: ToolRegistry) -> Self {
        Self {
            tools,
            skills: Mutex::new(BTreeMap::new()),
        }
    }

    /// 只读取 metadata manifest；不会读取 SKILL.md 或 references。
    pub fn discover(&self, roots: &[(PathBuf, SkillSource)]) -> Result<Vec<SkillView>, SkillError> {
        let report = self.discover_isolated(roots)?;
        if let Some(error) = report.errors.into_iter().next() {
            return Err(error.error);
        }
        Ok(report.skills)
    }

    pub fn discover_isolated(
        &self,
        roots: &[(PathBuf, SkillSource)],
    ) -> Result<SkillDiscoveryReport, SkillError> {
        let mut manifests = Vec::new();
        for (root, source) in roots {
            if !root.exists() {
                continue;
            }
            let root = fs::canonicalize(root)
                .map_err(|error| SkillError::new("skill-root-canonicalize", error.to_string()))?;
            for entry in fs::read_dir(&root)
                .map_err(|error| SkillError::new("skill-discovery-read", error.to_string()))?
            {
                let entry = entry
                    .map_err(|error| SkillError::new("skill-discovery-read", error.to_string()))?;
                if !entry
                    .file_type()
                    .map_err(|error| SkillError::new("skill-discovery-type", error.to_string()))?
                    .is_dir()
                {
                    continue;
                }
                let manifest = entry.path().join("skill.toml");
                if manifest.is_file() {
                    manifests.push((manifest, source.clone()));
                }
            }
        }
        manifests.sort_by(|left, right| left.0.cmp(&right.0));
        let mut report = SkillDiscoveryReport::default();
        for (manifest, source) in manifests {
            match self.install(&manifest, source) {
                Ok(skill) => report.skills.push(skill),
                Err(error) => report.errors.push(SkillDiscoveryError {
                    manifest_path: manifest,
                    error,
                }),
            }
        }
        Ok(report)
    }

    pub fn install(
        &self,
        manifest_path: &Path,
        source: SkillSource,
    ) -> Result<SkillView, SkillError> {
        let manifest_path = fs::canonicalize(manifest_path)
            .map_err(|error| SkillError::new("skill-manifest-path", error.to_string()))?;
        let root = manifest_path
            .parent()
            .ok_or_else(|| {
                SkillError::new("skill-root-missing", manifest_path.display().to_string())
            })?
            .to_path_buf();
        let bytes = fs::read(&manifest_path)
            .map_err(|error| SkillError::new("skill-manifest-read", error.to_string()))?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(SkillError::new(
                "skill-manifest-too-large",
                bytes.len().to_string(),
            ));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            SkillError::new(
                "skill-manifest-not-utf8",
                manifest_path.display().to_string(),
            )
        })?;
        let manifest: SkillManifest = toml::from_str(text)
            .map_err(|error| SkillError::new("skill-manifest-toml", error.to_string()))?;
        validate_manifest(&manifest)?;
        validate_declared_path(&root, &manifest.entry)?;
        for reference in &manifest.references {
            validate_declared_path(&root, reference)?;
        }
        let metadata_hash = format!("{:x}", Sha256::digest(&bytes));
        let installed = InstalledSkill {
            root,
            manifest,
            source,
            metadata_hash,
            loaded: None,
            last_error: None,
        };
        let view = skill_view(&installed);
        let mut skills = self
            .skills
            .lock()
            .map_err(|_| SkillError::new("skill-registry-poisoned", "skills"))?;
        if skills.contains_key(&view.id) {
            return Err(SkillError::new("skill-already-installed", view.id));
        }
        skills.insert(view.id.clone(), installed);
        Ok(view)
    }

    pub fn load(
        &self,
        skill_id: &str,
        requested_references: Option<&[PathBuf]>,
    ) -> Result<LoadedSkill, SkillError> {
        let mut skills = self
            .skills
            .lock()
            .map_err(|_| SkillError::new("skill-registry-poisoned", "skills"))?;
        let skill = skills
            .get_mut(skill_id)
            .ok_or_else(|| SkillError::new("skill-not-found", skill_id))?;
        if requested_references.is_none()
            && let Some(loaded) = &skill.loaded
        {
            return Ok(loaded.clone());
        }
        let available_tools = self
            .tools
            .try_list()
            .map_err(|error| SkillError::new(error.code, error.message))?
            .into_iter()
            .map(|tool| tool.canonical_name)
            .collect::<Vec<_>>();
        let missing = skill
            .manifest
            .required_tools
            .iter()
            .filter(|required| !available_tools.iter().any(|tool| tool == *required))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let error = SkillError::new("skill-required-tools-missing", missing.join(","));
            skill.last_error = Some(error.to_string());
            return Err(error);
        }
        let prompt_path = resolve_inside(&skill.root, &skill.manifest.entry)?;
        let prompt = read_utf8(&prompt_path, MAX_PROMPT_BYTES, "skill-prompt")?;
        let references = requested_references.unwrap_or(&skill.manifest.references);
        let declared = skill
            .manifest
            .references
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut loaded_references = BTreeMap::new();
        let mut total = 0_usize;
        for reference in references {
            if !declared.contains(&reference.to_string_lossy().into_owned()) {
                return Err(SkillError::new(
                    "skill-reference-not-declared",
                    reference.display().to_string(),
                ));
            }
            let path = resolve_inside(&skill.root, reference)?;
            let content = read_utf8(&path, MAX_REFERENCE_BYTES, "skill-reference")?;
            total = total.saturating_add(content.len());
            if total > MAX_TOTAL_REFERENCE_BYTES {
                return Err(SkillError::new(
                    "skill-references-too-large",
                    total.to_string(),
                ));
            }
            loaded_references.insert(reference.clone(), content);
        }
        let content_hash = hash_loaded(&prompt, &loaded_references);
        let mut view = skill_view(skill);
        view.status = SkillStatus::Loaded;
        view.content_hash = Some(content_hash);
        view.last_error = None;
        let loaded = LoadedSkill {
            view,
            prompt,
            references: loaded_references,
            workflows: skill.manifest.workflows.clone(),
            agent_templates: skill.manifest.agent_templates.clone(),
        };
        if requested_references.is_none() {
            skill.loaded = Some(loaded.clone());
        }
        skill.last_error = None;
        Ok(loaded)
    }

    pub fn unload(&self, skill_id: &str) -> Result<SkillView, SkillError> {
        let mut skills = self
            .skills
            .lock()
            .map_err(|_| SkillError::new("skill-registry-poisoned", "skills"))?;
        let skill = skills
            .get_mut(skill_id)
            .ok_or_else(|| SkillError::new("skill-not-found", skill_id))?;
        skill.loaded = None;
        skill.last_error = None;
        Ok(skill_view(skill))
    }

    pub fn uninstall(
        &self,
        skill_id: &str,
        expected_source: &SkillSource,
    ) -> Result<bool, SkillError> {
        let mut skills = self
            .skills
            .lock()
            .map_err(|_| SkillError::new("skill-registry-poisoned", "skills"))?;
        let Some(skill) = skills.get(skill_id) else {
            return Ok(false);
        };
        if &skill.source != expected_source {
            return Err(SkillError::new("skill-source-mismatch", skill_id));
        }
        skills.remove(skill_id);
        Ok(true)
    }

    pub fn list(&self) -> Result<Vec<SkillView>, SkillError> {
        Ok(self
            .skills
            .lock()
            .map_err(|_| SkillError::new("skill-registry-poisoned", "skills"))?
            .values()
            .map(skill_view)
            .collect())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SkillView>, SkillError> {
        let tokens = search_tokens(query);
        let skills = self
            .skills
            .lock()
            .map_err(|_| SkillError::new("skill-registry-poisoned", "skills"))?;
        let mut scored = skills
            .values()
            .filter_map(|skill| {
                let view = skill_view(skill);
                let text = format!(
                    "{} {} {} {}",
                    view.id,
                    view.name,
                    view.description,
                    view.tags.join(" ")
                )
                .to_lowercase();
                let score = tokens
                    .iter()
                    .filter(|token| text.contains(token.as_str()))
                    .count();
                (score > 0).then_some((score, view))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, view)| view)
            .collect())
    }
}

fn validate_manifest(manifest: &SkillManifest) -> Result<(), SkillError> {
    validate_id(&manifest.id)?;
    if manifest.name.trim().is_empty()
        || manifest.description.trim().is_empty()
        || manifest.entry.as_os_str().is_empty()
    {
        return Err(SkillError::new(
            "skill-manifest-field-invalid",
            &manifest.id,
        ));
    }
    Version::parse(&manifest.version)
        .map_err(|error| SkillError::new("skill-version-invalid", error.to_string()))?;
    let mut references = BTreeMap::new();
    for reference in &manifest.references {
        let normalized = reference.to_string_lossy().into_owned();
        if references.insert(normalized.clone(), ()).is_some() {
            return Err(SkillError::new("skill-reference-duplicate", normalized));
        }
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), SkillError> {
    if value.is_empty()
        || value.len() > 128
        || value
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    {
        return Err(SkillError::new("skill-id-invalid", value));
    }
    Ok(())
}

fn validate_declared_path(root: &Path, relative: &Path) -> Result<(), SkillError> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(SkillError::new(
            "skill-path-invalid",
            relative.display().to_string(),
        ));
    }
    let joined = root.join(relative);
    if !joined.is_file() {
        return Err(SkillError::new(
            "skill-path-not-file",
            joined.display().to_string(),
        ));
    }
    Ok(())
}

fn resolve_inside(root: &Path, relative: &Path) -> Result<PathBuf, SkillError> {
    validate_declared_path(root, relative)?;
    let root = fs::canonicalize(root)
        .map_err(|error| SkillError::new("skill-root-canonicalize", error.to_string()))?;
    let path = fs::canonicalize(root.join(relative))
        .map_err(|error| SkillError::new("skill-path-canonicalize", error.to_string()))?;
    let root_key = normalize_path(&root);
    let path_key = normalize_path(&path);
    if path_key != root_key
        && !path_key.starts_with(&format!("{root_key}{}", std::path::MAIN_SEPARATOR))
    {
        return Err(SkillError::new(
            "skill-path-outside-root",
            path.display().to_string(),
        ));
    }
    Ok(path)
}

fn read_utf8(path: &Path, limit: usize, code: &'static str) -> Result<String, SkillError> {
    let metadata = fs::metadata(path)
        .map_err(|error| SkillError::new(format!("{code}-metadata"), error.to_string()))?;
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size > limit {
        return Err(SkillError::new(
            format!("{code}-too-large"),
            size.to_string(),
        ));
    }
    fs::read_to_string(path)
        .map_err(|error| SkillError::new(format!("{code}-read"), error.to_string()))
}

fn hash_loaded(prompt: &str, references: &BTreeMap<PathBuf, String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    for (path, content) in references {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(content.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn skill_view(skill: &InstalledSkill) -> SkillView {
    SkillView {
        id: skill.manifest.id.clone(),
        name: skill.manifest.name.clone(),
        version: skill.manifest.version.clone(),
        description: skill.manifest.description.clone(),
        source: skill.source.clone(),
        status: if skill.last_error.is_some() {
            SkillStatus::Blocked
        } else if skill.loaded.is_some() {
            SkillStatus::Loaded
        } else {
            SkillStatus::MetadataOnly
        },
        tags: skill.manifest.tags.clone(),
        required_tools: skill.manifest.required_tools.clone(),
        reference_count: skill.manifest.references.len(),
        metadata_hash: skill.metadata_hash.clone(),
        content_hash: skill
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.view.content_hash.clone()),
        last_error: skill.last_error.clone(),
    }
}

fn search_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

fn normalize_path(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}
