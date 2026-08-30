use harness_memory::*;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn repository_index_is_incremental_searchable_and_removes_deleted_files() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("repo");
    fs::create_dir_all(root.join("src")).expect("dirs");
    fs::write(
        root.join("src/lib.rs"),
        "use crate::auth;\npub struct PermissionEngine;\npub fn approve() {}\n",
    )
    .expect("rust");
    fs::write(
        root.join("README.md"),
        "# Harness\nPermission approval architecture\n",
    )
    .expect("readme");
    fs::create_dir_all(root.join("target")).expect("target");
    fs::write(root.join("target/ignored.rs"), "pub fn ignored(){}").expect("ignored");
    let mut index =
        RepositoryIndex::open(&root, temporary.path().join("repository.sqlite")).expect("open");
    let first = index.update(1).expect("first");
    assert_eq!(first.indexed, 2);
    assert_eq!(index.view().expect("view").file_count, 2);
    assert!(
        index.search("PermissionEngine", 8).expect("search")[0]
            .symbols
            .contains(&"PermissionEngine".to_owned())
    );
    assert!(index.repository_map().expect("map").contains("src/lib.rs"));
    let seeds = index.semantic_seed(8).expect("semantic seeds");
    let library = seeds
        .iter()
        .find(|result| result.path == "src/lib.rs")
        .expect("library seed");
    assert_eq!(library.content_hash.len(), 64);
    assert!(library.imports.contains(&"crate::auth".to_owned()));
    assert_eq!(library.matched_by, "semantic-seed");
    let second = index.update(2).expect("second");
    assert_eq!(second.unchanged_metadata, 2);
    assert_eq!(second.indexed, 0);
    fs::write(
        root.join("src/lib.rs"),
        "pub struct CacheEngine;\npub fn cache_lookup() {}\n",
    )
    .expect("change");
    let third = index.update(3).expect("third");
    assert_eq!(third.indexed, 1);
    assert!(
        index.search("CacheEngine", 8).expect("search")[0]
            .path
            .ends_with("lib.rs")
    );
    fs::remove_file(root.join("README.md")).expect("remove");
    let fourth = index.update(4).expect("fourth");
    assert_eq!(fourth.deleted, 1);
    assert_eq!(index.view().expect("view").file_count, 1);
    index.clear().expect("clear");
    assert_eq!(index.view().expect("view").file_count, 0);
}

#[test]
fn lsp_facts_fuse_into_ranking_invalidate_by_hash_and_create_durable_delta_evidence() {
    let temporary = tempdir().expect("tempdir");
    let root = temporary.path().join("repo");
    fs::create_dir_all(root.join("src")).expect("dirs");
    let source = root.join("src/lib.rs");
    fs::write(&source, "pub fn local() {}\n").expect("source");
    let mut index =
        RepositoryIndex::open(&root, temporary.path().join("repository.sqlite")).expect("open");
    index.update(1).expect("index");
    let symbol = serde_json::json!({
        "name":"PreciseThing",
        "kind":12,
        "location":{
            "path":"src/lib.rs",
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},
            "humanRange":{"start":{"line":1,"character":1},"end":{"line":1,"character":5}},
            "positionEncoding":"utf-16",
            "external":false,
            "uri":"file:///fixture"
        }
    });
    let mismatch = index
        .ingest_lsp_facts(LspFactBatch {
            tool_name: "lsp.symbols",
            server_id: "fixture",
            path: Path::new("src/lib.rs"),
            facts: std::slice::from_ref(&symbol),
            expected_file_hash: Some(&"0".repeat(64)),
            run_id: None,
            observed_at_millis: 2,
        })
        .expect_err("response from another document version must be rejected");
    assert_eq!(mismatch.code, "repository-lsp-file-version-mismatch");
    index
        .ingest_lsp_facts(LspFactBatch {
            tool_name: "lsp.symbols",
            server_id: "fixture",
            path: Path::new("src/lib.rs"),
            facts: &[symbol],
            expected_file_hash: None,
            run_id: None,
            observed_at_millis: 2,
        })
        .expect("symbols");
    let diagnostic = serde_json::json!({
        "path":"src/lib.rs",
        "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}},
        "humanRange":{"start":{"line":1,"character":4},"end":{"line":1,"character":8}},
        "positionEncoding":"utf-16",
        "severity":1,
        "code":"E777",
        "source":"fixture",
        "message":"stale-only-code diagnostic"
    });
    let first = index
        .ingest_lsp_facts(LspFactBatch {
            tool_name: "lsp.diagnostics",
            server_id: "fixture",
            path: Path::new("src/lib.rs"),
            facts: std::slice::from_ref(&diagnostic),
            expected_file_hash: None,
            run_id: Some("run:reviewer"),
            observed_at_millis: 3,
        })
        .expect("diagnostics")
        .expect("report");
    assert_eq!(
        (first.before_count, first.after_count, first.added),
        (0, 1, 1)
    );
    let symbol_result = index.search("PreciseThing", 8).expect("symbol search");
    assert_eq!(symbol_result[0].path, "src/lib.rs");
    assert!(symbol_result[0].matched_by.contains("lsp-symbol"));
    assert!(
        symbol_result[0]
            .symbols
            .contains(&"PreciseThing".to_owned())
    );
    let diagnostic_result = index.search("E777", 8).expect("diagnostic search");
    assert!(diagnostic_result[0].matched_by.contains("lsp-diagnostic"));
    assert!(diagnostic_result[0].diagnostics[0].contains("E777"));
    let evidence = index
        .lsp_diagnostic_evidence("run:reviewer")
        .expect("evidence");
    assert_eq!(evidence.len(), 1);
    assert_eq!((evidence[0].added, evidence[0].removed), (1, 0));

    let unchanged = index
        .ingest_lsp_facts(LspFactBatch {
            tool_name: "lsp.diagnostics",
            server_id: "fixture",
            path: Path::new("src/lib.rs"),
            facts: std::slice::from_ref(&diagnostic),
            expected_file_hash: None,
            run_id: Some("run:tester"),
            observed_at_millis: 4,
        })
        .expect("same diagnostics")
        .expect("report");
    assert_eq!((unchanged.added, unchanged.removed), (0, 0));
    let removed = index
        .ingest_lsp_facts(LspFactBatch {
            tool_name: "lsp.diagnostics",
            server_id: "fixture",
            path: Path::new("src/lib.rs"),
            facts: &[],
            expected_file_hash: None,
            run_id: Some("run:tester-clean"),
            observed_at_millis: 5,
        })
        .expect("clean diagnostics")
        .expect("report");
    assert_eq!((removed.after_count, removed.removed), (0, 1));

    index
        .ingest_lsp_facts(LspFactBatch {
            tool_name: "lsp.diagnostics",
            server_id: "fixture",
            path: Path::new("src/lib.rs"),
            facts: &[diagnostic],
            expected_file_hash: None,
            run_id: None,
            observed_at_millis: 6,
        })
        .expect("diagnostics again");
    assert_eq!(index.view().expect("view").lsp_diagnostic_count, 1);
    fs::write(&source, "pub fn changed() {}\n").expect("change without reindex");
    assert!(
        index
            .search("stale-only-code", 8)
            .expect("stale search")
            .is_empty()
    );
    assert_eq!(index.view().expect("view").lsp_diagnostic_count, 0);
}
