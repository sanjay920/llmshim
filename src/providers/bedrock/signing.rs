//! SigV4 signs the byte buffer reqwest will send, never a second JSON serialization.
use super::BedrockError;
use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{sign, SignableBody, SignableRequest, SigningSettings},
    sign::v4,
};
use std::time::SystemTime;

pub(super) fn sign_request(
    request: &mut reqwest::Request,
    credentials: Credentials,
    region: &str,
    service: &str,
    time: SystemTime,
    settings: SigningSettings,
) -> Result<(), BedrockError> {
    for header in ["authorization", "x-amz-date", "x-amz-security-token"] {
        request.headers_mut().remove(header);
    }
    let identity = credentials.into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(time)
        .settings(settings)
        .build()
        .map_err(|_| BedrockError::protocol())?
        .into();
    let body = match request.body() {
        Some(body) => body.as_bytes().ok_or_else(BedrockError::protocol)?,
        None => &[],
    };
    let mut headers = Vec::new();
    for (key, value) in request.headers() {
        headers.push((
            key.as_str(),
            value.to_str().map_err(|_| BedrockError::protocol())?,
        ));
    }
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.url().as_str(),
        headers.into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|_| BedrockError::protocol())?;
    let (instructions, _) = sign(signable, &params)
        .map_err(|_| BedrockError::protocol())?
        .into_parts();
    for (name, value) in instructions.headers() {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| BedrockError::protocol())?;
        let mut value =
            reqwest::header::HeaderValue::from_str(value).map_err(|_| BedrockError::protocol())?;
        if name == "authorization" || name == "x-amz-security-token" {
            value.set_sensitive(true);
        }
        request.headers_mut().insert(name, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
