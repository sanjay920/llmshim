#[cfg(feature = "proxy")]
pub(crate) use llmshim_catalog::bounded_json::{measure_value, parse_slice_with_usage, Usage};
pub(crate) use llmshim_catalog::bounded_json::{parse_slice, parse_str, Limits, ParseError};

pub(crate) fn enforce_sse_complexity(input: &str) -> crate::error::Result<()> {
    match parse_str(input, Limits::SSE) {
        Ok(_) | Err(ParseError::Malformed(_)) => Ok(()),
        Err(ParseError::Complexity) => Err(crate::error::ShimError::Stream(
            "upstream JSON exceeds complexity limit".into(),
        )),
    }
}

pub(crate) fn bounded_json_complete(input: &str, limits: Limits) -> crate::error::Result<bool> {
    match parse_str(input, limits) {
        Ok(_) => Ok(true),
        Err(ParseError::Malformed(_)) => Ok(false),
        Err(ParseError::Complexity) => Err(crate::error::ShimError::Stream(
            "upstream JSON exceeds complexity limit".into(),
        )),
    }
}
