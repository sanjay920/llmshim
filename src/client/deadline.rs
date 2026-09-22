use std::time::Duration;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_POLICY_CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_ERROR_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_ERROR_BODY_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_UNARY_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_UNARY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_STREAM_SEMANTIC_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_STREAM_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Clone, Copy, Debug)]
pub struct AttemptDeadlines {
    pub(crate) connect: Duration,
    pub(crate) response_headers: Duration,
    pub(crate) policy_callback: Duration,
    pub(crate) error_body_idle: Duration,
    pub(crate) error_body_total: Duration,
    pub(crate) unary_body_idle: Duration,
    pub(crate) unary_attempt_total: Duration,
    pub(crate) stream_semantic_idle: Duration,
    pub(crate) stream_attempt_total: Duration,
}

impl Default for AttemptDeadlines {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            response_headers: DEFAULT_RESPONSE_HEADER_TIMEOUT,
            policy_callback: DEFAULT_POLICY_CALLBACK_TIMEOUT,
            error_body_idle: DEFAULT_ERROR_BODY_IDLE_TIMEOUT,
            error_body_total: DEFAULT_ERROR_BODY_TOTAL_TIMEOUT,
            unary_body_idle: DEFAULT_UNARY_BODY_IDLE_TIMEOUT,
            unary_attempt_total: DEFAULT_UNARY_ATTEMPT_TIMEOUT,
            stream_semantic_idle: DEFAULT_STREAM_SEMANTIC_IDLE_TIMEOUT,
            stream_attempt_total: DEFAULT_STREAM_ATTEMPT_TIMEOUT,
        }
    }
}

impl AttemptDeadlines {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            connect: env_duration("LLMSHIM_UPSTREAM_CONNECT_TIMEOUT_MS", defaults.connect),
            response_headers: env_duration(
                "LLMSHIM_UPSTREAM_HEADER_TIMEOUT_MS",
                defaults.response_headers,
            ),
            policy_callback: env_duration(
                "LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS",
                defaults.policy_callback,
            ),
            error_body_idle: env_duration(
                "LLMSHIM_UPSTREAM_ERROR_BODY_IDLE_TIMEOUT_MS",
                defaults.error_body_idle,
            ),
            error_body_total: env_duration(
                "LLMSHIM_UPSTREAM_ERROR_BODY_TOTAL_TIMEOUT_MS",
                defaults.error_body_total,
            ),
            unary_body_idle: env_duration(
                "LLMSHIM_UPSTREAM_UNARY_IDLE_TIMEOUT_MS",
                defaults.unary_body_idle,
            ),
            unary_attempt_total: env_duration(
                "LLMSHIM_UPSTREAM_UNARY_ATTEMPT_TIMEOUT_MS",
                defaults.unary_attempt_total,
            ),
            stream_semantic_idle: env_duration(
                "LLMSHIM_UPSTREAM_STREAM_IDLE_TIMEOUT_MS",
                defaults.stream_semantic_idle,
            ),
            stream_attempt_total: env_duration(
                "LLMSHIM_UPSTREAM_STREAM_ATTEMPT_TIMEOUT_MS",
                defaults.stream_attempt_total,
            ),
        }
    }

    pub fn with_connect_timeout(mut self, value: Duration) -> Result<Self, &'static str> {
        self.connect = checked_duration(value)?;
        Ok(self)
    }

    pub fn with_response_header_timeout(mut self, value: Duration) -> Result<Self, &'static str> {
        self.response_headers = checked_duration(value)?;
        Ok(self)
    }

    pub fn with_policy_callback_timeout(mut self, value: Duration) -> Result<Self, &'static str> {
        self.policy_callback = checked_duration(value)?;
        Ok(self)
    }

    pub fn with_error_body_timeouts(
        mut self,
        idle: Duration,
        total: Duration,
    ) -> Result<Self, &'static str> {
        self.error_body_idle = checked_duration(idle)?;
        self.error_body_total = checked_duration(total)?;
        Ok(self)
    }

    pub fn with_unary_timeouts(
        mut self,
        idle: Duration,
        attempt_total: Duration,
    ) -> Result<Self, &'static str> {
        self.unary_body_idle = checked_duration(idle)?;
        self.unary_attempt_total = checked_duration(attempt_total)?;
        Ok(self)
    }

    pub fn with_stream_timeouts(
        mut self,
        semantic_idle: Duration,
        attempt_total: Duration,
    ) -> Result<Self, &'static str> {
        self.stream_semantic_idle = checked_duration(semantic_idle)?;
        self.stream_attempt_total = checked_duration(attempt_total)?;
        Ok(self)
    }

    pub(crate) fn validate(self) -> Result<Self, &'static str> {
        for duration in [
            self.connect,
            self.response_headers,
            self.policy_callback,
            self.error_body_idle,
            self.error_body_total,
            self.unary_body_idle,
            self.unary_attempt_total,
            self.stream_semantic_idle,
            self.stream_attempt_total,
        ] {
            checked_duration(duration)?;
        }
        Ok(self)
    }
}

fn checked_duration(value: Duration) -> Result<Duration, &'static str> {
    if value.is_zero() || tokio::time::Instant::now().checked_add(value).is_none() {
        Err("attempt timeout must be positive and finite")
    } else {
        Ok(value)
    }
}

fn env_duration(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .and_then(|value| checked_duration(value).ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_unrepresentable_builder_values_are_rejected() {
        assert!(AttemptDeadlines::default()
            .with_connect_timeout(Duration::ZERO)
            .is_err());
        assert!(AttemptDeadlines::default()
            .with_stream_timeouts(Duration::from_millis(1), Duration::MAX)
            .is_err());
    }

    #[test]
    fn invalid_environment_values_retain_finite_defaults() {
        let key = "LLMSHIM_UPSTREAM_CONNECT_TIMEOUT_MS";
        let previous = std::env::var_os(key);
        std::env::set_var(key, "0");
        assert_eq!(
            AttemptDeadlines::from_env().connect,
            DEFAULT_CONNECT_TIMEOUT
        );
        std::env::set_var(key, "not-a-duration");
        assert_eq!(
            AttemptDeadlines::from_env().connect,
            DEFAULT_CONNECT_TIMEOUT
        );
        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
