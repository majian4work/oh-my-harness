use serde::{Deserialize, Serialize};

/// Configuration for task-level retry policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of attempts per task (including the first attempt).
    pub max_attempts: u32,
    /// Base delay in milliseconds before the first retry.
    pub base_delay_ms: u64,
    /// Maximum delay cap in milliseconds.
    pub max_delay_ms: u64,
    /// Global retry budget: total retries across all tasks in a run.
    /// Prevents retry storms. Default: task_count * 2.
    pub global_budget: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 1_000,
            max_delay_ms: 60_000,
            global_budget: 0, // set dynamically based on task count
        }
    }
}

impl RetryPolicy {
    /// Create a policy with budget scaled to task count.
    pub fn with_task_count(task_count: usize) -> Self {
        let mut policy = Self::default();
        policy.global_budget = (task_count as u32).saturating_mul(2).max(4);
        policy
    }
}

/// Classification of task-level failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient error that may succeed on retry.
    Retryable,
    /// Permanent error, do not retry.
    NonRetryable,
    /// Unknown — retry once then give up.
    Unknown,
}

/// Classify a task failure based on exit code and stderr output.
pub fn classify_error(exit_code: i32, stderr: &str) -> ErrorClass {
    let lower = stderr.to_ascii_lowercase();

    // OOM kill
    if exit_code == 137 {
        return ErrorClass::Retryable;
    }

    // Rate limits and transient server errors
    if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("429")
        || lower.contains("529")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
        || lower.contains("overloaded")
        || lower.contains("service unavailable")
    {
        return ErrorClass::Retryable;
    }

    // Network errors
    if lower.contains("connection")
        || lower.contains("timeout")
        || lower.contains("dns")
        || lower.contains("reset by peer")
        || lower.contains("broken pipe")
        || lower.contains("econnrefused")
        || lower.contains("econnreset")
        || lower.contains("etimedout")
    {
        return ErrorClass::Retryable;
    }

    // Permanent errors
    if lower.contains("permission denied")
        || lower.contains("invalid prompt")
        || lower.contains("authentication")
        || lower.contains("unauthorized")
        || lower.contains("no provider configured")
    {
        return ErrorClass::NonRetryable;
    }

    ErrorClass::Unknown
}

/// Calculate retry delay with exponential backoff and jitter.
///
/// Same pattern as omh's `agent_runtime.rs` retry_delay:
/// - Base × 2^(attempt-1)
/// - ±25% jitter
/// - Capped at max_delay_ms
pub fn retry_delay(policy: &RetryPolicy, attempt: u32, error_class: ErrorClass) -> u64 {
    let base = if error_class == ErrorClass::Retryable {
        // Rate-limited errors get a higher base to be more respectful
        policy.base_delay_ms.max(2_000)
    } else {
        policy.base_delay_ms
    };

    let exp = base.saturating_mul(1u64 << (attempt.saturating_sub(1).min(10)));
    let capped = exp.min(policy.max_delay_ms);

    // ±25% jitter
    let jitter_range = capped / 4;
    if jitter_range == 0 {
        return capped;
    }
    // Simple deterministic jitter based on attempt number to avoid rand dependency
    let jitter = (attempt as u64 * 7919) % (jitter_range * 2);
    capped.saturating_sub(jitter_range).saturating_add(jitter)
}

/// Whether a task should be retried given the current state.
pub fn should_retry(
    attempt_count: u32,
    max_attempts: u32,
    error_class: ErrorClass,
    global_retries_used: u32,
    global_budget: u32,
) -> bool {
    // Budget exhausted
    if global_budget > 0 && global_retries_used >= global_budget {
        return false;
    }

    match error_class {
        ErrorClass::Retryable => attempt_count < max_attempts,
        ErrorClass::NonRetryable => false,
        // Unknown errors get one retry
        ErrorClass::Unknown => attempt_count < 2.min(max_attempts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rate_limit() {
        assert_eq!(
            classify_error(1, "Error: rate limit exceeded"),
            ErrorClass::Retryable
        );
        assert_eq!(
            classify_error(1, "HTTP 429 Too Many Requests"),
            ErrorClass::Retryable
        );
    }

    #[test]
    fn classify_oom() {
        assert_eq!(classify_error(137, ""), ErrorClass::Retryable);
    }

    #[test]
    fn classify_network() {
        assert_eq!(
            classify_error(1, "connection reset by peer"),
            ErrorClass::Retryable
        );
        assert_eq!(
            classify_error(1, "DNS resolution failed"),
            ErrorClass::Retryable
        );
    }

    #[test]
    fn classify_permanent() {
        assert_eq!(
            classify_error(1, "Error: permission denied"),
            ErrorClass::NonRetryable
        );
        assert_eq!(
            classify_error(1, "no provider configured"),
            ErrorClass::NonRetryable
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify_error(1, "some weird error"), ErrorClass::Unknown);
    }

    #[test]
    fn backoff_increases() {
        let policy = RetryPolicy::default();
        let d1 = retry_delay(&policy, 1, ErrorClass::Retryable);
        let d2 = retry_delay(&policy, 2, ErrorClass::Retryable);
        let d3 = retry_delay(&policy, 3, ErrorClass::Retryable);
        // Each delay should generally increase (modulo jitter)
        assert!(d2 > d1 / 2);
        assert!(d3 > d2 / 2);
    }

    #[test]
    fn backoff_capped() {
        let policy = RetryPolicy {
            max_delay_ms: 5_000,
            ..Default::default()
        };
        let d = retry_delay(&policy, 20, ErrorClass::Retryable);
        assert!(d <= 5_000 + 1_250); // cap + max jitter
    }

    #[test]
    fn should_retry_respects_budget() {
        assert!(!should_retry(1, 3, ErrorClass::Retryable, 10, 10));
    }

    #[test]
    fn should_retry_unknown_once() {
        assert!(should_retry(1, 3, ErrorClass::Unknown, 0, 100));
        assert!(!should_retry(2, 3, ErrorClass::Unknown, 0, 100));
    }

    #[test]
    fn should_retry_non_retryable_never() {
        assert!(!should_retry(0, 3, ErrorClass::NonRetryable, 0, 100));
    }
}
