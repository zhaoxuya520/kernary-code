use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::MemoryError;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 200_000;
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".harness",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    "vendor",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryUpdateStats {
    pub discovered: usize,
    pub indexed: usize,
    pub unchanged_metadata: usize,
    pub unchanged_content: usize,
    pub deleted: usize,
    pub skipped: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositorySearchResult {
    pub path: String,
    pub language: String,
    pub summary: String,
    pub symbols: Vec<String>,
    pub diagnostics: Vec<String>,
    pub score: f64,
    pub matched_by: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryIndexView {
    pub root: PathBuf,
    pub database_path: PathBuf,
    pub file_count: usize,
    pub symbol_count: usize,
    pub import_count: usize,
    pub lsp_symbol_count: usize,
    pub lsp_diagnostic_count: usize,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspFactIngestReport {
    pub path: String,
    pub server_id: String,
    pub kind: String,
    pub before_count: usize,
    pub after_count: usize,
    pub added: usize,
    pub removed: usize,
    pub file_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspDiagnosticEvidence {
    pub run_id: String,
    pub path: String,
    pub server_id: String,
    pub before_count: usize,
    pub after_count: usize,
    pub added: usize,
    pub removed: usize,
    pub error_count: usize,
    pub warning_count: usize,
    pub file_hash: String,
    pub observed_at_millis: i64,
}

pub struct LspFactBatch<'a> {
    pub tool_name: &'a str,
    pub server_id: &'a str,
    pub path: &'a Path,
    pub facts: &'a [serde_json::Value],
    pub expected_file_hash: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub observed_at_millis: i64,
}

pub struct RepositoryIndex {
    root: PathBuf,
    database_path: PathBuf,
    connection: Connection,
}

impl RepositoryIndex {
    pub fn open(
        root: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let root = fs::canonicalize(root).map_err(io_error)?;
        if !root.is_dir() {
            return Err(MemoryError::new(
                "repository-root-not-directory",
                root.display().to_string(),
            ));
        }
        let database_path = database_path.as_ref().to_path_buf();
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let connection = Connection::open(&database_path).map_err(sql_error)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;
          CREATE TABLE IF NOT EXISTS repository_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          INSERT OR IGNORE INTO repository_meta VALUES('revision','0');
          CREATE TABLE IF NOT EXISTS repository_files(path TEXT PRIMARY KEY,content_hash TEXT NOT NULL,size INTEGER NOT NULL,mtime_millis INTEGER NOT NULL,language TEXT NOT NULL,summary TEXT NOT NULL,indexed_at_millis INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS repository_symbols(path TEXT NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL,line INTEGER NOT NULL,PRIMARY KEY(path,name,kind,line));
          CREATE INDEX IF NOT EXISTS idx_repository_symbol_name ON repository_symbols(name,path);
          CREATE TABLE IF NOT EXISTS repository_imports(path TEXT NOT NULL,target TEXT NOT NULL,line INTEGER NOT NULL,PRIMARY KEY(path,target,line));
          CREATE INDEX IF NOT EXISTS idx_repository_import_target ON repository_imports(target,path);
          CREATE TABLE IF NOT EXISTS repository_lsp_symbols(path TEXT NOT NULL,server_id TEXT NOT NULL,file_hash TEXT NOT NULL,fact_key TEXT NOT NULL,name TEXT NOT NULL,kind INTEGER NOT NULL,line INTEGER NOT NULL,character INTEGER NOT NULL,observed_at_millis INTEGER NOT NULL,PRIMARY KEY(path,server_id,fact_key));
          CREATE INDEX IF NOT EXISTS idx_repository_lsp_symbol_name ON repository_lsp_symbols(name,path);
          CREATE TABLE IF NOT EXISTS repository_lsp_diagnostics(path TEXT NOT NULL,server_id TEXT NOT NULL,file_hash TEXT NOT NULL,fact_key TEXT NOT NULL,severity INTEGER,code TEXT,source TEXT,message TEXT NOT NULL,line INTEGER NOT NULL,character INTEGER NOT NULL,observed_at_millis INTEGER NOT NULL,PRIMARY KEY(path,server_id,fact_key));
          CREATE INDEX IF NOT EXISTS idx_repository_lsp_diagnostic_code ON repository_lsp_diagnostics(code,path);
          CREATE TABLE IF NOT EXISTS repository_lsp_diagnostic_runs(run_id TEXT NOT NULL,path TEXT NOT NULL,server_id TEXT NOT NULL,before_count INTEGER NOT NULL,after_count INTEGER NOT NULL,added INTEGER NOT NULL,removed INTEGER NOT NULL,error_count INTEGER NOT NULL,warning_count INTEGER NOT NULL,file_hash TEXT NOT NULL,observed_at_millis INTEGER NOT NULL,PRIMARY KEY(run_id,path,server_id));
          CREATE VIRTUAL TABLE IF NOT EXISTS repository_fts USING fts5(path UNINDEXED,summary,content,tokenize='unicode61');").map_err(sql_error)?;
        Ok(Self {
            root,
            database_path,
            connection,
        })
    }

    pub fn update(&mut self, now_millis: i64) -> Result<RepositoryUpdateStats, MemoryError> {
        let mut paths = Vec::new();
        collect_files(&self.root, &self.root, &mut paths)?;
        paths.sort();
        if paths.len() > MAX_FILES {
            return Err(MemoryError::new(
                "repository-file-limit",
                paths.len().to_string(),
            ));
        }
        let mut stats = RepositoryUpdateStats::default();
        let mut seen = BTreeSet::new();
        for absolute in paths {
            stats.discovered += 1;
            let relative = absolute
                .strip_prefix(&self.root)
                .map_err(|_| {
                    MemoryError::new("repository-relative-path", absolute.display().to_string())
                })?
                .to_string_lossy()
                .replace('\\', "/");
            seen.insert(relative.clone());
            let metadata = fs::metadata(&absolute).map_err(io_error)?;
            let size = metadata.len();
            if size > MAX_FILE_BYTES {
                stats.skipped += 1;
                continue;
            }
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
                .unwrap_or(0);
            let existing = self
                .connection
                .query_row(
                    "SELECT content_hash,size,mtime_millis FROM repository_files WHERE path=?1",
                    [&relative],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(sql_error)?;
            if existing
                .as_ref()
                .is_some_and(|(_, old_size, old_mtime)| *old_size == size && *old_mtime == mtime)
            {
                stats.unchanged_metadata += 1;
                continue;
            }
            let bytes = fs::read(&absolute).map_err(io_error)?;
            let Ok(content) = String::from_utf8(bytes.clone()) else {
                stats.skipped += 1;
                continue;
            };
            let hash = format!("{:x}", Sha256::digest(&bytes));
            if existing
                .as_ref()
                .is_some_and(|(old_hash, _, _)| old_hash == &hash)
            {
                self.connection
                    .execute(
                        "UPDATE repository_files SET size=?2,mtime_millis=?3 WHERE path=?1",
                        params![relative, size, mtime],
                    )
                    .map_err(sql_error)?;
                stats.unchanged_content += 1;
                continue;
            }
            let language = language_for(&absolute);
            let summary = summarize(&content);
            let symbols = extract_symbols(&content, language);
            let imports = extract_imports(&content, language);
            let transaction = self.connection.transaction().map_err(sql_error)?;
            transaction
                .execute("DELETE FROM repository_fts WHERE path=?1", [&relative])
                .map_err(sql_error)?;
            transaction
                .execute("DELETE FROM repository_symbols WHERE path=?1", [&relative])
                .map_err(sql_error)?;
            transaction
                .execute("DELETE FROM repository_imports WHERE path=?1", [&relative])
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM repository_lsp_symbols WHERE path=?1 AND file_hash<>?2",
                    params![relative, hash],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM repository_lsp_diagnostics WHERE path=?1 AND file_hash<>?2",
                    params![relative, hash],
                )
                .map_err(sql_error)?;
            transaction.execute("INSERT INTO repository_files VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(path) DO UPDATE SET content_hash=excluded.content_hash,size=excluded.size,mtime_millis=excluded.mtime_millis,language=excluded.language,summary=excluded.summary,indexed_at_millis=excluded.indexed_at_millis",params![relative,hash,size,mtime,language,summary,now_millis]).map_err(sql_error)?;
            transaction
                .execute(
                    "INSERT INTO repository_fts VALUES(?1,?2,?3)",
                    params![relative, summary, content],
                )
                .map_err(sql_error)?;
            for symbol in symbols {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO repository_symbols VALUES(?1,?2,?3,?4)",
                        params![relative, symbol.name, symbol.kind, symbol.line],
                    )
                    .map_err(sql_error)?;
            }
            for import in imports {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO repository_imports VALUES(?1,?2,?3)",
                        params![relative, import.0, import.1],
                    )
                    .map_err(sql_error)?;
            }
            transaction.commit().map_err(sql_error)?;
            stats.indexed += 1;
        }
        let existing = self
            .connection
            .prepare("SELECT path FROM repository_files")
            .map_err(sql_error)?
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for path in existing {
            if !seen.contains(&path) {
                let transaction = self.connection.transaction().map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM repository_fts WHERE path=?1", [&path])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM repository_symbols WHERE path=?1", [&path])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM repository_imports WHERE path=?1", [&path])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM repository_files WHERE path=?1", [&path])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM repository_lsp_symbols WHERE path=?1", [&path])
                    .map_err(sql_error)?;
                transaction
                    .execute(
                        "DELETE FROM repository_lsp_diagnostics WHERE path=?1",
                        [&path],
                    )
                    .map_err(sql_error)?;
                transaction.commit().map_err(sql_error)?;
                stats.deleted += 1;
            }
        }
        if stats.indexed + stats.deleted > 0 {
            self.connection.execute("UPDATE repository_meta SET value=CAST(value AS INTEGER)+1 WHERE key='revision'",[]).map_err(sql_error)?;
        }
        Ok(stats)
    }

    /// 接收经过 ToolRuntime 限流后的 LSP facts；current facts 始终绑定当前文件 hash。
    pub fn ingest_lsp_facts(
        &mut self,
        batch: LspFactBatch<'_>,
    ) -> Result<Option<LspFactIngestReport>, MemoryError> {
        let LspFactBatch {
            tool_name,
            server_id,
            path: requested_path,
            facts,
            expected_file_hash,
            run_id,
            observed_at_millis: now_millis,
        } = batch;
        if !matches!(tool_name, "lsp.symbols" | "lsp.diagnostics") {
            return Ok(None);
        }
        if server_id.is_empty()
            || server_id.len() > 96
            || facts.len() > 1024
            || run_id.is_some_and(|run_id| run_id.is_empty() || run_id.len() > 256)
        {
            return Err(MemoryError::new("repository-lsp-ingest-invalid", server_id));
        }
        let (relative, file_hash) = self.current_file_identity(requested_path)?;
        if expected_file_hash.is_some_and(|expected| expected != file_hash) {
            return Err(MemoryError::new(
                "repository-lsp-file-version-mismatch",
                format!("path={relative}"),
            ));
        }
        let old_keys = self.lsp_fact_keys(tool_name, &relative, server_id, &file_hash)?;
        if tool_name == "lsp.symbols" {
            let rows = facts
                .iter()
                .filter_map(parse_lsp_symbol_row)
                .collect::<Vec<_>>();
            let new_keys = rows
                .iter()
                .map(|row| row.fact_key.clone())
                .collect::<BTreeSet<_>>();
            let transaction = self.connection.transaction().map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM repository_lsp_symbols WHERE path=?1 AND server_id=?2",
                    params![relative, server_id],
                )
                .map_err(sql_error)?;
            for row in &rows {
                transaction
                    .execute(
                        "INSERT INTO repository_lsp_symbols VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                        params![
                            relative,
                            server_id,
                            file_hash,
                            row.fact_key,
                            row.name,
                            row.kind,
                            row.line,
                            row.character,
                            now_millis
                        ],
                    )
                    .map_err(sql_error)?;
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(Some(ingest_report(
                relative, server_id, "symbols", &old_keys, &new_keys, file_hash,
            )));
        }

        let rows = facts
            .iter()
            .filter_map(parse_lsp_diagnostic_row)
            .collect::<Vec<_>>();
        let new_keys = rows
            .iter()
            .map(|row| row.fact_key.clone())
            .collect::<BTreeSet<_>>();
        let report = ingest_report(
            relative.clone(),
            server_id,
            "diagnostics",
            &old_keys,
            &new_keys,
            file_hash.clone(),
        );
        let error_count = rows.iter().filter(|row| row.severity == Some(1)).count();
        let warning_count = rows.iter().filter(|row| row.severity == Some(2)).count();
        let transaction = self.connection.transaction().map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM repository_lsp_diagnostics WHERE path=?1 AND server_id=?2",
                params![relative, server_id],
            )
            .map_err(sql_error)?;
        for row in &rows {
            transaction.execute("INSERT INTO repository_lsp_diagnostics VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![relative,server_id,file_hash,row.fact_key,row.severity,row.code,row.source,row.message,row.line,row.character,now_millis]).map_err(sql_error)?;
        }
        if let Some(run_id) = run_id {
            transaction.execute("INSERT INTO repository_lsp_diagnostic_runs VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(run_id,path,server_id) DO UPDATE SET before_count=excluded.before_count,after_count=excluded.after_count,added=excluded.added,removed=excluded.removed,error_count=excluded.error_count,warning_count=excluded.warning_count,file_hash=excluded.file_hash,observed_at_millis=excluded.observed_at_millis",params![run_id,relative,server_id,report.before_count,report.after_count,report.added,report.removed,error_count,warning_count,file_hash,now_millis]).map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)?;
        Ok(Some(report))
    }

    pub fn lsp_diagnostic_evidence(
        &self,
        run_id: &str,
    ) -> Result<Vec<LspDiagnosticEvidence>, MemoryError> {
        self.connection
            .prepare("SELECT run_id,path,server_id,before_count,after_count,added,removed,error_count,warning_count,file_hash,observed_at_millis FROM repository_lsp_diagnostic_runs WHERE run_id=?1 ORDER BY path,server_id")
            .map_err(sql_error)?
            .query_map([run_id], |row| {
                Ok(LspDiagnosticEvidence {
                    run_id: row.get(0)?,
                    path: row.get(1)?,
                    server_id: row.get(2)?,
                    before_count: row.get(3)?,
                    after_count: row.get(4)?,
                    added: row.get(5)?,
                    removed: row.get(6)?,
                    error_count: row.get(7)?,
                    warning_count: row.get(8)?,
                    file_hash: row.get(9)?,
                    observed_at_millis: row.get(10)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }

    fn current_file_identity(&self, requested: &Path) -> Result<(String, String), MemoryError> {
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        let absolute = fs::canonicalize(joined).map_err(io_error)?;
        if !absolute.is_file() || !is_inside(&self.root, &absolute) {
            return Err(MemoryError::new(
                "repository-lsp-path-outside-root",
                absolute.display().to_string(),
            ));
        }
        let bytes = fs::read(&absolute).map_err(io_error)?;
        if bytes.len() as u64 > MAX_FILE_BYTES || std::str::from_utf8(&bytes).is_err() {
            return Err(MemoryError::new(
                "repository-lsp-file-invalid",
                absolute.display().to_string(),
            ));
        }
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| {
                MemoryError::new("repository-relative-path", absolute.display().to_string())
            })?
            .to_string_lossy()
            .replace('\\', "/");
        Ok((relative, format!("{:x}", Sha256::digest(&bytes))))
    }

    fn lsp_fact_keys(
        &self,
        tool_name: &str,
        path: &str,
        server_id: &str,
        file_hash: &str,
    ) -> Result<BTreeSet<String>, MemoryError> {
        let table = if tool_name == "lsp.symbols" {
            "repository_lsp_symbols"
        } else {
            "repository_lsp_diagnostics"
        };
        let sql =
            format!("SELECT fact_key FROM {table} WHERE path=?1 AND server_id=?2 AND file_hash=?3");
        self.connection
            .prepare(&sql)
            .map_err(sql_error)?
            .query_map(params![path, server_id, file_hash], |row| row.get(0))
            .map_err(sql_error)?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(sql_error)
    }

    fn prune_stale_lsp_facts(&self) -> Result<(), MemoryError> {
        let identities = self
            .connection
            .prepare("SELECT path,file_hash FROM repository_lsp_symbols UNION SELECT path,file_hash FROM repository_lsp_diagnostics")
            .map_err(sql_error)?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for (path, stored_hash) in identities {
            let current_hash = self
                .current_file_identity(Path::new(&path))
                .ok()
                .map(|(_, hash)| hash);
            if current_hash.as_deref() == Some(stored_hash.as_str()) {
                continue;
            }
            self.connection
                .execute(
                    "DELETE FROM repository_lsp_symbols WHERE path=?1 AND file_hash=?2",
                    params![path, stored_hash],
                )
                .map_err(sql_error)?;
            self.connection
                .execute(
                    "DELETE FROM repository_lsp_diagnostics WHERE path=?1 AND file_hash=?2",
                    params![path, stored_hash],
                )
                .map_err(sql_error)?;
        }
        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RepositorySearchResult>, MemoryError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(vec![]);
        }
        self.prune_stale_lsp_facts()?;
        let limit = limit.clamp(1, 50);
        let mut scores = BTreeMap::<String, (f64, BTreeSet<String>)>::new();
        let pattern = format!("%{query}%");
        let mut stmt=self.connection.prepare("SELECT path FROM repository_files WHERE path LIKE ?1 ORDER BY length(path),path LIMIT ?2").map_err(sql_error)?;
        for (index, row) in stmt
            .query_map(params![pattern, limit * 2], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .enumerate()
        {
            add_score(
                &mut scores,
                row.map_err(sql_error)?,
                3.0 / (index + 1) as f64,
                "path",
            );
        }
        let mut stmt=self.connection.prepare("SELECT path FROM repository_symbols WHERE name LIKE ?1 ORDER BY name,path LIMIT ?2").map_err(sql_error)?;
        for (index, row) in stmt
            .query_map(params![pattern, limit * 2], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .enumerate()
        {
            let path = row.map_err(sql_error)?;
            add_score(&mut scores, path, 4.0 / (index + 1) as f64, "symbol");
        }
        let mut stmt = self.connection.prepare("SELECT path,name FROM repository_lsp_symbols WHERE name LIKE ?1 ORDER BY name,path LIMIT ?2").map_err(sql_error)?;
        for row in stmt
            .query_map(params![pattern, limit * 4], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_error)?
        {
            let (path, name) = row.map_err(sql_error)?;
            let exact = name.eq_ignore_ascii_case(query);
            add_score(
                &mut scores,
                path,
                if exact { 8.0 } else { 6.0 },
                "lsp-symbol",
            );
        }
        let mut stmt = self.connection.prepare("SELECT path,code,message FROM repository_lsp_diagnostics WHERE COALESCE(code,'') LIKE ?1 OR message LIKE ?1 ORDER BY path,code,message LIMIT ?2").map_err(sql_error)?;
        for row in stmt
            .query_map(params![pattern, limit * 4], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_error)?
        {
            let (path, code, _) = row.map_err(sql_error)?;
            let exact_code = code
                .as_deref()
                .is_some_and(|code| code.eq_ignore_ascii_case(query));
            add_score(
                &mut scores,
                path,
                if exact_code { 7.0 } else { 3.0 },
                "lsp-diagnostic",
            );
        }
        let escaped = query.replace('"', "\"\"");
        let mut stmt=self.connection.prepare("SELECT path,-bm25(repository_fts,0.0,2.0,1.0) FROM repository_fts WHERE repository_fts MATCH ?1 ORDER BY bm25(repository_fts,0.0,2.0,1.0) LIMIT ?2").map_err(sql_error)?;
        if let Ok(rows) = stmt.query_map(params![format!("\"{escaped}\""), limit * 2], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        }) {
            for row in rows {
                let (path, score) = row.map_err(sql_error)?;
                add_score(&mut scores, path, score.max(0.01), "fts");
            }
        }
        let mut ranked = scores.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        ranked
            .into_iter()
            .map(|(path, (score, matched_by))| {
                self.result(
                    path,
                    score,
                    matched_by.into_iter().collect::<Vec<_>>().join("+"),
                )
            })
            .collect()
    }

    fn result(
        &self,
        path: String,
        score: f64,
        matched_by: String,
    ) -> Result<RepositorySearchResult, MemoryError> {
        let indexed = self
            .connection
            .query_row(
                "SELECT language,summary FROM repository_files WHERE path=?1",
                [&path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        let (language, summary) = indexed.map_or_else(
            || {
                let absolute = self.root.join(&path);
                let content = fs::read_to_string(&absolute).map_err(io_error)?;
                Ok::<_, MemoryError>((language_for(&absolute).to_owned(), summarize(&content)))
            },
            Ok,
        )?;
        let mut symbols = self
            .connection
            .prepare("SELECT name FROM repository_symbols WHERE path=?1 ORDER BY line LIMIT 32")
            .map_err(sql_error)?
            .query_map([&path], |row| row.get(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        symbols.extend(
            self.connection
                .prepare("SELECT name FROM repository_lsp_symbols WHERE path=?1 ORDER BY line,character,name LIMIT 64")
                .map_err(sql_error)?
                .query_map([&path], |row| row.get(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<String>, _>>()
                .map_err(sql_error)?,
        );
        symbols.sort();
        symbols.dedup();
        let diagnostics = self
            .connection
            .prepare("SELECT severity,code,message,line,character FROM repository_lsp_diagnostics WHERE path=?1 ORDER BY severity,line,character LIMIT 16")
            .map_err(sql_error)?
            .query_map([&path], |row| {
                let severity: Option<u32> = row.get(0)?;
                let code: Option<String> = row.get(1)?;
                let message: String = row.get(2)?;
                let line: u32 = row.get(3)?;
                let character: u32 = row.get(4)?;
                Ok(format!(
                    "severity={} code={} {path}:{line}:{character} {}",
                    severity.unwrap_or(0),
                    code.as_deref().unwrap_or("none"),
                    message.chars().take(512).collect::<String>()
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(RepositorySearchResult {
            path,
            language,
            summary,
            symbols,
            diagnostics,
            score,
            matched_by,
        })
    }
    pub fn repository_map(&self) -> Result<String, MemoryError> {
        let mut stmt = self
            .connection
            .prepare("SELECT path FROM repository_files ORDER BY path")
            .map_err(sql_error)?;
        let paths = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(paths.join("\n"))
    }
    pub fn view(&self) -> Result<RepositoryIndexView, MemoryError> {
        let file_count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM repository_files", [], |r| r.get(0))
            .map_err(sql_error)?;
        let symbol_count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM repository_symbols", [], |r| r.get(0))
            .map_err(sql_error)?;
        let import_count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM repository_imports", [], |r| r.get(0))
            .map_err(sql_error)?;
        let lsp_symbol_count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM repository_lsp_symbols", [], |row| {
                row.get(0)
            })
            .map_err(sql_error)?;
        let lsp_diagnostic_count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM repository_lsp_diagnostics",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let revision: String = self
            .connection
            .query_row(
                "SELECT value FROM repository_meta WHERE key='revision'",
                [],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        Ok(RepositoryIndexView {
            root: self.root.clone(),
            database_path: self.database_path.clone(),
            file_count: file_count as usize,
            symbol_count: symbol_count as usize,
            import_count: import_count as usize,
            lsp_symbol_count: lsp_symbol_count as usize,
            lsp_diagnostic_count: lsp_diagnostic_count as usize,
            revision: revision.parse().unwrap_or(0),
        })
    }
    pub fn clear(&mut self) -> Result<(), MemoryError> {
        self.connection.execute_batch("DELETE FROM repository_fts;DELETE FROM repository_symbols;DELETE FROM repository_imports;DELETE FROM repository_lsp_symbols;DELETE FROM repository_lsp_diagnostics;DELETE FROM repository_lsp_diagnostic_runs;DELETE FROM repository_files;UPDATE repository_meta SET value=CAST(value AS INTEGER)+1 WHERE key='revision';").map_err(sql_error)
    }
}

#[derive(Clone)]
struct Symbol {
    name: String,
    kind: String,
    line: usize,
}

struct LspSymbolRow {
    fact_key: String,
    name: String,
    kind: i64,
    line: u32,
    character: u32,
}

struct LspDiagnosticRow {
    fact_key: String,
    severity: Option<u32>,
    code: Option<String>,
    source: Option<String>,
    message: String,
    line: u32,
    character: u32,
}

fn parse_lsp_symbol_row(value: &serde_json::Value) -> Option<LspSymbolRow> {
    let name = value.get("name")?.as_str()?.trim();
    if name.is_empty() || name.len() > 512 {
        return None;
    }
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let (line, character) = lsp_fact_position(value.get("location").unwrap_or(value))?;
    let fact_key = fact_key(&[
        name,
        &kind.to_string(),
        &line.to_string(),
        &character.to_string(),
    ]);
    Some(LspSymbolRow {
        fact_key,
        name: name.to_owned(),
        kind: i64::try_from(kind).unwrap_or(i64::MAX),
        line,
        character,
    })
}

fn parse_lsp_diagnostic_row(value: &serde_json::Value) -> Option<LspDiagnosticRow> {
    let message = value.get("message")?.as_str()?.trim();
    if message.is_empty() {
        return None;
    }
    let message = message.chars().take(4096).collect::<String>();
    let severity = value
        .get("severity")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let code = value.get("code").and_then(|code| match code {
        serde_json::Value::String(value) => Some(value.chars().take(256).collect()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    });
    let source = value
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(|value| value.chars().take(256).collect::<String>());
    let (line, character) = lsp_fact_position(value)?;
    let fact_key = fact_key(&[
        &severity.unwrap_or(0).to_string(),
        code.as_deref().unwrap_or(""),
        source.as_deref().unwrap_or(""),
        &message,
        &line.to_string(),
        &character.to_string(),
    ]);
    Some(LspDiagnosticRow {
        fact_key,
        severity,
        code,
        source,
        message,
        line,
        character,
    })
}

fn lsp_fact_position(value: &serde_json::Value) -> Option<(u32, u32)> {
    let human = value.get("humanRange").and_then(|range| range.get("start"));
    if let Some(human) = human {
        return Some((
            u32::try_from(human.get("line")?.as_u64()?).ok()?,
            u32::try_from(human.get("character")?.as_u64()?).ok()?,
        ));
    }
    let protocol = value.get("range")?.get("start")?;
    Some((
        u32::try_from(protocol.get("line")?.as_u64()?)
            .ok()?
            .saturating_add(1),
        u32::try_from(protocol.get("character")?.as_u64()?)
            .ok()?
            .saturating_add(1),
    ))
}

fn fact_key(parts: &[&str]) -> String {
    format!("{:x}", Sha256::digest(parts.join("\n").as_bytes()))
}

fn ingest_report(
    path: String,
    server_id: &str,
    kind: &str,
    old_keys: &BTreeSet<String>,
    new_keys: &BTreeSet<String>,
    file_hash: String,
) -> LspFactIngestReport {
    LspFactIngestReport {
        path,
        server_id: server_id.to_owned(),
        kind: kind.to_owned(),
        before_count: old_keys.len(),
        after_count: new_keys.len(),
        added: new_keys.difference(old_keys).count(),
        removed: old_keys.difference(new_keys).count(),
        file_hash,
    }
}

fn add_score(
    scores: &mut BTreeMap<String, (f64, BTreeSet<String>)>,
    path: String,
    score: f64,
    signal: &str,
) {
    let entry = scores.entry(path).or_default();
    entry.0 += score;
    entry.1.insert(signal.to_owned());
}

fn is_inside(root: &Path, target: &Path) -> bool {
    target == root || target.starts_with(root)
}

fn collect_files(
    root: &Path,
    current: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), MemoryError> {
    for entry in fs::read_dir(current).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let file_type = entry.file_type().map_err(io_error)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect_files(root, &entry.path(), output)?;
            }
        } else if file_type.is_file() && supported(&entry.path()) {
            output.push(entry.path());
        }
        if output.len() > MAX_FILES {
            return Err(MemoryError::new(
                "repository-file-limit",
                root.display().to_string(),
            ));
        }
    }
    Ok(())
}
fn supported(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "py"
            | "java"
            | "go"
            | "md"
            | "toml"
            | "json"
            | "yaml"
            | "yml"
    )
}
fn language_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        "py" => "python",
        "java" => "java",
        "go" => "go",
        "md" => "markdown",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        _ => "text",
    }
}
fn summarize(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(800)
        .collect()
}
fn extract_symbols(content: &str, language: &str) -> Vec<Symbol> {
    let mut output = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        let candidates = match language {
            "rust" => [
                "pub fn ",
                "fn ",
                "pub struct ",
                "struct ",
                "pub enum ",
                "enum ",
                "trait ",
            ]
            .as_slice(),
            "typescript" | "javascript" => [
                "export function ",
                "function ",
                "export class ",
                "class ",
                "export interface ",
                "interface ",
            ]
            .as_slice(),
            "python" => ["def ", "class "].as_slice(),
            "c" | "cpp" => ["struct ", "class "].as_slice(),
            _ => &[],
        };
        for prefix in candidates {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name = rest
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or_default();
                if !name.is_empty() {
                    output.push(Symbol {
                        name: name.to_owned(),
                        kind: prefix.trim().to_owned(),
                        line: index + 1,
                    });
                }
                break;
            }
        }
    }
    output
}
fn extract_imports(content: &str, language: &str) -> Vec<(String, usize)> {
    let mut output = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        let target = match language {
            "rust" => line.strip_prefix("use ").map(|v| v.trim_end_matches(';')),
            "typescript" | "javascript" => line.strip_prefix("import "),
            "python" => line
                .strip_prefix("import ")
                .or_else(|| line.strip_prefix("from ")),
            "c" | "cpp" => line.strip_prefix("#include"),
            _ => None,
        };
        if let Some(target) = target {
            output.push((target.chars().take(512).collect(), index + 1));
        }
    }
    output
}
fn sql_error(error: rusqlite::Error) -> MemoryError {
    MemoryError::new("repository-sqlite", error.to_string())
}
fn io_error(error: std::io::Error) -> MemoryError {
    MemoryError::new("repository-io", error.to_string())
}
