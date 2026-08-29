#![forbid(unsafe_code)]

//! 项目级结构化 Memory。Lexical 永远可用；Semantic 只由有效 Embedding Model 配置派生。

mod embedding_http;
mod repository;
mod vector_config;
pub use embedding_http::*;
pub use repository::*;
pub use vector_config::*;

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryError {
    pub code: String,
    pub message: String,
}

impl MemoryError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for MemoryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for MemoryError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryKind {
    Architecture,
    Decision,
    Contract,
    Lesson,
    Failure,
    Verification,
    Meeting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryStatus {
    Observed,
    Verified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRecord {
    pub id: String,
    pub project_id: String,
    pub kind: MemoryKind,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_ref: Option<String>,
    pub status: MemoryStatus,
    pub created_at_millis: i64,
    pub updated_at_millis: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMemoryRecord {
    pub id: String,
    pub kind: MemoryKind,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_ref: Option<String>,
    pub status: MemoryStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetrievalMode {
    Metadata,
    Lexical,
    Semantic,
    Hybrid,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutedRetrievalMode {
    Metadata,
    Lexical,
    Semantic,
    Hybrid,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemorySearchResult {
    pub record: MemoryRecord,
    pub score: f64,
    pub matched_by: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemorySearchResponse {
    pub requested_mode: RetrievalMode,
    pub executed_mode: ExecutedRetrievalMode,
    pub degraded: bool,
    pub degradation_reason: Option<String>,
    pub results: Vec<MemorySearchResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingConfig {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub dimensions: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddingProfile {
    pub model: String,
    pub provider: String,
    pub dimensions: usize,
}

pub trait EmbeddingProvider: Send + Sync {
    fn profile(&self) -> &EmbeddingProfile;
    fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError>;
}

pub trait EmbeddingProviderFactory: Send + Sync {
    fn create(&self, profile: &EmbeddingProfile)
    -> Result<Arc<dyn EmbeddingProvider>, MemoryError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum SemanticCapability {
    Absent {
        reason: String,
    },
    Blocked {
        reason: String,
    },
    Ready {
        model: String,
        provider: String,
        dimensions: usize,
    },
    Active {
        model: String,
        provider: String,
        dimensions: usize,
        generation: u64,
    },
    Degraded {
        reason: String,
    },
}

impl SemanticCapability {
    pub fn resolve(config: &EmbeddingConfig) -> Self {
        let model = config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(model) = model else {
            return Self::Absent {
                reason: "embedding-model-not-configured".to_owned(),
            };
        };
        let Some(provider) = config
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Self::Blocked {
                reason: "embedding-provider-missing".to_owned(),
            };
        };
        let Some(dimensions) = config
            .dimensions
            .filter(|value| *value > 0 && *value <= 65_536)
        else {
            return Self::Blocked {
                reason: "embedding-dimensions-invalid".to_owned(),
            };
        };
        Self::Ready {
            model: model.to_owned(),
            provider: provider.to_owned(),
            dimensions,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectMemoryView {
    pub project_id: String,
    pub database_path: PathBuf,
    pub record_count: usize,
    pub fts_indexed_count: usize,
    pub semantic: SemanticCapability,
    pub vector_schema_present: bool,
}

pub struct ProjectMemory {
    project_id: String,
    database_path: PathBuf,
    connection: Connection,
    semantic: SemanticCapability,
    embedding_factory: Option<Arc<dyn EmbeddingProviderFactory>>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    active_generation: Option<u64>,
}

impl ProjectMemory {
    pub fn open(
        project_id: impl Into<String>,
        database_path: impl AsRef<Path>,
        embedding: EmbeddingConfig,
    ) -> Result<Self, MemoryError> {
        let project_id = project_id.into();
        if project_id.trim().is_empty() {
            return Err(MemoryError::new("memory-project-id-empty", "project"));
        }
        let database_path = database_path.as_ref().to_path_buf();
        if let Some(parent) = database_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| MemoryError::new("memory-directory-create", error.to_string()))?;
        }
        let connection = Connection::open(&database_path).map_err(sql_error)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS memory_records(
               id TEXT PRIMARY KEY, project_id TEXT NOT NULL, kind TEXT NOT NULL,
               title TEXT NOT NULL, content TEXT NOT NULL, tags_json TEXT NOT NULL,
               source_ref TEXT, status TEXT NOT NULL, created_at_millis INTEGER NOT NULL,
               updated_at_millis INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_memory_project_kind_status
               ON memory_records(project_id,kind,status,updated_at_millis);
             CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
               record_id UNINDEXED, project_id UNINDEXED, title, content, tags,
               tokenize='unicode61'
             );",
            )
            .map_err(sql_error)?;
        Ok(Self {
            project_id,
            database_path,
            connection,
            semantic: SemanticCapability::resolve(&embedding),
            embedding_factory: None,
            embedding_provider: None,
            active_generation: None,
        })
    }

    pub fn attach_embedding_factory(
        &mut self,
        factory: Arc<dyn EmbeddingProviderFactory>,
    ) -> Result<(), MemoryError> {
        if !matches!(self.semantic, SemanticCapability::Ready { .. }) {
            return Err(MemoryError::new(
                "semantic-capability-not-ready",
                format!("{:?}", self.semantic),
            ));
        }
        self.embedding_factory = Some(factory);
        Ok(())
    }

    pub fn block_semantic(&mut self, reason: impl Into<String>) {
        self.embedding_factory = None;
        self.embedding_provider = None;
        self.active_generation = None;
        self.semantic = SemanticCapability::Blocked {
            reason: reason.into(),
        };
    }

    pub fn add(
        &mut self,
        input: NewMemoryRecord,
        now_millis: i64,
    ) -> Result<MemoryRecord, MemoryError> {
        let title = input.title.trim().to_owned();
        let content = input.content.trim().to_owned();
        if input.id.trim().is_empty() || title.is_empty() || content.is_empty() {
            return Err(MemoryError::new("memory-required-field-empty", input.id));
        }
        let mut tags = input
            .tags
            .into_iter()
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();
        let record = MemoryRecord {
            id: input.id,
            project_id: self.project_id.clone(),
            kind: input.kind,
            title,
            content,
            tags,
            source_ref: input.source_ref,
            status: input.status,
            created_at_millis: now_millis,
            updated_at_millis: now_millis,
        };
        let transaction = self.connection.transaction().map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO memory_records VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    record.id,
                    record.project_id,
                    enum_json(record.kind)?,
                    record.title,
                    record.content,
                    serde_json::to_string(&record.tags).map_err(json_error)?,
                    record.source_ref,
                    enum_json(record.status)?,
                    now_millis,
                    now_millis
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO memory_fts VALUES(?1,?2,?3,?4,?5)",
                params![
                    record.id,
                    record.project_id,
                    record.title,
                    record.content,
                    record.tags.join(" ")
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        if let (Some(provider), Some(generation)) =
            (self.embedding_provider.clone(), self.active_generation)
            && let Err(error) = self.index_record(&record, provider.as_ref(), generation)
        {
            self.semantic = SemanticCapability::Degraded { reason: error.code };
        }
        Ok(record)
    }

    pub fn forget(&mut self, id: &str) -> Result<bool, MemoryError> {
        let transaction = self.connection.transaction().map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM memory_fts WHERE record_id=?1 AND project_id=?2",
                params![id, self.project_id],
            )
            .map_err(sql_error)?;
        let deleted = transaction
            .execute(
                "DELETE FROM memory_records WHERE id=?1 AND project_id=?2",
                params![id, self.project_id],
            )
            .map_err(sql_error)?
            > 0;
        transaction.commit().map_err(sql_error)?;
        Ok(deleted)
    }

    pub fn search(
        &mut self,
        query: &str,
        mode: RetrievalMode,
        limit: usize,
    ) -> Result<MemorySearchResponse, MemoryError> {
        let query = query.trim();
        let limit = limit.clamp(1, 50);
        if query.is_empty() {
            return Ok(MemorySearchResponse {
                requested_mode: mode,
                executed_mode: ExecutedRetrievalMode::Metadata,
                degraded: false,
                degradation_reason: None,
                results: vec![],
            });
        }
        let wants_semantic = matches!(mode, RetrievalMode::Semantic | RetrievalMode::Hybrid)
            || (mode == RetrievalMode::Auto
                && !looks_exact(query)
                && matches!(
                    self.semantic,
                    SemanticCapability::Ready { .. } | SemanticCapability::Active { .. }
                ));
        if wants_semantic
            && matches!(self.semantic, SemanticCapability::Ready { .. })
            && let Err(error) = self.activate_semantic(now_millis())
        {
            self.semantic = SemanticCapability::Degraded {
                reason: error.code.clone(),
            };
        }
        if wants_semantic && matches!(self.semantic, SemanticCapability::Active { .. }) {
            let semantic = self.semantic_search(query, limit);
            return match semantic {
                Ok(results) if matches!(mode, RetrievalMode::Semantic) => {
                    Ok(MemorySearchResponse {
                        requested_mode: mode,
                        executed_mode: ExecutedRetrievalMode::Semantic,
                        degraded: false,
                        degradation_reason: None,
                        results,
                    })
                }
                Ok(semantic) => {
                    let lexical = self.lexical(query, limit.saturating_mul(2))?;
                    Ok(MemorySearchResponse {
                        requested_mode: mode,
                        executed_mode: ExecutedRetrievalMode::Hybrid,
                        degraded: false,
                        degradation_reason: None,
                        results: rrf(lexical, semantic, limit),
                    })
                }
                Err(error) => {
                    self.semantic = SemanticCapability::Degraded { reason: error.code };
                    Ok(MemorySearchResponse {
                        requested_mode: mode,
                        executed_mode: ExecutedRetrievalMode::Lexical,
                        degraded: true,
                        degradation_reason: Some("semantic-failed".to_owned()),
                        results: self.lexical(query, limit)?,
                    })
                }
            };
        }
        if wants_semantic && !matches!(self.semantic, SemanticCapability::Active { .. }) {
            return Ok(MemorySearchResponse {
                requested_mode: mode,
                executed_mode: ExecutedRetrievalMode::Lexical,
                degraded: true,
                degradation_reason: Some(
                    match self.semantic {
                        SemanticCapability::Absent { .. } => "semantic-not-configured",
                        SemanticCapability::Blocked { .. } => "semantic-invalid-configuration",
                        SemanticCapability::Ready { .. } => "semantic-not-active",
                        SemanticCapability::Degraded { .. } => "semantic-failed",
                        SemanticCapability::Active { .. } => unreachable!(),
                    }
                    .to_owned(),
                ),
                results: self.lexical(query, limit)?,
            });
        }
        let executed = if mode == RetrievalMode::Metadata {
            ExecutedRetrievalMode::Metadata
        } else {
            ExecutedRetrievalMode::Lexical
        };
        let results = if executed == ExecutedRetrievalMode::Metadata {
            self.metadata(query, limit)?
        } else {
            self.lexical(query, limit)?
        };
        Ok(MemorySearchResponse {
            requested_mode: mode,
            executed_mode: executed,
            degraded: false,
            degradation_reason: None,
            results,
        })
    }

    fn lexical(&self, query: &str, limit: usize) -> Result<Vec<MemorySearchResult>, MemoryError> {
        let escaped = query.replace('"', "\"\"");
        let mut statement = self.connection.prepare(
            "SELECT r.id,r.project_id,r.kind,r.title,r.content,r.tags_json,r.source_ref,r.status,r.created_at_millis,r.updated_at_millis,-bm25(memory_fts,0.0,5.0,1.0,2.0)
             FROM memory_fts JOIN memory_records r ON r.id=memory_fts.record_id
             WHERE memory_fts MATCH ?1 AND r.project_id=?2 ORDER BY bm25(memory_fts,0.0,5.0,1.0,2.0) LIMIT ?3"
        ).map_err(sql_error)?;
        let rows = statement
            .query_map(
                params![format!("\"{escaped}\""), self.project_id, limit],
                |row| {
                    let record = row_record(row)?;
                    let score = row.get::<_, f64>(10)?;
                    Ok((record, score))
                },
            )
            .map_err(sql_error)?;
        let results = rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?;
        if results.is_empty() {
            return self.metadata(query, limit);
        }
        Ok(results
            .into_iter()
            .map(|(record, score)| MemorySearchResult {
                record,
                score,
                matched_by: "fts".to_owned(),
            })
            .collect())
    }

    fn metadata(&self, query: &str, limit: usize) -> Result<Vec<MemorySearchResult>, MemoryError> {
        let pattern = format!("%{query}%");
        let mut statement = self.connection.prepare(
            "SELECT id,project_id,kind,title,content,tags_json,source_ref,status,created_at_millis,updated_at_millis
             FROM memory_records WHERE project_id=?1 AND (title LIKE ?2 OR content LIKE ?2 OR tags_json LIKE ?2)
             ORDER BY updated_at_millis DESC LIMIT ?3"
        ).map_err(sql_error)?;
        let rows = statement
            .query_map(params![self.project_id, pattern, limit], row_record)
            .map_err(sql_error)?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
            .into_iter()
            .enumerate()
            .map(|(index, record)| MemorySearchResult {
                record,
                score: 1.0 / (index + 1) as f64,
                matched_by: "metadata".to_owned(),
            })
            .collect())
    }

    fn activate_semantic(&mut self, now_millis: i64) -> Result<(), MemoryError> {
        let SemanticCapability::Ready {
            model,
            provider,
            dimensions,
        } = self.semantic.clone()
        else {
            return Ok(());
        };
        let profile = EmbeddingProfile {
            model,
            provider,
            dimensions,
        };
        let factory = self
            .embedding_factory
            .as_ref()
            .ok_or_else(|| MemoryError::new("embedding-factory-missing", &profile.model))?;
        let runtime = factory.create(&profile)?;
        if runtime.profile() != &profile {
            return Err(MemoryError::new(
                "embedding-profile-mismatch",
                format!("expected={profile:?}, actual={:?}", runtime.profile()),
            ));
        }
        self.connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS vector_generations(
                   generation INTEGER PRIMARY KEY, model TEXT NOT NULL, provider TEXT NOT NULL,
                   dimensions INTEGER NOT NULL, status TEXT NOT NULL, created_at_millis INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS memory_embeddings(
                   generation INTEGER NOT NULL, record_id TEXT NOT NULL, dimensions INTEGER NOT NULL,
                   vector_json TEXT NOT NULL, updated_at_millis INTEGER NOT NULL,
                   PRIMARY KEY(generation,record_id),
                   FOREIGN KEY(record_id) REFERENCES memory_records(id) ON DELETE CASCADE
                 );",
            )
            .map_err(sql_error)?;
        let generation: u64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(generation),0)+1 FROM vector_generations",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        self.connection
            .execute(
                "INSERT INTO vector_generations VALUES(?1,?2,?3,?4,'building',?5)",
                params![
                    generation,
                    profile.model,
                    profile.provider,
                    profile.dimensions,
                    now_millis
                ],
            )
            .map_err(sql_error)?;
        let records = self.all_records()?;
        for record in &records {
            if let Err(error) = self.index_record(record, runtime.as_ref(), generation) {
                let _ = self.connection.execute(
                    "UPDATE vector_generations SET status='failed' WHERE generation=?1",
                    [generation],
                );
                return Err(error);
            }
        }
        let transaction = self.connection.transaction().map_err(sql_error)?;
        transaction
            .execute(
                "UPDATE vector_generations SET status='dormant' WHERE status='active'",
                [],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "UPDATE vector_generations SET status='active' WHERE generation=?1 AND status='building'",
                [generation],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        self.embedding_provider = Some(runtime);
        self.active_generation = Some(generation);
        self.semantic = SemanticCapability::Active {
            model: profile.model,
            provider: profile.provider,
            dimensions: profile.dimensions,
            generation,
        };
        Ok(())
    }

    fn index_record(
        &self,
        record: &MemoryRecord,
        provider: &dyn EmbeddingProvider,
        generation: u64,
    ) -> Result<(), MemoryError> {
        let input = format!(
            "{:?}\n{}\n{}\n{}",
            record.kind,
            record.title,
            record.content,
            record.tags.join(" ")
        );
        let vector = provider.embed(&input)?;
        validate_vector(&vector, provider.profile().dimensions)?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO memory_embeddings VALUES(?1,?2,?3,?4,?5)",
                params![
                    generation,
                    record.id,
                    vector.len(),
                    serde_json::to_string(&vector).map_err(json_error)?,
                    now_millis()
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    fn semantic_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>, MemoryError> {
        let provider = self
            .embedding_provider
            .as_ref()
            .ok_or_else(|| MemoryError::new("embedding-provider-not-active", "provider"))?;
        let generation = self
            .active_generation
            .ok_or_else(|| MemoryError::new("vector-generation-not-active", "generation"))?;
        let query_vector = provider.embed(query)?;
        validate_vector(&query_vector, provider.profile().dimensions)?;
        let mut statement = self.connection.prepare(
            "SELECT r.id,r.project_id,r.kind,r.title,r.content,r.tags_json,r.source_ref,r.status,
                    r.created_at_millis,r.updated_at_millis,e.vector_json
             FROM memory_embeddings e JOIN memory_records r ON r.id=e.record_id
             WHERE e.generation=?1 AND r.project_id=?2"
        ).map_err(sql_error)?;
        let rows = statement
            .query_map(params![generation, self.project_id], |row| {
                let record = row_record(row)?;
                let json: String = row.get(10)?;
                Ok((record, json))
            })
            .map_err(sql_error)?;
        let mut results = Vec::new();
        for row in rows {
            let (record, json) = row.map_err(sql_error)?;
            let vector: Vec<f32> = serde_json::from_str(&json).map_err(json_error)?;
            results.push(MemorySearchResult {
                record,
                score: cosine(&query_vector, &vector),
                matched_by: "vector".to_owned(),
            });
        }
        results.sort_by(|left, right| right.score.total_cmp(&left.score));
        results.truncate(limit);
        Ok(results)
    }

    fn all_records(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut statement=self.connection.prepare(
            "SELECT id,project_id,kind,title,content,tags_json,source_ref,status,created_at_millis,updated_at_millis
             FROM memory_records WHERE project_id=?1 ORDER BY created_at_millis,id"
        ).map_err(sql_error)?;
        let rows = statement
            .query_map([&self.project_id], row_record)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn reconfigure(&mut self, config: EmbeddingConfig) -> Result<(), MemoryError> {
        let next = SemanticCapability::resolve(&config);
        if !matches!(next, SemanticCapability::Ready { .. }) {
            if self.vector_schema_present()? {
                self.connection
                    .execute(
                        "UPDATE vector_generations SET status='dormant' WHERE status='active'",
                        [],
                    )
                    .map_err(sql_error)?;
            }
            self.embedding_provider = None;
            self.embedding_factory = None;
            self.active_generation = None;
        }
        self.semantic = next;
        Ok(())
    }

    pub fn purge_vectors(&mut self) -> Result<(), MemoryError> {
        self.connection
            .execute_batch(
                "DROP TABLE IF EXISTS memory_embeddings; DROP TABLE IF EXISTS vector_generations;",
            )
            .map_err(sql_error)?;
        self.embedding_provider = None;
        self.active_generation = None;
        Ok(())
    }

    fn vector_schema_present(&self) -> Result<bool, MemoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_embeddings'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql_error)?
            .is_some())
    }

    pub fn view(&self) -> Result<ProjectMemoryView, MemoryError> {
        let record_count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM memory_records WHERE project_id=?1",
                [&self.project_id],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let fts_indexed_count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM memory_fts WHERE project_id=?1",
                [&self.project_id],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let vector_schema_present = self.vector_schema_present()?;
        Ok(ProjectMemoryView {
            project_id: self.project_id.clone(),
            database_path: self.database_path.clone(),
            record_count: record_count as usize,
            fts_indexed_count: fts_indexed_count as usize,
            semantic: self.semantic.clone(),
            vector_schema_present,
        })
    }
}

fn validate_vector(vector: &[f32], dimensions: usize) -> Result<(), MemoryError> {
    if vector.len() != dimensions || vector.iter().any(|value| !value.is_finite()) {
        return Err(MemoryError::new(
            "invalid-embedding-vector",
            format!("expected={dimensions}, actual={}", vector.len()),
        ));
    }
    Ok(())
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return -1.0;
    }
    let mut dot = 0.0_f64;
    let mut ln = 0.0_f64;
    let mut rn = 0.0_f64;
    for (left, right) in left.iter().zip(right) {
        let l = f64::from(*left);
        let r = f64::from(*right);
        dot += l * r;
        ln += l * l;
        rn += r * r;
    }
    if ln == 0.0 || rn == 0.0 {
        -1.0
    } else {
        dot / (ln.sqrt() * rn.sqrt())
    }
}

fn rrf(
    lexical: Vec<MemorySearchResult>,
    semantic: Vec<MemorySearchResult>,
    limit: usize,
) -> Vec<MemorySearchResult> {
    let mut merged = std::collections::BTreeMap::<String, (MemoryRecord, f64)>::new();
    for results in [lexical, semantic] {
        for (index, result) in results.into_iter().enumerate() {
            let entry = merged
                .entry(result.record.id.clone())
                .or_insert((result.record, 0.0));
            entry.1 += 1.0 / (61 + index) as f64;
        }
    }
    let mut output = merged
        .into_values()
        .map(|(record, score)| MemorySearchResult {
            record,
            score,
            matched_by: "hybrid".to_owned(),
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
    output.truncate(limit);
    output
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn looks_exact(query: &str) -> bool {
    query.split_whitespace().count() == 1
        && query
            .chars()
            .all(|c| c.is_alphanumeric() || "_$./:@-".contains(c))
}
fn enum_json<T: Serialize>(value: T) -> Result<String, MemoryError> {
    serde_json::to_value(value)
        .map_err(json_error)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| MemoryError::new("memory-enum-json", "not string"))
}
fn row_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    let kind: String = row.get(2)?;
    let status: String = row.get(7)?;
    let tags: String = row.get(5)?;
    Ok(MemoryRecord {
        id: row.get(0)?,
        project_id: row.get(1)?,
        kind: serde_json::from_value(serde_json::Value::String(kind)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?,
        title: row.get(3)?,
        content: row.get(4)?,
        tags: serde_json::from_str(&tags).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        source_ref: row.get(6)?,
        status: serde_json::from_value(serde_json::Value::String(status)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?,
        created_at_millis: row.get(8)?,
        updated_at_millis: row.get(9)?,
    })
}
fn sql_error(error: rusqlite::Error) -> MemoryError {
    MemoryError::new("memory-sqlite", error.to_string())
}
fn json_error(error: serde_json::Error) -> MemoryError {
    MemoryError::new("memory-json", error.to_string())
}
