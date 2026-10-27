//! Shared typed errors for Kiro upstream calls.

/// The upstream still returned HTTP 429 after any applicable failover/retry.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream rate limited")]
pub struct UpstreamRateLimitError {
    retry_after: Option<String>,
}

/// Kiro rejected the serialized conversation because it exceeded the model's
/// context limit. Keeping this typed at the provider boundary lets compact
/// retry once without reverse-engineering an HTTP error response.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream context window exceeded")]
pub struct UpstreamContextOverflowError;

/// Kiro's overload marker, e.g.
/// `{"message":"Encountered unexpectedly high load when processing the request, please try again.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}`.
/// It arrives as an HTTP 429/5xx body or as the payload of an in-stream exception frame.
pub const MODEL_TEMPORARILY_UNAVAILABLE: &str = "MODEL_TEMPORARILY_UNAVAILABLE";

/// Whether an upstream HTTP error body carries exactly the overload `reason`.
pub(crate) fn is_model_overloaded_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("reason")
                .and_then(|reason| reason.as_str())
                .map(|reason| reason == MODEL_TEMPORARILY_UNAVAILABLE)
        })
        .unwrap_or(false)
}

/// Model capacity is temporarily unavailable, independently of account health.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream model temporarily unavailable")]
pub struct UpstreamModelUnavailableError {
    pub retry_after: String,
}

#[derive(Debug, thiserror::Error)]
#[error("upstream response deadline exceeded")]
pub struct UpstreamTimeoutError {
    #[source]
    pub source: reqwest::Error,
}

impl UpstreamRateLimitError {
    pub(crate) fn new(retry_after: Option<String>) -> Self {
        Self {
            retry_after: retry_after.and_then(normalize_retry_after),
        }
    }

    pub(crate) fn from_headers(headers: &http::HeaderMap) -> Self {
        let retry_after = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self::new(retry_after)
    }

    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }

    /// Without an explicit upstream delay, a short local retry is still useful.
    pub(crate) fn should_retry_locally(&self) -> bool {
        self.retry_after.is_none()
    }
}

fn normalize_retry_after(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value.parse::<u64>().is_ok() || httpdate::parse_http_date(value).is_ok() {
        Some(value.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_delta_seconds_and_http_date() {
        let seconds = UpstreamRateLimitError::new(Some(" 1800 ".to_string()));
        assert_eq!(seconds.retry_after(), Some("1800"));
        assert!(!seconds.should_retry_locally());

        let date = "Sun, 12 Jul 2026 02:30:00 GMT";
        let http_date = UpstreamRateLimitError::new(Some(date.to_string()));
        assert_eq!(http_date.retry_after(), Some(date));
        assert!(!http_date.should_retry_locally());
    }

    #[test]
    fn rejects_invalid_retry_after() {
        let error = UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        assert_eq!(error.retry_after(), None);
        assert!(error.should_retry_locally());
    }
}
