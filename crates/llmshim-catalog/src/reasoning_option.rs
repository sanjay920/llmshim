//! Reasoning choices with an optional documented default, independent of request controls.
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Effort {
        values: Vec<EffortLevel>,
        /// The documented omitted-control default. Absent means unknown, not the level `none`.
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<EffortLevel>,
    },
    BudgetTokens {
        min: Option<u32>,
        max: Option<u32>,
    },
}

impl ReasoningOption {
    pub(crate) fn clear_invalid_default(&mut self) {
        if let Self::Effort { values, default } = self {
            *default = default.filter(|level| values.contains(level));
        }
    }
}

impl<'de> Deserialize<'de> for ReasoningOption {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum WireOption {
            Effort {
                values: Vec<EffortLevel>,
                #[serde(default)]
                default: serde_json::Value,
            },
            BudgetTokens {
                min: Option<u32>,
                max: Option<u32>,
            },
        }
        let mut option = match WireOption::deserialize(deserializer)? {
            WireOption::Effort { values, default } => {
                // A new or malformed default must not hide valid effort choices.
                let default = serde_json::from_value(default).ok();
                Self::Effort { values, default }
            }
            WireOption::BudgetTokens { min, max } => Self::BudgetTokens { min, max },
        };
        option.clear_invalid_default();
        Ok(option)
    }
}
