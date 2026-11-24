use crate::{aliases, CatalogError};

const MAX_IMPORTED_MODELS: usize = 32_768;
const MAX_IDENTITY_BYTES: usize = 128 * 1024 * 1024;
const MODEL_BOOKKEEPING_BYTES: usize = 4_096;
const INDEX_BOOKKEEPING_BYTES: usize = 1_024;

pub(crate) struct ImportBudget {
    remaining_models: usize,
    remaining_identity_bytes: usize,
}

impl Default for ImportBudget {
    fn default() -> Self {
        Self {
            remaining_models: MAX_IMPORTED_MODELS,
            remaining_identity_bytes: MAX_IDENTITY_BYTES,
        }
    }
}

fn complexity_error() -> CatalogError {
    CatalogError::Invalid("catalog import exceeds derived identity limits")
}

impl ImportBudget {
    pub(crate) fn admit(&mut self, provider: &str, model_name: &str) -> Result<(), CatalogError> {
        let provider = aliases::provider_key(provider);
        let qualified_length = provider
            .len()
            .checked_add(model_name.len())
            .and_then(|length| length.checked_add(1))
            .ok_or_else(complexity_error)?;
        let spelling_count = 1
            + usize::from(model_name.starts_with("claude-")) * 2
            + aliases::REGION_PREFIXES
                .iter()
                .filter(|prefix| model_name.starts_with(**prefix))
                .count();
        let mut alias_count = 0_usize;
        let mut alias_bytes = 0_usize;
        for (alias, canonical) in aliases::PROVIDER_ALIASES {
            if provider == *canonical {
                alias_count = alias_count.checked_add(1).ok_or_else(complexity_error)?;
                alias_bytes = alias_bytes
                    .checked_add(alias.len())
                    .ok_or_else(complexity_error)?;
            }
        }

        // Cover incoming/stored identities, both spelling indexes, and spelling scratch.
        let qualified_copies = spelling_count
            .checked_mul(
                3_usize
                    .checked_add(alias_count)
                    .ok_or_else(complexity_error)?,
            )
            .and_then(|copies| copies.checked_add(5))
            .ok_or_else(complexity_error)?;
        let name_copies = spelling_count
            .checked_mul(
                4_usize
                    .checked_add(alias_count)
                    .ok_or_else(complexity_error)?,
            )
            .and_then(|copies| copies.checked_add(14))
            .ok_or_else(complexity_error)?;
        let index_bytes = alias_count
            .checked_add(2)
            .and_then(|count| count.checked_mul(INDEX_BOOKKEEPING_BYTES))
            .and_then(|bytes| bytes.checked_add(alias_bytes))
            .and_then(|bytes| bytes.checked_mul(spelling_count))
            .ok_or_else(complexity_error)?;
        let projected_bytes = qualified_length
            .checked_mul(qualified_copies)
            .and_then(|bytes| {
                model_name
                    .len()
                    .checked_mul(name_copies)
                    .and_then(|name_bytes| bytes.checked_add(name_bytes))
            })
            .and_then(|bytes| bytes.checked_add(index_bytes))
            .and_then(|bytes| bytes.checked_add(MODEL_BOOKKEEPING_BYTES))
            .ok_or_else(complexity_error)?;
        let remaining_models = self
            .remaining_models
            .checked_sub(1)
            .ok_or_else(complexity_error)?;
        let remaining_identity_bytes = self
            .remaining_identity_bytes
            .checked_sub(projected_bytes)
            .ok_or_else(complexity_error)?;
        self.remaining_models = remaining_models;
        self.remaining_identity_bytes = remaining_identity_bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_ceiling_remains_independent_of_available_identity_bytes() {
        let mut budget = ImportBudget {
            remaining_models: 2,
            remaining_identity_bytes: usize::MAX,
        };
        budget.admit("fixture", "one").unwrap();
        budget.admit("fixture", "two").unwrap();
        assert!(budget.admit("fixture", "three").is_err());
    }

    #[test]
    fn repeated_rows_and_lookup_variants_consume_the_shared_byte_budget() {
        let mut ordinary_budget = ImportBudget {
            remaining_models: 100,
            remaining_identity_bytes: 14_000,
        };
        ordinary_budget.admit("fixture", "m").unwrap();
        ordinary_budget.admit("fixture", "m").unwrap();
        assert!(ordinary_budget.admit("fixture", "m").is_err());

        let mut spelling_budget = ImportBudget {
            remaining_models: 100,
            remaining_identity_bytes: 14_000,
        };
        spelling_budget.admit("anthropic", "claude-1-1").unwrap();
        assert!(spelling_budget.admit("anthropic", "claude-1-1").is_err());
    }
}
