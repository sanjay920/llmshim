//! Bounded SSE data fields; LLM transports do not retain SSE event IDs or retry metadata.

use crate::error::{Result, ShimError};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Clone, Copy)]
struct StreamLimits {
    total_bytes: usize,
    line_bytes: usize,
    frame_bytes: usize,
    frames: usize,
    chunks: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            total_bytes: 32 * 1024 * 1024,
            line_bytes: 8 * 1024 * 1024,
            frame_bytes: 8 * 1024 * 1024,
            frames: 100_000,
            chunks: 1_048_576,
        }
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = std::result::Result<Bytes, ()>> + Send>>;

pub(crate) fn data<Source, SourceError>(source: Source) -> DataStream
where
    Source: Stream<Item = std::result::Result<Bytes, SourceError>> + Send + 'static,
    SourceError: Send + 'static,
{
    DataStream::new(
        Box::pin(source.map(|chunk| chunk.map_err(|_| ()))),
        StreamLimits::default(),
    )
}

pub(crate) struct DataStream {
    source: ByteStream,
    limits: StreamLimits,
    current_chunk: Bytes,
    chunk_offset: usize,
    line: Vec<u8>,
    event_data: Vec<u8>,
    total_bytes: usize,
    frame_bytes: usize,
    frames: usize,
    chunks: usize,
    first_line: bool,
    after_carriage_return: bool,
    finished: bool,
}

impl DataStream {
    fn new(source: ByteStream, limits: StreamLimits) -> Self {
        Self {
            source,
            limits,
            current_chunk: Bytes::new(),
            chunk_offset: 0,
            line: Vec::new(),
            event_data: Vec::new(),
            total_bytes: 0,
            frame_bytes: 0,
            frames: 0,
            chunks: 0,
            first_line: true,
            after_carriage_return: false,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
        self.source = Box::pin(futures::stream::empty());
        self.current_chunk = Bytes::new();
        self.line = Vec::new();
        self.event_data = Vec::new();
    }

    fn fail(&mut self, message: &'static str) -> Poll<Option<Result<String>>> {
        self.finish();
        Poll::Ready(Some(Err(ShimError::Stream(message.into()))))
    }

    fn finish_line(&mut self) -> std::result::Result<Option<String>, &'static str> {
        let line = std::str::from_utf8(&self.line).map_err(|_| "invalid upstream SSE UTF-8")?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let event = if line.is_empty() {
            if self.frames >= self.limits.frames {
                return Err("upstream SSE frame count exceeds limit");
            }
            self.frames += 1;
            self.frame_bytes = 0;
            if self.event_data.is_empty() {
                None
            } else {
                self.event_data.pop();
                Some(
                    String::from_utf8(std::mem::take(&mut self.event_data))
                        .map_err(|_| "invalid upstream SSE UTF-8")?,
                )
            }
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                let value = value.strip_prefix(' ').unwrap_or(value);
                self.event_data.extend_from_slice(value.as_bytes());
                self.event_data.push(b'\n');
            }
            None
        };
        self.line.clear();
        Ok(event)
    }
}

