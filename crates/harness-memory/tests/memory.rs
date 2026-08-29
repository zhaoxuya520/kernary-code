use harness_memory::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

struct FakeProvider {
    profile: EmbeddingProfile,
    calls: Arc<AtomicUsize>,
    fail: bool,
}
impl EmbeddingProvider for FakeProvider {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(MemoryError::new("fake-embedding-failed", "fixture"));
        }
        let lower = text.to_lowercase();
        Ok(vec![
            if lower.contains("cache") { 1.0 } else { 0.0 },
            if lower.contains("approval") { 1.0 } else { 0.0 },
            1.0,
        ])
    }
}
struct FakeFactory {
    calls: Arc<AtomicUsize>,
    embeds: Arc<AtomicUsize>,
    fail: bool,
}
impl EmbeddingProviderFactory for FakeFactory {
    fn create(
        &self,
        profile: &EmbeddingProfile,
    ) -> Result<Arc<dyn EmbeddingProvider>, MemoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeProvider {
            profile: profile.clone(),
            calls: self.embeds.clone(),
            fail: self.fail,
        }))
    }
}

#[test]
fn no_embedding_model_creates_only_lexical_schema_and_semantic_falls_back() {
    let temporary = tempdir().expect("tempdir");
    let database = temporary.path().join("memory.sqlite");
    let mut memory = ProjectMemory::open(
        "project:test",
        &database,
        EmbeddingConfig {
            model: None,
            provider: None,
            dimensions: None,
        },
    )
    .expect("open");
    memory
        .add(
            NewMemoryRecord {
                id: "memory:approval".to_owned(),
                kind: MemoryKind::Decision,
                title: "权限审批".to_owned(),
                content: "所有外部副作用必须先审批。".to_owned(),
                tags: vec!["安全".to_owned()],
                source_ref: None,
                status: MemoryStatus::Verified,
            },
            1,
        )
        .expect("add");
    let lexical = memory
        .search("权限审批", RetrievalMode::Lexical, 8)
        .expect("search");
    assert_eq!(lexical.results[0].record.id, "memory:approval");
    let semantic = memory
        .search("如何保护外部操作", RetrievalMode::Semantic, 8)
        .expect("fallback");
    assert_eq!(semantic.executed_mode, ExecutedRetrievalMode::Lexical);
    assert!(semantic.degraded);
    let view = memory.view().expect("view");
    assert!(!view.vector_schema_present);
    assert!(matches!(view.semantic, SemanticCapability::Absent { .. }));
    assert!(!temporary.path().join("vector").exists());
}

#[test]
fn valid_embedding_config_is_ready_but_does_not_eagerly_create_vector_schema() {
    let temporary = tempdir().expect("tempdir");
    let memory = ProjectMemory::open(
        "project:test",
        temporary.path().join("memory.sqlite"),
        EmbeddingConfig {
            model: Some(" local-embed ".to_owned()),
            provider: Some("local".to_owned()),
            dimensions: Some(384),
        },
    )
    .expect("open");
    let view = memory.view().expect("view");
    assert!(matches!(
        view.semantic,
        SemanticCapability::Ready {
            dimensions: 384,
            ..
        }
    ));
    assert!(!view.vector_schema_present);
}

#[test]
fn invalid_nonempty_embedding_config_is_blocked_without_partial_initialization() {
    let temporary = tempdir().expect("tempdir");
    let memory = ProjectMemory::open(
        "project:test",
        temporary.path().join("memory.sqlite"),
        EmbeddingConfig {
            model: Some("embed".to_owned()),
            provider: None,
            dimensions: Some(3),
        },
    )
    .expect("open");
    let view = memory.view().expect("view");
    assert!(matches!(view.semantic, SemanticCapability::Blocked { .. }));
    assert!(!view.vector_schema_present);
}

