//! Bounded binary framing projects Converse events onto shared Messages semantics.
use super::{events::Projection, BedrockError};
use crate::transport::{ByteStream, EventStream};
use bytes::{Buf, BytesMut};
use futures::StreamExt;

const MAX_FRAME: usize = 32 * 1024 * 1024;

#[non_exhaustive]
pub struct EventStreamDecoder {
    buffer: BytesMut,
    projection: Projection,
    failed: bool,
}

impl EventStreamDecoder {
    pub fn new(model: &str) -> Self {
        Self {
            buffer: BytesMut::new(),
            projection: Projection::new(model),
            failed: false,
        }
    }

    /// Attach the correlation identifier from the HTTP response headers.
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.projection.request_id = id.into();
        self
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, BedrockError> {
        let (events, error) = self.decode(bytes);
        match error {
            Some(error) => Err(error),
            None => Ok(events),
        }
    }

    fn decode(&mut self, bytes: &[u8]) -> (Vec<String>, Option<BedrockError>) {
        if self.failed {
            return (Vec::new(), Some(BedrockError::protocol()));
        }
        let mut events = Vec::new();
        let error = self.push_inner(bytes, &mut events).err();
        if error.is_some() {
            self.failed = true;
            self.buffer.clear();
            self.projection = Projection::new("");
        }
        (events, error)
    }

    fn push_inner(
        &mut self,
        mut bytes: &[u8],
        output: &mut Vec<String>,
    ) -> Result<(), BedrockError> {
        while !bytes.is_empty() {
            let target = if self.buffer.len() < 12 {
                12
            } else {
                let length = u32::from_be_bytes(
                    self.buffer[..4]
                        .try_into()
                        .map_err(|_| BedrockError::protocol())?,
                ) as usize;
                let crc = u32::from_be_bytes(
                    self.buffer[8..12]
                        .try_into()
                        .map_err(|_| BedrockError::protocol())?,
                );
                if !(16..=MAX_FRAME).contains(&length) || crc32fast::hash(&self.buffer[..8]) != crc
                {
                    return Err(BedrockError::protocol());
                }
                length
            };
            let take = bytes.len().min(target.saturating_sub(self.buffer.len()));
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes.advance(take);
            if self.buffer.len() < target || target == 12 {
                continue;
            }
            let frame = self.buffer.split().freeze();
            let message = aws_smithy_eventstream::frame::read_message_from(frame)
                .map_err(|_| BedrockError::protocol())?;
            let header = |name: &str| -> Result<&str, BedrockError> {
                let mut found = message
                    .headers()
                    .iter()
                    .filter(|header| header.name().as_str() == name);
                let value = found.next().ok_or_else(BedrockError::protocol)?;
                if found.next().is_some() {
                    return Err(BedrockError::protocol());
                }
                value
                    .value()
                    .as_string()
                    .map(|value| value.as_str())
                    .map_err(|_| BedrockError::protocol())
            };
            let kind = header(":message-type")?;
            let payload =
                std::str::from_utf8(message.payload()).map_err(|_| BedrockError::protocol())?;
            let payload = if kind == "error" && payload.is_empty() {
                serde_json::Value::Null
            } else {
                crate::json_bounds::parse_str(payload, crate::json_bounds::Limits::SSE)
                    .map_err(|_| BedrockError::protocol())?
            };
            match kind {
                "exception" | "error" => {
                    let code = header(":exception-type").or_else(|_| header(":error-code"))?;
                    let status = match code {
                        "throttlingException" => 429,
                        "validationException" => 400,
                        "modelStreamErrorException" => 424,
                        "serviceUnavailableException" => 503,
                        "modelTimeoutException" => 408,
                        "InvalidSignatureException"
                        | "SignatureDoesNotMatch"
                        | "ExpiredTokenException"
                        | "UnrecognizedClientException" => 403,
                        "internalServerException" => 500,
                        _ => 500,
                    };
                    let mut error = BedrockError::service(status, code);
                    error.native = Some(error.safe_native(payload));
                    return Err(error);
                }
                "event" => self
                    .projection
                    .event(header(":event-type")?, payload, output)?,
                _ => return Err(BedrockError::protocol()),
            }
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<(), BedrockError> {
        if self.failed || !self.buffer.is_empty() || !self.projection.completed {
            Err(BedrockError::protocol())
        } else {
            Ok(())
        }
    }
}

pub(super) fn events(model: &str, id: Option<&str>, bytes: ByteStream) -> EventStream {
    let mut decoder = EventStreamDecoder::new(model);
    if let Some(id) = id {
        decoder = decoder.with_request_id(id);
    }
    Box::pin(futures::stream::unfold(
        (bytes, decoder, std::collections::VecDeque::new(), false),
        |(mut bytes, mut decoder, mut pending, mut done)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((event, (bytes, decoder, pending, done)));
                }
                if done {
                    return None;
                }
                match bytes.next().await {
                    Some(Ok(chunk)) => {
                        let (events, error) = decoder.decode(&chunk);
                        pending.extend(events.into_iter().map(Ok));
                        if let Some(error) = error {
                            pending.push_back(Err(error.into_shim(None)));
                            done = true;
                        }
                    }
                    Some(Err(error)) => {
                        pending.push_back(Err(error));
                        done = true;
                    }
                    None => {
                        if let Err(error) = decoder.finish() {
                            pending.push_back(Err(error.into_shim(None)));
                        }
                        done = true;
                    }
                }
            }
        },
    ))
}
