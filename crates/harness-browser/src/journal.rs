//! Browser Action Journal：先记 Running，再执行副作用，崩溃后统一转为 Uncertain。

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use harness_types::{BrowserActionId, BrowserSessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{BrowserActionRecord, BrowserActionStatus, BrowserError, BrowserSessionStatus};

const SCHEMA_VERSION: u32 = 1;

pub trait BrowserActionJournal: Send + Sync {
    fn upsert_session(
        &self,
        session_id: &BrowserSessionId,
        status: BrowserSessionStatus,
        now_millis: i64,
    ) -> Result<(), BrowserError>;
    fn begin(&self, record: BrowserActionRecord) -> Result<BrowserActionRecord, BrowserError>;
    fn complete(
        &self,
        action_id: &BrowserActionId,
        summary: String,
        now_millis: i64,
    ) -> Result<BrowserActionRecord, BrowserError>;
    fn fail(
        &self,
        action_id: &BrowserActionId,
        error: String,
        now_millis: i64,
    ) -> Result<BrowserActionRecord, BrowserError>;
    fn list(&self, session_id: &BrowserSessionId)
    -> Result<Vec<BrowserActionRecord>, BrowserError>;
    fn recover_interrupted(&self, now_millis: i64) -> Result<usize, BrowserError>;
}

pub struct SqliteBrowserJournal {
    connection: Mutex<Connection>,
}

impl SqliteBrowserJournal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BrowserError> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|error| BrowserError::new("browser-journal-io", error.to_string()))?;
        }
        let connection = Connection::open(path).map_err(sql_error)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE IF NOT EXISTS browser_schema(version INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS browser_sessions(session_id TEXT PRIMARY KEY,status TEXT NOT NULL,updated_at_millis INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS browser_actions(action_id TEXT PRIMARY KEY,session_id TEXT NOT NULL,sequence INTEGER NOT NULL,record_json TEXT NOT NULL,status TEXT NOT NULL,started_at_millis INTEGER NOT NULL,completed_at_millis INTEGER,UNIQUE(session_id,sequence));
                 CREATE INDEX IF NOT EXISTS idx_browser_actions_session ON browser_actions(session_id,sequence);",
            )
            .map_err(sql_error)?;
        let version = connection
            .query_row("SELECT version FROM browser_schema LIMIT 1", [], |row| {
                row.get::<_, u32>(0)
            })
            .optional()
            .map_err(sql_error)?;
        match version {
            None => {
                connection
                    .execute(
                        "INSERT INTO browser_schema(version) VALUES(?1)",
                        [SCHEMA_VERSION],
                    )
                    .map_err(sql_error)?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(other) => {
                return Err(BrowserError::new(
                    "browser-schema-unsupported",
                    other.to_string(),
                ));
            }
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn update(
        &self,
        action_id: &BrowserActionId,
        status: BrowserActionStatus,
        summary: Option<String>,
        error: Option<String>,
        now_millis: i64,
    ) -> Result<BrowserActionRecord, BrowserError> {
        let mut connection = self.connection.lock().map_err(lock_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let json: String = transaction
            .query_row(
                "SELECT record_json FROM browser_actions WHERE action_id=?1",
                [action_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let mut record: BrowserActionRecord = serde_json::from_str(&json).map_err(json_error)?;
        if record.status != BrowserActionStatus::Running {
            return Err(BrowserError::new(
                "browser-action-not-running",
                action_id.to_string(),
            ));
        }
        record.status = status;
        record.result_summary = summary;
        record.error = error;
        record.completed_at_millis = Some(now_millis);
        transaction
            .execute(
                "UPDATE browser_actions SET record_json=?2,status=?3,completed_at_millis=?4 WHERE action_id=?1 AND status='running'",
                params![
                    action_id.to_string(),
                    serde_json::to_string(&record).map_err(json_error)?,
                    status_name(status),
                    now_millis
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(record)
    }
}

impl BrowserActionJournal for SqliteBrowserJournal {
    fn upsert_session(
        &self,
        session_id: &BrowserSessionId,
        status: BrowserSessionStatus,
        now_millis: i64,
    ) -> Result<(), BrowserError> {
        self.connection
            .lock()
            .map_err(lock_error)?
            .execute(
                "INSERT INTO browser_sessions(session_id,status,updated_at_millis) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET status=excluded.status,updated_at_millis=excluded.updated_at_millis",
                params![session_id.to_string(), session_status_name(status), now_millis],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    fn begin(&self, mut record: BrowserActionRecord) -> Result<BrowserActionRecord, BrowserError> {
        let mut connection = self.connection.lock().map_err(lock_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        record.sequence = transaction
            .query_row(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM browser_actions WHERE session_id=?1",
                [record.session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO browser_actions(action_id,session_id,sequence,record_json,status,started_at_millis,completed_at_millis) VALUES(?1,?2,?3,?4,'running',?5,NULL)",
                params![
                    record.id.to_string(),
                    record.session_id.to_string(),
                    record.sequence,
                    serde_json::to_string(&record).map_err(json_error)?,
                    record.started_at_millis
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(record)
    }

    fn complete(
        &self,
        action_id: &BrowserActionId,
        summary: String,
        now_millis: i64,
    ) -> Result<BrowserActionRecord, BrowserError> {
        self.update(
            action_id,
            BrowserActionStatus::Completed,
            Some(summary),
            None,
            now_millis,
        )
    }

    fn fail(
        &self,
        action_id: &BrowserActionId,
        error: String,
        now_millis: i64,
    ) -> Result<BrowserActionRecord, BrowserError> {
        self.update(
            action_id,
            BrowserActionStatus::Failed,
            None,
            Some(error),
            now_millis,
        )
    }

    fn list(
        &self,
        session_id: &BrowserSessionId,
    ) -> Result<Vec<BrowserActionRecord>, BrowserError> {
        let connection = self.connection.lock().map_err(lock_error)?;
        let mut statement = connection
            .prepare(
                "SELECT record_json FROM browser_actions WHERE session_id=?1 ORDER BY sequence",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([session_id.as_str()], |row| row.get::<_, String>(0))
            .map_err(sql_error)?;
        rows.map(|row| {
            row.map_err(sql_error)
                .and_then(|json| serde_json::from_str(&json).map_err(json_error))
        })
        .collect()
    }

    fn recover_interrupted(&self, now_millis: i64) -> Result<usize, BrowserError> {
        // 浏览器点击/输入/下载无法在崩溃后安全猜测是否完成，因此 fail closed。
        let mut connection = self.connection.lock().map_err(lock_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let rows = {
            let mut statement = transaction
                .prepare("SELECT action_id,record_json FROM browser_actions WHERE status='running'")
                .map_err(sql_error)?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        for (id, json) in &rows {
            let mut record: BrowserActionRecord = serde_json::from_str(json).map_err(json_error)?;
            record.status = BrowserActionStatus::Uncertain;
            record.error = Some("browser-process-interrupted".to_owned());
            record.completed_at_millis = Some(now_millis);
            transaction
                .execute(
                    "UPDATE browser_actions SET record_json=?2,status='uncertain',completed_at_millis=?3 WHERE action_id=?1 AND status='running'",
                    params![id, serde_json::to_string(&record).map_err(json_error)?, now_millis],
                )
                .map_err(sql_error)?;
        }
        transaction
            .execute(
                "UPDATE browser_sessions SET status='needs-reconciliation',updated_at_millis=?1 WHERE status IN ('starting','ready','user-control','closing')",
                [now_millis],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(rows.len())
    }
}

fn status_name(status: BrowserActionStatus) -> &'static str {
    match status {
        BrowserActionStatus::Running => "running",
        BrowserActionStatus::Completed => "completed",
        BrowserActionStatus::Failed => "failed",
        BrowserActionStatus::Uncertain => "uncertain",
    }
}

fn session_status_name(status: BrowserSessionStatus) -> &'static str {
    match status {
        BrowserSessionStatus::Closed => "closed",
        BrowserSessionStatus::Starting => "starting",
        BrowserSessionStatus::Ready => "ready",
        BrowserSessionStatus::UserControl => "user-control",
        BrowserSessionStatus::Closing => "closing",
        BrowserSessionStatus::Failed => "failed",
        BrowserSessionStatus::NeedsReconciliation => "needs-reconciliation",
    }
}

fn sql_error(error: rusqlite::Error) -> BrowserError {
    BrowserError::new("browser-journal-sqlite", error.to_string())
}

fn json_error(error: serde_json::Error) -> BrowserError {
    BrowserError::new("browser-journal-json", error.to_string())
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> BrowserError {
    BrowserError::new("browser-journal-poisoned", "connection")
}
