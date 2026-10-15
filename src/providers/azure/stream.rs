//! Keep bounded SSE framing and native annotations, intercepting typed errors after HTTP 200.
use super::{error, AzureError};
use crate::transport::{ByteStream, EventStream};
use futures::StreamExt;
pub(super) fn events(model: &str, bytes: ByteStream) -> EventStream {
    Box::pin(futures::stream::unfold(
        (crate::sse::data(bytes), model.to_owned(), false),
        |(mut events, model, failed)| async move {
            if failed {
                return None;
            }
            let result = events.next().await?.and_then(|data| {
                if data == "[DONE]" {
                    return Ok(data);
                }
                let mut native =
                    crate::json_bounds::parse_str(&data, crate::json_bounds::Limits::SSE)
                        .map_err(|_| AzureError::service(502, "InvalidEvent").into_shim(None))?;
                if let Some(error) = error::native_error(&native, None) {
                    return Err(error.into_shim(None));
                }
                let object = native
                    .as_object_mut()
                    .ok_or_else(|| AzureError::service(502, "InvalidEvent").into_shim(None))?;
                object.insert("model".into(), model.clone().into());
                Ok(native.to_string())
            });
            let failed = result.is_err();
            Some((result, (events, model, failed)))
        },
    ))
}
