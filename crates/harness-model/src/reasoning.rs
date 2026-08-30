use std::cmp::Ordering;

use crate::{ModelCapability, ReasoningLevel, ReasoningMapping};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasoningResolution {
    pub requested: ReasoningLevel,
    pub effective: Option<ReasoningLevel>,
    pub mapping: ReasoningMapping,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReasoningAdapter;

impl ReasoningAdapter {
    #[must_use]
    pub fn resolve(
        &self,
        requested: ReasoningLevel,
        capability: &ModelCapability,
    ) -> ReasoningResolution {
        if capability.reasoning_levels.is_empty() {
            return ReasoningResolution {
                requested,
                effective: None,
                mapping: ReasoningMapping::UnsupportedIgnored,
            };
        }
        if capability.reasoning_levels.contains(&requested) {
            return ReasoningResolution {
                requested,
                effective: Some(requested),
                mapping: ReasoningMapping::Exact,
            };
        }
        let effective = capability
            .reasoning_levels
            .iter()
            .copied()
            .min_by(|left, right| compare_distance(requested, *left, *right))
            .expect("非空集合已经检查");
        ReasoningResolution {
            requested,
            effective: Some(effective),
            mapping: if effective < requested {
                ReasoningMapping::ClampedDown
            } else {
                ReasoningMapping::ClampedUp
            },
        }
    }
}

fn compare_distance(
    requested: ReasoningLevel,
    left: ReasoningLevel,
    right: ReasoningLevel,
) -> Ordering {
    let requested = rank(requested);
    let left_distance = rank(left).abs_diff(requested);
    let right_distance = rank(right).abs_diff(requested);
    left_distance
        .cmp(&right_distance)
        // 距离相同优先向下 clamp，避免意外增加成本。
        .then_with(|| rank(left).cmp(&rank(right)))
}

const fn rank(level: ReasoningLevel) -> u8 {
    match level {
        ReasoningLevel::Off => 0,
        ReasoningLevel::Minimal => 1,
        ReasoningLevel::Low => 2,
        ReasoningLevel::Medium => 3,
        ReasoningLevel::High => 4,
        ReasoningLevel::Xhigh => 5,
        ReasoningLevel::Max => 6,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use harness_types::{ModelId, ProviderId};

    use super::*;

    fn capability(levels: &[ReasoningLevel]) -> ModelCapability {
        ModelCapability {
            provider_id: ProviderId::from("provider:test"),
            model_id: ModelId::from("model:test"),
            streaming: true,
            tool_calling: true,
            structured_output: true,
            image_input: false,
            prompt_cache_metrics: true,
            conversation_continuation: true,
            provider_compaction: false,
            context_window_tokens: 8_192,
            max_output_tokens: 1_024,
            reasoning_summary: true,
            reasoning_levels: levels.iter().copied().collect::<BTreeSet<_>>(),
        }
    }

    #[test]
    fn exact_clamp_and_unsupported_are_visible() {
        let adapter = ReasoningAdapter;
        let supported = capability(&[ReasoningLevel::Low, ReasoningLevel::High]);
        assert_eq!(
            adapter.resolve(ReasoningLevel::High, &supported).mapping,
            ReasoningMapping::Exact
        );
        let down = adapter.resolve(ReasoningLevel::Medium, &supported);
        assert_eq!(down.effective, Some(ReasoningLevel::Low));
        assert_eq!(down.mapping, ReasoningMapping::ClampedDown);
        assert_eq!(
            adapter
                .resolve(ReasoningLevel::High, &capability(&[]))
                .mapping,
            ReasoningMapping::UnsupportedIgnored
        );
    }
}
