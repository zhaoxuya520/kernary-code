use std::fs;
use std::path::Path;

use harness_types::{AgentEndpointId, AgentSessionId, MissionId, RunId, TaskId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{AgentEndpoint, AgentError, AgentResult, AgentSession};

const AGENT_STATE_SCHEMA_VERSION: u32 = 2;

/// Endpoint/AgentSession 的本地 SQLite Store；Mission 状态仍由 Kernel Event Store 独占。
pub struct AgentStateStore {
    connection: Connection,
}

impl AgentStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|error| AgentError::new("agent-state-io", error.to_string()))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| AgentError::new("agent-state-sqlite", error.to_string()))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE IF NOT EXISTS agent_state_schema(version INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS agent_endpoints(id TEXT PRIMARY KEY,state_json TEXT NOT NULL,version INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS agent_sessions(id TEXT PRIMARY KEY,state_json TEXT NOT NULL,version INTEGER NOT NULL,status TEXT NOT NULL,updated_at_millis INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS agent_results(run_id TEXT PRIMARY KEY,result_json TEXT NOT NULL,recorded_at_millis INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS agent_task_controls(mission_id TEXT NOT NULL,task_id TEXT NOT NULL,priority INTEGER NOT NULL,updated_at_millis INTEGER NOT NULL,PRIMARY KEY(mission_id,task_id));",
            )
            .map_err(sql_error)?;
        let version = connection
            .query_row(
                "SELECT version FROM agent_state_schema LIMIT 1",
                [],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(sql_error)?;
        match version {
            None => {
                connection
                    .execute(
                        "INSERT INTO agent_state_schema(version) VALUES(?1)",
                        [AGENT_STATE_SCHEMA_VERSION],
                    )
                    .map_err(sql_error)?;
            }
            Some(AGENT_STATE_SCHEMA_VERSION) => {}
            Some(1) => {
                // v2 只增加 Result 与 Task Control 投影，CREATE IF NOT EXISTS 已完成迁移。
                connection
                    .execute("UPDATE agent_state_schema SET version=2", [])
                    .map_err(sql_error)?;
            }
            Some(other) => {
                return Err(AgentError::new(
                    "agent-state-schema-unsupported",
                    format!("expected={AGENT_STATE_SCHEMA_VERSION}, actual={other}"),
                ));
            }
        }
        Ok(Self { connection })
    }

    pub fn create_endpoint(&self, endpoint: &AgentEndpoint) -> Result<(), AgentError> {
        if endpoint.version != 1 {
            return Err(AgentError::new(
                "agent-endpoint-initial-version",
                endpoint.version.to_string(),
            ));
        }
        self.connection
            .execute(
                "INSERT INTO agent_endpoints(id,state_json,version) VALUES(?1,?2,?3)",
                params![
                    endpoint.id.to_string(),
                    serde_json::to_string(endpoint).map_err(json_error)?,
                    endpoint.version
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn endpoint(&self, id: &AgentEndpointId) -> Result<Option<AgentEndpoint>, AgentError> {
        read_json(
            &self.connection,
            "SELECT state_json FROM agent_endpoints WHERE id=?1",
            id.as_str(),
        )
    }

    pub fn update_endpoint(
        &mut self,
        expected_version: u64,
        endpoint: &mut AgentEndpoint,
    ) -> Result<(), AgentError> {
        endpoint.version = expected_version.saturating_add(1);
        let changed = self
            .connection
            .execute(
                "UPDATE agent_endpoints SET state_json=?2,version=?3 WHERE id=?1 AND version=?4",
                params![
                    endpoint.id.to_string(),
                    serde_json::to_string(endpoint).map_err(json_error)?,
                    endpoint.version,
                    expected_version
                ],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            endpoint.version = expected_version;
            return Err(AgentError::new(
                "agent-endpoint-cas-conflict",
                endpoint.id.to_string(),
            ));
        }
        Ok(())
    }

    pub fn create_session(&self, session: &AgentSession) -> Result<(), AgentError> {
        if session.version != 1 {
            return Err(AgentError::new(
                "agent-session-initial-version",
                session.version.to_string(),
            ));
        }
        self.connection
            .execute(
                "INSERT INTO agent_sessions(id,state_json,version,status,updated_at_millis) VALUES(?1,?2,?3,?4,?5)",
                params![
                    session.id.to_string(),
                    serde_json::to_string(session).map_err(json_error)?,
                    session.version,
                    status_name(session.status),
                    session.updated_at_millis
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn session(&self, id: &AgentSessionId) -> Result<Option<AgentSession>, AgentError> {
        read_json(
            &self.connection,
            "SELECT state_json FROM agent_sessions WHERE id=?1",
            id.as_str(),
        )
    }

    pub fn update_session(
        &mut self,
        expected_version: u64,
        session: &mut AgentSession,
    ) -> Result<(), AgentError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        session.version = expected_version.saturating_add(1);
        let changed = transaction
            .execute(
                "UPDATE agent_sessions SET state_json=?2,version=?3,status=?4,updated_at_millis=?5 WHERE id=?1 AND version=?6",
                params![
                    session.id.to_string(),
                    serde_json::to_string(session).map_err(json_error)?,
                    session.version,
                    status_name(session.status),
                    session.updated_at_millis,
                    expected_version
                ],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            session.version = expected_version;
            return Err(AgentError::new(
                "agent-session-cas-conflict",
                session.id.to_string(),
            ));
        }
        transaction.commit().map_err(sql_error)?;
        Ok(())
    }

    pub fn recoverable_sessions(&self) -> Result<Vec<AgentSession>, AgentError> {
        let mut statement = self
            .connection
            .prepare("SELECT state_json FROM agent_sessions ORDER BY updated_at_millis,id")
            .map_err(sql_error)?;
        let sessions = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .map(|row| {
                row.map_err(sql_error)
                    .and_then(|json| serde_json::from_str(&json).map_err(json_error))
            })
            .collect::<Result<Vec<AgentSession>, AgentError>>()?;
        Ok(sessions
            .into_iter()
            .filter(|session| session.status.recoverable())
            .collect())
    }

    pub fn save_result(
        &self,
        run_id: &RunId,
        result: &AgentResult,
        recorded_at_millis: i64,
    ) -> Result<(), AgentError> {
        self.connection
            .execute(
                "INSERT INTO agent_results(run_id,result_json,recorded_at_millis) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO UPDATE SET result_json=excluded.result_json,recorded_at_millis=excluded.recorded_at_millis",
                params![
                    run_id.to_string(),
                    serde_json::to_string(result).map_err(json_error)?,
                    recorded_at_millis
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn result(&self, run_id: &RunId) -> Result<Option<AgentResult>, AgentError> {
        read_json(
            &self.connection,
            "SELECT result_json FROM agent_results WHERE run_id=?1",
            run_id.as_str(),
        )
    }

    pub fn set_task_priority(
        &self,
        mission_id: &MissionId,
        task_id: &TaskId,
        priority: i32,
        now_millis: i64,
    ) -> Result<(), AgentError> {
        self.connection
            .execute(
                "INSERT INTO agent_task_controls(mission_id,task_id,priority,updated_at_millis) VALUES(?1,?2,?3,?4) ON CONFLICT(mission_id,task_id) DO UPDATE SET priority=excluded.priority,updated_at_millis=excluded.updated_at_millis",
                params![mission_id.to_string(), task_id.to_string(), priority, now_millis],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn task_priority(
        &self,
        mission_id: &MissionId,
        task_id: &TaskId,
    ) -> Result<i32, AgentError> {
        self.connection
            .query_row(
                "SELECT priority FROM agent_task_controls WHERE mission_id=?1 AND task_id=?2",
                params![mission_id.to_string(), task_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0))
            .map_err(sql_error)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    id: &str,
) -> Result<Option<T>, AgentError> {
    connection
        .query_row(sql, [id], |row| row.get::<_, String>(0))
        .optional()
        .map_err(sql_error)?
        .map(|json| serde_json::from_str(&json).map_err(json_error))
        .transpose()
}

fn status_name(status: crate::AgentSessionStatus) -> &'static str {
    match status {
        crate::AgentSessionStatus::Prepared => "prepared",
        crate::AgentSessionStatus::Running => "running",
        crate::AgentSessionStatus::WaitingTool => "waiting-tool",
        crate::AgentSessionStatus::WaitingApproval => "waiting-approval",
        crate::AgentSessionStatus::Submitted => "submitted",
        crate::AgentSessionStatus::Completed => "completed",
        crate::AgentSessionStatus::CancelRequested => "cancel-requested",
        crate::AgentSessionStatus::Cancelled => "cancelled",
        crate::AgentSessionStatus::Failed => "failed",
    }
}

fn sql_error(error: rusqlite::Error) -> AgentError {
    AgentError::new("agent-state-sqlite", error.to_string())
}

fn json_error(error: serde_json::Error) -> AgentError {
    AgentError::new("agent-state-json", error.to_string())
}
