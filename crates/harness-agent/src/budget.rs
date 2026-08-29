use std::fs;
use std::path::Path;

use harness_types::{MissionId, RunId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::AgentError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgetPolicy {
    pub max_agents: usize,
    pub max_parallel_agents: usize,
    pub max_total_tokens: u64,
    pub max_tool_calls: u64,
    pub max_runtime_millis: u64,
    pub max_retries: u32,
}

impl AgentBudgetPolicy {
    #[must_use]
    pub const fn balanced() -> Self {
        Self {
            max_agents: 8,
            max_parallel_agents: 4,
            max_total_tokens: 100_000,
            max_tool_calls: 128,
            max_runtime_millis: 30 * 60 * 1_000,
            max_retries: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgetRequest {
    pub reserved_tokens: u64,
    pub reserved_tool_calls: u64,
    pub reserved_runtime_millis: u64,
    pub reserved_retries: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetEscrowStatus {
    Active,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgetEscrow {
    pub mission_id: MissionId,
    pub run_id: RunId,
    pub reserved_tokens: u64,
    pub consumed_tokens: u64,
    pub reserved_tool_calls: u64,
    pub consumed_tool_calls: u64,
    pub reserved_runtime_millis: u64,
    pub reserved_retries: u32,
    pub status: BudgetEscrowStatus,
    pub expires_at_millis: i64,
    pub version: u64,
}

/// 每项目 durable budget escrow；调度前 reserve，执行中 charge，终态 release。
pub struct AgentBudgetManager {
    connection: Connection,
}

impl AgentBudgetManager {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)
                .map_err(|error| AgentError::new("agent-budget-io", error.to_string()))?;
        }
        let connection = Connection::open(path).map_err(sql_error)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE IF NOT EXISTS agent_budget_escrows(
                   run_id TEXT PRIMARY KEY,
                   mission_id TEXT NOT NULL,
                   escrow_json TEXT NOT NULL,
                   status TEXT NOT NULL,
                   expires_at_millis INTEGER NOT NULL,
                   version INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_agent_budget_mission_status ON agent_budget_escrows(mission_id,status);",
            )
            .map_err(sql_error)?;
        Ok(Self { connection })
    }

    pub fn reserve(
        &mut self,
        mission_id: MissionId,
        run_id: RunId,
        request: &AgentBudgetRequest,
        policy: &AgentBudgetPolicy,
        now_millis: i64,
    ) -> Result<AgentBudgetEscrow, AgentError> {
        validate_request(request, policy)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        tx.execute(
            "UPDATE agent_budget_escrows SET status='expired',escrow_json=json_set(escrow_json,'$.status','expired','$.version',version+1),version=version+1 WHERE status='active' AND expires_at_millis<=?1",
            [now_millis],
        )
        .map_err(sql_error)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT escrow_json FROM agent_budget_escrows WHERE run_id=?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if let Some(existing) = existing {
            let existing = serde_json::from_str(&existing).map_err(json_error)?;
            tx.commit().map_err(sql_error)?;
            return Ok(existing);
        }
        let (active, tokens, tools, runtime): (usize, u64, u64, u64) = tx
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(json_extract(escrow_json,'$.reservedTokens')),0),COALESCE(SUM(json_extract(escrow_json,'$.reservedToolCalls')),0),COALESCE(SUM(json_extract(escrow_json,'$.reservedRuntimeMillis')),0) FROM agent_budget_escrows WHERE mission_id=?1 AND status='active'",
                [mission_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(sql_error)?;
        // max_parallel 由 BoundedAgentExecutor 的 worker 数硬限制；escrow 允许队列中的 Run 预留预算。
        if active >= policy.max_agents {
            return Err(AgentError::new(
                "agent-budget-agent-limit",
                format!("active={active}"),
            ));
        }
        if tokens.saturating_add(request.reserved_tokens) > policy.max_total_tokens {
            return Err(AgentError::new(
                "agent-budget-token-limit",
                mission_id.to_string(),
            ));
        }
        if tools.saturating_add(request.reserved_tool_calls) > policy.max_tool_calls {
            return Err(AgentError::new(
                "agent-budget-tool-limit",
                mission_id.to_string(),
            ));
        }
        if runtime.saturating_add(request.reserved_runtime_millis) > policy.max_runtime_millis {
            return Err(AgentError::new(
                "agent-budget-runtime-limit",
                mission_id.to_string(),
            ));
        }
        let expires_at_millis = now_millis.saturating_add(
            i64::try_from(request.reserved_runtime_millis.max(1)).unwrap_or(i64::MAX),
        );
        let escrow = AgentBudgetEscrow {
            mission_id,
            run_id,
            reserved_tokens: request.reserved_tokens,
            consumed_tokens: 0,
            reserved_tool_calls: request.reserved_tool_calls,
            consumed_tool_calls: 0,
            reserved_runtime_millis: request.reserved_runtime_millis,
            reserved_retries: request.reserved_retries,
            status: BudgetEscrowStatus::Active,
            expires_at_millis,
            version: 1,
        };
        tx.execute(
            "INSERT INTO agent_budget_escrows(run_id,mission_id,escrow_json,status,expires_at_millis,version) VALUES(?1,?2,?3,'active',?4,1)",
            params![
                escrow.run_id.to_string(),
                escrow.mission_id.to_string(),
                serde_json::to_string(&escrow).map_err(json_error)?,
                escrow.expires_at_millis
            ],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(escrow)
    }

    pub fn charge(
        &mut self,
        run_id: &RunId,
        token_delta: u64,
        tool_call_delta: u64,
        now_millis: i64,
    ) -> Result<AgentBudgetEscrow, AgentError> {
        let mut escrow = self
            .get(run_id)?
            .ok_or_else(|| AgentError::new("agent-budget-missing", run_id.to_string()))?;
        if escrow.status != BudgetEscrowStatus::Active {
            return Err(AgentError::new(
                "agent-budget-not-active",
                run_id.to_string(),
            ));
        }
        if now_millis >= escrow.expires_at_millis {
            self.release(run_id, BudgetEscrowStatus::Expired)?;
            return Err(AgentError::new("agent-budget-expired", run_id.to_string()));
        }
        let next_tokens = escrow.consumed_tokens.saturating_add(token_delta);
        let next_tools = escrow.consumed_tool_calls.saturating_add(tool_call_delta);
        if next_tokens > escrow.reserved_tokens {
            return Err(AgentError::new(
                "agent-budget-token-escrow",
                run_id.to_string(),
            ));
        }
        if next_tools > escrow.reserved_tool_calls {
            return Err(AgentError::new(
                "agent-budget-tool-escrow",
                run_id.to_string(),
            ));
        }
        let expected_version = escrow.version;
        escrow.consumed_tokens = next_tokens;
        escrow.consumed_tool_calls = next_tools;
        escrow.version = escrow.version.saturating_add(1);
        let changed = self
            .connection
            .execute(
                "UPDATE agent_budget_escrows SET escrow_json=?2,version=?3 WHERE run_id=?1 AND version=?4 AND status='active'",
                params![
                    run_id.to_string(),
                    serde_json::to_string(&escrow).map_err(json_error)?,
                    escrow.version,
                    expected_version
                ],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            return Err(AgentError::new(
                "agent-budget-cas-conflict",
                run_id.to_string(),
            ));
        }
        Ok(escrow)
    }

    pub fn release(
        &self,
        run_id: &RunId,
        status: BudgetEscrowStatus,
    ) -> Result<AgentBudgetEscrow, AgentError> {
        if status == BudgetEscrowStatus::Active {
            return Err(AgentError::new(
                "agent-budget-release-active",
                run_id.to_string(),
            ));
        }
        let mut escrow = self
            .get(run_id)?
            .ok_or_else(|| AgentError::new("agent-budget-missing", run_id.to_string()))?;
        if escrow.status != BudgetEscrowStatus::Active {
            return Ok(escrow);
        }
        let expected_version = escrow.version;
        escrow.status = status;
        escrow.version = escrow.version.saturating_add(1);
        let changed = self
            .connection
            .execute(
                "UPDATE agent_budget_escrows SET escrow_json=?2,status=?3,version=?4 WHERE run_id=?1 AND version=?5 AND status='active'",
                params![
                    run_id.to_string(),
                    serde_json::to_string(&escrow).map_err(json_error)?,
                    status_name(status),
                    escrow.version,
                    expected_version
                ],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            return Err(AgentError::new(
                "agent-budget-cas-conflict",
                run_id.to_string(),
            ));
        }
        Ok(escrow)
    }

    pub fn get(&self, run_id: &RunId) -> Result<Option<AgentBudgetEscrow>, AgentError> {
        self.connection
            .query_row(
                "SELECT escrow_json FROM agent_budget_escrows WHERE run_id=?1",
                [run_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
            .map(|json| serde_json::from_str(&json).map_err(json_error))
            .transpose()
    }
}

fn validate_request(
    request: &AgentBudgetRequest,
    policy: &AgentBudgetPolicy,
) -> Result<(), AgentError> {
    if request.reserved_tokens == 0
        || request.reserved_tokens > policy.max_total_tokens
        || request.reserved_tool_calls > policy.max_tool_calls
        || request.reserved_runtime_millis == 0
        || request.reserved_runtime_millis > policy.max_runtime_millis
        || request.reserved_retries > policy.max_retries
        || policy.max_agents == 0
        || policy.max_parallel_agents == 0
    {
        return Err(AgentError::new(
            "agent-budget-request-invalid",
            "policy/request",
        ));
    }
    Ok(())
}

fn status_name(status: BudgetEscrowStatus) -> &'static str {
    match status {
        BudgetEscrowStatus::Active => "active",
        BudgetEscrowStatus::Completed => "completed",
        BudgetEscrowStatus::Failed => "failed",
        BudgetEscrowStatus::Cancelled => "cancelled",
        BudgetEscrowStatus::Expired => "expired",
    }
}

fn sql_error(error: rusqlite::Error) -> AgentError {
    AgentError::new("agent-budget-sqlite", error.to_string())
}

fn json_error(error: serde_json::Error) -> AgentError {
    AgentError::new("agent-budget-json", error.to_string())
}
