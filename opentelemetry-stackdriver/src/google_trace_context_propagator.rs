use once_cell::sync::Lazy;
use opentelemetry::propagation::text_map_propagator::FieldIter;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use opentelemetry::Context;
use regex::Regex;

/// Propagates span context in the Google Cloud Trace format,
/// using the __X-Cloud-Trace-Context__ header.
///
/// See https://cloud.google.com/trace/docs/setup/#force-trace for details on the format.
#[derive(Clone, Debug, Default)]
pub struct GoogleTraceContextPropagator {
    _private: (),
}

// https://cloud.google.com/trace/docs/setup/#force-trace
// documentation for the structure of X-Cloud-Trace-Context header is not very detailed

// this implementation is based on the official GCP golang library implementation:
// https://github.com/GoogleCloudPlatform/opentelemetry-operations-go/blob/main/propagator/propagator.go
// the regex they use: "^(?P<trace_id>[0-9a-f]{32})/(?P<span_id>[0-9]{1,20})(;o=(?P<trace_flags>[0-9]))?$"
// - trace id is 32 hex characters, mandatory
// - span id is 1-20 decimal characters, mandatory
// - trace flags is optional, 0 to 9 (0 - not sampled, missing or any other number - sampled)

const CLOUD_TRACE_CONTEXT_HEADER: &str = "X-Cloud-Trace-Context";
const GOOGLE_PROPAGATION_HEADER_VALUE_REGEX_STR: &str =
    r"^(?P<trace_id>[0-9a-f]{32})/(?P<span_id>[0-9]{1,20})(;o=(?P<trace_flags>[0-9]))?$";

static TRACE_CONTEXT_HEADER_FIELDS: Lazy<[String; 1]> =
    Lazy::new(|| [CLOUD_TRACE_CONTEXT_HEADER.to_owned()]);

static GOOGLE_PROPAGATION_HEADER_VALUE_REGEX: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(GOOGLE_PROPAGATION_HEADER_VALUE_REGEX_STR).ok());

impl GoogleTraceContextPropagator {
    /// Create a new `GoogleTraceContextPropagator`.
    pub fn new() -> Self {
        GoogleTraceContextPropagator { _private: () }
    }

    fn extract_span_context(&self, extractor: &dyn Extractor) -> Result<SpanContext, ()> {
        let header_value = extractor
            .get(CLOUD_TRACE_CONTEXT_HEADER)
            .map(|v| v.trim())
            .ok_or(())?;

        let regex = GOOGLE_PROPAGATION_HEADER_VALUE_REGEX.as_ref().ok_or(())?;

        // we could do a quick check if the header value matches the regex here to avoid more expensive regex capture in the next step if it doesn't match
        // but the assumption is that the header value will almost always be valid, so that would add an unnecessary check in majority of cases
        let caps = regex.captures(header_value).ok_or(())?;

        // trace id is mandatory, if it's missing, the header value is invalid
        let trace_id_hex = caps.name("trace_id").map(|m| m.as_str()).ok_or(())?;

        // span id is mandatory, if it's missing, the header value is invalid
        let span_id_dec = caps.name("span_id").map(|m| m.as_str()).ok_or(())?;

        // the request is sampled by default, it's not sampled only if explicitly set to 0
        let trace_flags = caps.name("trace_flags").map_or(TraceFlags::SAMPLED, |m| {
            if m.as_str() == "0" {
                TraceFlags::NOT_SAMPLED
            } else {
                TraceFlags::SAMPLED
            }
        });

        Self::construct_span_context(trace_flags, trace_id_hex, span_id_dec)
    }

    fn construct_span_context(
        trace_flags: TraceFlags,
        trace_id_hex: &str,
        span_id_dec: &str,
    ) -> Result<SpanContext, ()> {
        let trace_id = TraceId::from_hex(trace_id_hex).map_err(|_| ())?;

        let span_id = span_id_dec
            .parse::<u64>()
            .map(|v| SpanId::from_bytes(v.to_be_bytes())) // we can create SPAN ID only from bytes or hex string
            .map_err(|_| ())?;

        let span_context = SpanContext::new(trace_id, span_id, trace_flags, true, TraceState::NONE);

        // Ensure span is valid
        if !span_context.is_valid() {
            return Err(());
        }

        Ok(span_context)
    }
}

impl TextMapPropagator for GoogleTraceContextPropagator {
    fn inject_context(&self, cx: &Context, injector: &mut dyn Injector) {
        let span = cx.span();
        let span_context = span.span_context();
        let sampled_flag = span_context.trace_flags().to_u8();
        if span_context.is_valid() {
            let header_value = format!(
                "{:032x}/{};o={}",
                span_context.trace_id(),
                // at the moment we can only get span id as bytes
                u64::from_be_bytes(span_context.span_id().to_bytes()),
                sampled_flag
            );
            injector.set(CLOUD_TRACE_CONTEXT_HEADER, header_value);
        }
    }

    fn extract_with_context(&self, cx: &Context, extractor: &dyn Extractor) -> Context {
        self.extract_span_context(extractor)
            .map(|sc| cx.with_remote_span_context(sc))
            .unwrap_or_else(|_| cx.clone())
    }

