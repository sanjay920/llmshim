//! Reuse the bounded SSE parser, intercepting cloud errors before semantic normalization.
use super::error::native_error;
use crate::{
    json_bounds,
    transport::{ByteStream, EventStream},
};
use futures::StreamExt;

pub(super) fn events(bytes: ByteStream) -> EventStream {
    Box::pin(futures::stream::unfold(
        (crate::sse::data(bytes), false),
        |(mut events, failed)| async move {
            if failed {
                return None;
            }
            let event = events.next().await?;
            let result = event.and_then(|data| {
                if data == "[DONE]" {
                    return Ok(data);
                }
                let native =
                    json_bounds::parse_str(&data, json_bounds::Limits::SSE).map_err(|_| {
                        super::VertexError::service(502, "InvalidEvent").into_shim(None)
                    })?;
                match native_error(&native, None) {
                    Some(error) => Err(error.into_shim(None)),
                    None => Ok(super::response::prepare(native).to_string()),
                }
            });
            let failed = result.is_err();
            Some((result, (events, failed)))
        },
    ))
}
