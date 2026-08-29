use std::collections::BTreeSet;

use harness_types::ToolCallId;

use crate::{ModelError, ModelErrorKind, ModelEvent};

/// 所有 Provider Adapter 共享的 normalized event contract。
pub fn validate_event_contract(events: &[ModelEvent]) -> Result<(), ModelError> {
    if !matches!(events.first(), Some(ModelEvent::Started { .. })) {
        return Err(contract_error(
            "model-contract-started-first",
            "第一个事件必须是 Started",
        ));
    }
    if !matches!(events.last(), Some(ModelEvent::Completed { .. })) {
        return Err(contract_error(
            "model-contract-completed-last",
            "最后一个事件必须是 Completed",
        ));
    }
    if events
        .iter()
        .filter(|event| matches!(event, ModelEvent::Started { .. }))
        .count()
        != 1
    {
        return Err(contract_error(
            "model-contract-started-count",
            "Started 必须恰好出现一次",
        ));
    }
    if events
        .iter()
        .filter(|event| matches!(event, ModelEvent::Usage { .. }))
        .count()
        != 1
    {
        return Err(contract_error(
            "model-contract-usage-count",
            "Usage 必须恰好出现一次",
        ));
    }
    if events
        .iter()
        .filter(|event| matches!(event, ModelEvent::Completed { .. }))
        .count()
        != 1
    {
        return Err(contract_error(
            "model-contract-completed-count",
            "Completed 必须恰好出现一次",
        ));
    }
    let mut tool_calls = BTreeSet::<ToolCallId>::new();
    for event in events {
        match event {
            ModelEvent::ToolCall { call_id, .. } if !tool_calls.insert(call_id.clone()) => {
                return Err(contract_error(
                    "model-contract-tool-call-duplicate",
                    "Tool Call ID 必须唯一",
                ));
            }
            ModelEvent::TextDelta { delta } | ModelEvent::ReasoningSummaryDelta { delta }
                if delta.is_empty() =>
            {
                return Err(contract_error(
                    "model-contract-empty-delta",
                    "空 Delta 不应进入上层 EventBus",
                ));
            }
            ModelEvent::Usage { usage } => {
                usage.validate()?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn contract_error(code: &'static str, message: &'static str) -> ModelError {
    ModelError::new(ModelErrorKind::Protocol, code, message)
}

#[cfg(test)]
mod tests {
    use harness_types::{ModelId, ResponseId};

    use crate::{CompletionStatus, ModelUsage};

    use super::*;

    #[test]
    fn malformed_sequence_is_rejected() {
        let events = vec![
            ModelEvent::Started {
                response_id: ResponseId::from("response:1"),
                model_id: ModelId::from("model:1"),
            },
            ModelEvent::Completed {
                status: CompletionStatus::Completed,
                incomplete_reason: None,
            },
        ];
        assert_eq!(
            validate_event_contract(&events)
                .expect_err("usage missing")
                .code,
            "model-contract-usage-count"
        );
        let valid = vec![
            events[0].clone(),
            ModelEvent::Usage {
                usage: ModelUsage::default(),
            },
            events[1].clone(),
        ];
        validate_event_contract(&valid).expect("valid");
    }
}
