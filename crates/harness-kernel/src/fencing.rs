use std::error::Error;
use std::fmt::{Display, Formatter};

use harness_types::{ClaimToken, EffectId, MissionId, RunId};
use serde::{Deserialize, Serialize};

/// Mission owner 每次重新绑定时递增的 epoch。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MissionEpoch(pub u64);

/// 同一个逻辑 Run 每次恢复/替换时递增的 fence。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunFence(pub u64);

/// Outbox Runner 领取 Effect 后形成的执行权证明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectClaim {
    pub effect_id: EffectId,
    pub mission_id: MissionId,
    pub mission_epoch: MissionEpoch,
    pub claim_token: ClaimToken,
    pub run_id: Option<RunId>,
    pub run_fence: Option<RunFence>,
    pub attempt: u32,
    pub lease_expires_at_millis: i64,
}

/// Runtime 完成 Effect 时必须原样带回的 fencing 信息。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionFence {
    pub effect_id: EffectId,
    pub mission_epoch: MissionEpoch,
    pub claim_token: ClaimToken,
    pub run_fence: Option<RunFence>,
}

/// 过期 Runtime 的结果不能推进状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FencingError {
    EffectMismatch,
    MissionEpochMismatch,
    ClaimTokenMismatch,
    RunFenceMismatch,
}

impl Display for FencingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EffectMismatch => "effect-id-mismatch",
            Self::MissionEpochMismatch => "mission-epoch-mismatch",
            Self::ClaimTokenMismatch => "claim-token-mismatch",
            Self::RunFenceMismatch => "run-fence-mismatch",
        })
    }
}

impl Error for FencingError {}

/// 校验完成结果是否仍属于当前 claim/run。
pub fn validate_completion_fence(
    claim: &EffectClaim,
    completion: &CompletionFence,
) -> Result<(), FencingError> {
    if claim.effect_id != completion.effect_id {
        return Err(FencingError::EffectMismatch);
    }
    if claim.mission_epoch != completion.mission_epoch {
        return Err(FencingError::MissionEpochMismatch);
    }
    if claim.claim_token != completion.claim_token {
        return Err(FencingError::ClaimTokenMismatch);
    }
    if claim.run_fence != completion.run_fence {
        return Err(FencingError::RunFenceMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim() -> EffectClaim {
        EffectClaim {
            effect_id: EffectId::from("effect:1"),
            mission_id: MissionId::from("mission:1"),
            mission_epoch: MissionEpoch(2),
            claim_token: ClaimToken::from("claim:2"),
            run_id: Some(RunId::from("run:1")),
            run_fence: Some(RunFence(3)),
            attempt: 1,
            lease_expires_at_millis: 100,
        }
    }

    #[test]
    fn exact_fence_is_accepted() {
        let claim = claim();
        let completion = CompletionFence {
            effect_id: claim.effect_id.clone(),
            mission_epoch: claim.mission_epoch,
            claim_token: claim.claim_token.clone(),
            run_fence: claim.run_fence,
        };
        validate_completion_fence(&claim, &completion).expect("完全匹配的 fence 应接受");
    }

    #[test]
    fn stale_epoch_and_run_are_rejected() {
        let claim = claim();
        let stale_epoch = CompletionFence {
            effect_id: claim.effect_id.clone(),
            mission_epoch: MissionEpoch(1),
            claim_token: claim.claim_token.clone(),
            run_fence: claim.run_fence,
        };
        assert_eq!(
            validate_completion_fence(&claim, &stale_epoch),
            Err(FencingError::MissionEpochMismatch)
        );
        let stale_run = CompletionFence {
            effect_id: claim.effect_id.clone(),
            mission_epoch: claim.mission_epoch,
            claim_token: claim.claim_token.clone(),
            run_fence: Some(RunFence(2)),
        };
        assert_eq!(
            validate_completion_fence(&claim, &stale_run),
            Err(FencingError::RunFenceMismatch)
        );
    }
}