#[test]
fn first_semantic_demand_builds_generation_and_switches_atomically() {
    let temporary = tempdir().expect("tempdir");
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let embedding_calls = Arc::new(AtomicUsize::new(0));
    let mut memory = ProjectMemory::open(
        "project:test",
        temporary.path().join("memory.sqlite"),
        EmbeddingConfig {
            model: Some("fake".to_owned()),
            provider: Some("local".to_owned()),
            dimensions: Some(3),
        },
    )
    .expect("open");
    for (id, title, content) in [
        ("m:cache", "Prompt Cache", "stable prefix cache"),
        ("m:approval", "Approval", "approval required"),
    ] {
        memory
            .add(
                NewMemoryRecord {
                    id: id.to_owned(),
                    kind: MemoryKind::Decision,
                    title: title.to_owned(),
                    content: content.to_owned(),
                    tags: vec![],
                    source_ref: None,
                    status: MemoryStatus::Verified,
                },
                1,
            )
            .expect("add");
    }
    memory
        .attach_embedding_factory(Arc::new(FakeFactory {
            calls: factory_calls.clone(),
            embeds: embedding_calls.clone(),
            fail: false,
        }))
        .expect("factory");
    let exact = memory
        .search("PromptCache", RetrievalMode::Auto, 8)
        .expect("exact");
    assert_eq!(exact.executed_mode, ExecutedRetrievalMode::Lexical);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
    let semantic = memory
        .search("how should approval work", RetrievalMode::Semantic, 8)
        .expect("semantic");
    assert_eq!(semantic.executed_mode, ExecutedRetrievalMode::Semantic);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding_calls.load(Ordering::SeqCst), 3);
    assert_eq!(semantic.results[0].record.id, "m:approval");
    let view = memory.view().expect("view");
    assert!(view.vector_schema_present);
    assert!(matches!(
        view.semantic,
        SemanticCapability::Active { generation: 1, .. }
    ));
    memory
        .add(
            NewMemoryRecord {
                id: "m:new".to_owned(),
                kind: MemoryKind::Lesson,
                title: "Cache lesson".to_owned(),
                content: "cache after activation".to_owned(),
                tags: vec![],
                source_ref: None,
                status: MemoryStatus::Observed,
            },
            2,
        )
        .expect("add active");
    assert_eq!(embedding_calls.load(Ordering::SeqCst), 4);
    memory
        .reconfigure(EmbeddingConfig {
            model: None,
            provider: None,
            dimensions: None,
        })
        .expect("clear");
    let fallback = memory
        .search("semantic cache strategy", RetrievalMode::Semantic, 8)
        .expect("fallback");
    assert!(fallback.degraded);
    assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
    assert!(memory.view().expect("view").vector_schema_present);
    memory.purge_vectors().expect("purge");
    assert!(!memory.view().expect("view").vector_schema_present);
}

#[test]
fn embedding_failure_degrades_to_lexical_without_losing_records() {
    let temporary = tempdir().expect("tempdir");
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let embeds = Arc::new(AtomicUsize::new(0));
    let mut memory = ProjectMemory::open(
        "project:test",
        temporary.path().join("memory.sqlite"),
        EmbeddingConfig {
            model: Some("fake".to_owned()),
            provider: Some("local".to_owned()),
            dimensions: Some(3),
        },
    )
    .expect("open");
    memory
        .add(
            NewMemoryRecord {
                id: "m:1".to_owned(),
                kind: MemoryKind::Failure,
                title: "Embedding failure".to_owned(),
                content: "fallback lexical".to_owned(),
                tags: vec![],
                source_ref: None,
                status: MemoryStatus::Verified,
            },
            1,
        )
        .expect("add");
    memory
        .attach_embedding_factory(Arc::new(FakeFactory {
            calls: factory_calls,
            embeds,
            fail: true,
        }))
        .expect("factory");
    let response = memory
        .search("embedding failure", RetrievalMode::Semantic, 8)
        .expect("fallback");
    assert!(response.degraded);
    assert_eq!(response.executed_mode, ExecutedRetrievalMode::Lexical);
    assert_eq!(response.results[0].record.id, "m:1");
    assert!(matches!(
        memory.view().expect("view").semantic,
        SemanticCapability::Degraded { .. }
    ));
}
