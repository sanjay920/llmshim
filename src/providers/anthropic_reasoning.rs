//! Resolve once per request. The catalog's effort/budget union determines the
//! native control; verified historical facts fill missing catalog metadata.
use crate::{
    catalog::{EffortLevel, ModelCapabilities, ReasoningOption, Support},
    error::{Result, ShimError},
};
#[derive(Debug, Clone)]
pub enum Profile {
    Unsupported,
    Effort(Vec<EffortLevel>),
    Budget { min: u64, max: Option<u64> },
}
impl Profile {
    pub fn for_model(model: &str) -> Self {
        let metadata = crate::catalog::resolve(&format!("anthropic/{model}"));
        let fallback = crate::catalog::builtin::anthropic_reasoning_options(model);
        let options = metadata
            .as_ref()
            .map(|m| m.reasoning_options.as_slice())
            .filter(|o| !o.is_empty())
            .unwrap_or(&fallback);
        Self::from_options(
            metadata
                .as_ref()
                .map(|m| m.capabilities)
                .unwrap_or_default(),
            options,
        )
    }
    pub fn from_options(caps: ModelCapabilities, options: &[ReasoningOption]) -> Self {
        if caps.reasoning == Support::Unsupported {
            return Self::Unsupported;
        }
        for option in options {
            if let ReasoningOption::Effort { values } = option {
                if !values.is_empty() {
                    return Self::Effort(values.clone());
                }
            }
        }
        for option in options {
            if let ReasoningOption::BudgetTokens { min, max } = option {
                return Self::Budget {
                    min: min.unwrap_or(1024) as u64,
                    max: max.map(u64::from),
                };
            }
        }
        Self::Unsupported
    }
    pub fn supported(&self) -> bool {
        !matches!(self, Self::Unsupported)
    }
    pub fn adaptive(&self) -> bool {
        matches!(self, Self::Effort(_))
    }
    pub fn effort(&self, requested: &str) -> &'static str {
        let levels = [
            (EffortLevel::None, "none"),
            (EffortLevel::Minimal, "minimal"),
            (EffortLevel::Low, "low"),
            (EffortLevel::Medium, "medium"),
            (EffortLevel::High, "high"),
            (EffortLevel::Xhigh, "xhigh"),
            (EffortLevel::Max, "max"),
        ];
        let Self::Effort(values) = self else {
            return "high";
        };
        let rank = levels
            .iter()
            .position(|(_, name)| *name == requested)
            .unwrap_or(4);
        levels
            .iter()
            .enumerate()
            .find(|(i, (level, _))| *i >= rank && values.contains(level))
            .or_else(|| {
                levels
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, (level, _))| values.contains(level))
            })
            .map(|(_, (_, name))| *name)
            .unwrap_or("high")
    }
    pub fn budget(&self, effort: &str, max_tokens: u64) -> Result<u64> {
        let Self::Budget { min, max } = self else {
            return Err(invalid());
        };
        let ceiling = max.unwrap_or(u64::MAX).min(max_tokens.saturating_sub(1));
        if *min > ceiling {
            return Err(invalid());
        }
        let budget = match effort {
            "low" => max_tokens / 4,
            "medium" => max_tokens / 2,
            "high" => ((max_tokens as u128 * 3) / 4) as u64,
            "xhigh" => ((max_tokens as u128 * 9) / 10) as u64,
            _ => max_tokens.saturating_sub(1),
        };
        Ok(budget.clamp(*min, ceiling))
    }
}
fn invalid() -> ShimError {
    ShimError::ProviderError {
        status: 400,
        body: "max_tokens cannot accommodate the configured reasoning budget".into(),
    }
}
