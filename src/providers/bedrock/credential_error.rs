//! Classify typed SDK failures without exposing SDK messages or paths.
use super::{BedrockError, CredentialReason};
use aws_config::profile::credentials::ProfileFileError;
use aws_credential_types::provider::error::CredentialsError;
use std::error::Error;

pub(super) fn classify(profile: &str, error: &(dyn Error + 'static)) -> BedrockError {
    let mut current = Some(error);
    let mut outcome = (CredentialReason::ProviderFailed, 503);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<CredentialsError>() {
            outcome = match error {
                CredentialsError::CredentialsNotLoaded(_) => {
                    (CredentialReason::ProfileNotFound, 401)
                }
                CredentialsError::InvalidConfiguration(_) => {
                    (CredentialReason::InvalidConfiguration, 401)
                }
                CredentialsError::ProviderTimedOut(_) => (CredentialReason::Timeout, 503),
                CredentialsError::ProviderError(_) => (CredentialReason::ProviderFailed, 503),
                _ => (CredentialReason::Unhandled, 401),
            };
        }
        if let Some(ProfileFileError::MissingProfile { .. }) =
            error.downcast_ref::<ProfileFileError>()
        {
            outcome = (CredentialReason::ProfileNotFound, 401);
        }
        current = error.source();
    }
    BedrockError::credentials(profile, outcome.0, outcome.1)
}