impl Stream for DataStream {
    type Item = Result<String>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        for _ in 0..65_536 {
            if self.chunk_offset == self.current_chunk.len() {
                match self.source.as_mut().poll_next(context) {
                    Poll::Ready(Some(Ok(chunk))) => {
                        if self.chunks >= self.limits.chunks {
                            return self.fail("upstream SSE chunk count exceeds limit");
                        }
                        self.chunks += 1;
                        self.current_chunk = chunk;
                        self.chunk_offset = 0;
                        continue;
                    }
                    Poll::Ready(Some(Err(()))) => {
                        return self.fail("could not read upstream SSE");
                    }
                    Poll::Ready(None) => {
                        if std::str::from_utf8(&self.line).is_err() {
                            return self.fail("invalid upstream SSE UTF-8");
                        }
                        self.finish();
                        return Poll::Ready(None);
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            if self.total_bytes >= self.limits.total_bytes {
                return self.fail("upstream SSE decoded bytes exceed limit");
            }
            let byte = self.current_chunk[self.chunk_offset];
            self.chunk_offset += 1;
            self.total_bytes += 1;

            let follows_carriage_return = self.after_carriage_return;
            self.after_carriage_return = false;
            // Count both transport bytes, but only one logical CRLF line ending.
            if follows_carriage_return && byte == b'\n' {
                continue;
            }
            if self.frame_bytes >= self.limits.frame_bytes {
                return self.fail("upstream SSE frame exceeds size limit");
            }
            self.frame_bytes += 1;
            if byte == b'\n' || byte == b'\r' {
                self.after_carriage_return = byte == b'\r';
                match self.finish_line() {
                    Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                    Ok(None) => {}
                    Err(message) => return self.fail(message),
                }
            } else {
                if self.line.len() >= self.limits.line_bytes {
                    return self.fail("upstream SSE line exceeds size limit");
                }
                self.line.push(byte);
            }
        }
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventsource_stream::Eventsource;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn small_limits() -> StreamLimits {
        StreamLimits {
            total_bytes: 512,
            line_bytes: 128,
            frame_bytes: 256,
            frames: 16,
            chunks: 512,
        }
    }

    fn byte_source(bytes: &[u8], chunk_size: usize) -> ByteStream {
        let chunks: Vec<_> = bytes
            .chunks(chunk_size)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }

    async fn collect_values(mut stream: DataStream) -> Vec<String> {
        let mut values = Vec::new();
        while let Some(value) = stream.next().await {
            values.push(value.unwrap());
        }
        values
    }

    #[tokio::test]
    async fn standard_data_fields_match_the_previous_parser_across_fragmentation() {
        for ending in ["\n", "\r\n"] {
            let fixture = [
                ": comment",
                "id: identity",
                "event: custom",
                "retry: 100",
                "data: é雪🙂",
                "data: second",
                "",
                "data",
                "",
                "data:  leading space",
                "",
                "",
            ]
            .join(ending);
            for chunk_size in [1, 2, 7, fixture.len()] {
                let expected: Vec<_> = byte_source(fixture.as_bytes(), chunk_size)
                    .eventsource()
                    .map(|event| event.unwrap().data)
                    .collect()
                    .await;
                let actual = collect_values(DataStream::new(
                    byte_source(fixture.as_bytes(), chunk_size),
                    small_limits(),
                ))
                .await;
                assert_eq!(actual, expected);
                assert_eq!(actual, ["é雪🙂\nsecond", "", " leading space"]);
            }
        }
    }

    #[tokio::test]
    async fn bom_and_carriage_return_frames_complete_without_waiting_for_eof() {
        let source = byte_source("\u{feff}data: é雪🙂\rdata: second\r\r".as_bytes(), 1)
            .chain(futures::stream::pending());
        let mut stream = DataStream::new(Box::pin(source), small_limits());
        let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(event, "é雪🙂\nsecond");
    }

    #[tokio::test]
    async fn line_limit_applies_before_newline_to_data_and_unused_metadata() {
        for prefix in ["data:", "id:", "event:", "retry:", ":"] {
            let fixture = format!("{prefix} ordinary field text");
            let source = byte_source(fixture.as_bytes(), 1).chain(futures::stream::pending());
            let mut stream = DataStream::new(
                Box::pin(source),
                StreamLimits {
                    line_bytes: 8,
                    ..small_limits()
                },
            );
            let error = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains("line exceeds size limit"));
            assert!(stream.next().await.is_none());
            assert!(stream.line.is_empty() && stream.event_data.is_empty());
        }
    }

    #[tokio::test]
    async fn multiple_data_lines_share_one_frame_budget() {
        let mut stream = DataStream::new(
            byte_source(b"data:a\ndata:b\n\n", 1),
            StreamLimits {
                frame_bytes: 12,
                ..small_limits()
            },
        );
        let error = stream.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("frame exceeds size limit"));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn total_bytes_include_crlf_and_preserve_prior_completed_frames() {
        let limits = StreamLimits {
            total_bytes: 10,
            line_bytes: 6,
            frame_bytes: 8,
            ..small_limits()
        };
        let exact = DataStream::new(byte_source(b"data:a\r\n\r\n", 1), limits);
        assert_eq!(collect_values(exact).await, ["a"]);

        let mut overflowing = DataStream::new(byte_source(b"data:a\r\n\r\n:", 11), limits);
        assert_eq!(overflowing.next().await.unwrap().unwrap(), "a");
        let error = overflowing.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("decoded bytes exceed limit"));
        assert!(overflowing.next().await.is_none());
    }

    #[tokio::test]
    async fn empty_frames_still_consume_the_frame_count_budget() {
        let mut stream = DataStream::new(
            byte_source(b": comment\n\n: comment\n\n", 2),
            StreamLimits {
                frames: 1,
                ..small_limits()
            },
        );
        let error = stream.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("frame count exceeds limit"));
    }

    #[tokio::test]
    async fn empty_transport_chunks_cannot_bypass_the_work_budget() {
        let source = futures::stream::repeat_with(|| Ok(Bytes::new()));
        let mut stream = DataStream::new(
            Box::pin(source),
            StreamLimits {
                chunks: 3,
                ..small_limits()
            },
        );
        let error = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("chunk count exceeds limit"));
    }

    #[tokio::test]
    async fn eof_discards_unterminated_frames_and_rejects_incomplete_utf8() {
        for fixture in [b"data: unfinished".as_slice(), b"data: unfinished\n"] {
            assert!(
                collect_values(DataStream::new(byte_source(fixture, 1), small_limits()))
                    .await
                    .is_empty()
            );
        }
        let mut stream = DataStream::new(
            byte_source(&[b'd', b'a', b't', b'a', b':', 0xf0], 1),
            small_limits(),
        );
        assert!(stream
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("invalid upstream SSE UTF-8"));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn source_errors_do_not_expose_transport_details() {
        let mut stream = data(futures::stream::iter([Err::<Bytes, _>(
            "private transport data",
        )]));
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(error.contains("could not read upstream SSE"));
        assert!(!error.contains("private transport data"));
        assert!(stream.next().await.is_none());
    }

    struct SourceGuard(Arc<AtomicBool>);

    impl Drop for SourceGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn limit_failure_releases_the_source_without_another_poll() {
        let released = Arc::new(AtomicBool::new(false));
        let source = futures::stream::unfold(SourceGuard(released.clone()), |guard| async move {
            Some((Ok(Bytes::from_static(b"data: ordinary field text")), guard))
        });
        let mut stream = DataStream::new(
            Box::pin(source),
            StreamLimits {
                line_bytes: 8,
                ..small_limits()
            },
        );
        assert!(!released.load(Ordering::SeqCst));
        assert!(stream.next().await.unwrap().is_err());
        assert!(released.load(Ordering::SeqCst));
    }
}