    fn fields(&self) -> FieldIter<'_> {
        FieldIter::new(TRACE_CONTEXT_HEADER_FIELDS.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::testing::trace::TestSpan;
    use opentelemetry::trace::TraceState;
    use std::collections::HashMap;

    #[test]
    fn test_google_propagation_header_value_regex_str_valid() {
        // Try to create a Regex from the string
        let regex_result = Regex::new(GOOGLE_PROPAGATION_HEADER_VALUE_REGEX_STR);

        // Assert that the Regex was created successfully
        assert!(
            regex_result.is_ok(),
            "Failed to create Regex from GOOGLE_PROPAGATION_HEADER_VALUE_REGEX_STR"
        );

        // If the Regex was created successfully, validate it against a known valid string
        let regex = regex_result.unwrap();
        let valid_string = "105445aa7843bc8bf206b12000100000/1;o=1";
        assert!(
            regex.is_match(valid_string),
            "Regex does not match known valid string"
        );
    }

    #[test]
    fn test_extract_span_context_valid() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        headers.insert(
            // hashmap implementation of Extractor trait uses lowercase keys
            CLOUD_TRACE_CONTEXT_HEADER.to_string().to_lowercase(),
            "105445aa7843bc8bf206b12000100000/1;o=1".to_string(),
        );

        let span_context = propagator.extract_span_context(&headers).unwrap();
        assert_eq!(
            format!("{:x}", span_context.trace_id()),
            "105445aa7843bc8bf206b12000100000"
        );
        assert_eq!(u64::from_be_bytes(span_context.span_id().to_bytes()), 1);
        assert!(span_context.is_sampled());
    }

    #[test]
    fn test_extract_span_context_valid_without_options() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        headers.insert(
            // hashmap implementation of Extractor trait uses lowercase keys
            CLOUD_TRACE_CONTEXT_HEADER.to_string().to_lowercase(),
            "105445aa7843bc8bf206b12000100000/1".to_string(),
        );

        let span_context = propagator.extract_span_context(&headers).unwrap();
        assert_eq!(
            format!("{:x}", span_context.trace_id()),
            "105445aa7843bc8bf206b12000100000"
        );
        assert_eq!(u64::from_be_bytes(span_context.span_id().to_bytes()), 1);
        assert!(span_context.is_sampled());
    }

    #[test]
    fn test_extract_span_context_valid_not_sampled() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        headers.insert(
            // hashmap implementation of Extractor trait uses lowercase keys
            CLOUD_TRACE_CONTEXT_HEADER.to_string().to_lowercase(),
            "105445aa7843bc8bf206b12000100000/1;o=0".to_string(),
        );

        let span_context = propagator.extract_span_context(&headers).unwrap();
        assert_eq!(
            format!("{:x}", span_context.trace_id()),
            "105445aa7843bc8bf206b12000100000"
        );
        assert_eq!(u64::from_be_bytes(span_context.span_id().to_bytes()), 1);
        assert!(!span_context.is_sampled());
    }

    #[test]
    fn test_extract_span_context_invalid() {
        let propagator = GoogleTraceContextPropagator::new();
        let headers = HashMap::new();

        assert!(propagator.extract_span_context(&headers).is_err());
    }

    #[test]
    fn test_inject_context_valid() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        let span = TestSpan(SpanContext::new(
            TraceId::from_hex("105445aa7843bc8bf206b12000100000").unwrap(),
            SpanId::from_hex("0000000000000001").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        ));
        let cx = Context::current_with_span(span);

        propagator.inject_context(&cx, &mut headers);
        assert_eq!(
            // hashmap implementation of Extractor trait uses lowercase keys
            headers.get(CLOUD_TRACE_CONTEXT_HEADER.to_lowercase().as_str()),
            Some(&"105445aa7843bc8bf206b12000100000/1;o=1".to_string())
        );
    }

    #[test]
    fn test_extract_with_context_valid() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        headers.insert(
            CLOUD_TRACE_CONTEXT_HEADER.to_string().to_lowercase(),
            "105445aa7843bc8bf206b12000100000/1;o=1".to_string(),
        );
        let cx = Context::current();

        let new_cx = propagator.extract_with_context(&cx, &headers);
        assert!(new_cx.span().span_context().is_valid());
    }

    #[test]
    fn test_extract_with_context_invalid_trace_id() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        // Insert a trace ID with less than 32 characters
        headers.insert(
            CLOUD_TRACE_CONTEXT_HEADER.to_string().to_lowercase(),
            "105445aa7843bc8b/1;o=1".to_string(), // This trace ID is shorter than 32 characters
        );
        let cx = Context::current();

        let new_cx = propagator.extract_with_context(&cx, &headers);
        // Assert that the span context is not valid
        assert!(!new_cx.span().span_context().is_valid());
    }

    #[test]
    fn test_extract_with_context_invalid_span_id() {
        let propagator = GoogleTraceContextPropagator::new();
        let mut headers = HashMap::new();
        // Insert a trace ID with less than 32 characters
        headers.insert(
            CLOUD_TRACE_CONTEXT_HEADER.to_string().to_lowercase(),
            "105445aa7843bc8b/1abc;o=1".to_string(), // This span id is not decimal
        );
        let cx = Context::current();

        let new_cx = propagator.extract_with_context(&cx, &headers);
        // Assert that the span context is not valid
        assert!(!new_cx.span().span_context().is_valid());
    }
}
