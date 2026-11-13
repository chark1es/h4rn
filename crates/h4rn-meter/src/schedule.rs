//! Pure auto-resume scheduling.
//!
//! A provider reset is used when it is known and inside the policy's maximum
//! wait. An unknown reset gets capped exponential backoff, and the reason says
//! so; h4rn never claims a provider reset it did not observe.

use chrono::{DateTime, Duration, Utc};
use h4rn_protocol::{AutoResumePolicy, ResumeSchedule, ResumeState, ThreadId};

/// The first retry delay when the provider did not give a reset time.
pub const INITIAL_BACKOFF: Duration = Duration::seconds(30);
/// The largest delay between unknown-reset retries.
pub const MAX_BACKOFF: Duration = Duration::minutes(15);

/// A chosen resume time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulePlan {
    /// When to try.
    pub resume_at: DateTime<Utc>,
    /// Whether this is a known reset or a backoff retry.
    pub state: ResumeState,
    /// Why this time was chosen, written for the user.
    pub reason: String,
}

/// Chooses the next resume time.
///
/// * A known reset inside `max_wait` is used, plus deterministic jitter.
/// * A known reset beyond `max_wait` is capped at `max_wait` so a user still
///   gets a retry instead of a silently abandoned thread.
/// * An unknown reset uses `min(30s * 2^attempt, 15min, max_wait)` plus jitter.
pub fn plan_resume(
    thread_id: &ThreadId,
    now: DateTime<Utc>,
    reset_at: Option<DateTime<Utc>>,
    attempt: u32,
    policy: &AutoResumePolicy,
) -> SchedulePlan {
    let max_wait = Duration::milliseconds(policy.max_wait_ms.max(1) as i64);
    let jitter = Duration::milliseconds(deterministic_jitter(thread_id, attempt, policy.jitter_ms));

    match reset_at {
        Some(reset_at) => {
            let wait = (reset_at - now).max(Duration::zero());
            if wait > max_wait {
                let when = now + max_wait + jitter;
                SchedulePlan {
                    resume_at: when,
                    state: ResumeState::Waiting,
                    reason: format!(
                        "the provider reset is beyond the {} max wait; retrying at the cap",
                        human_duration(max_wait)
                    ),
                }
            } else {
                SchedulePlan {
                    resume_at: reset_at + jitter,
                    state: ResumeState::Scheduled,
                    reason: format!(
                        "provider reset at {}",
                        reset_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    ),
                }
            }
        }
        None => {
            let backoff = backoff(attempt).min(max_wait);
            SchedulePlan {
                resume_at: now + backoff + jitter,
                state: ResumeState::Waiting,
                reason: format!(
                    "the provider did not report a reset time; retrying with capped exponential backoff (attempt {})",
                    attempt + 1
                ),
            }
        }
    }
}

/// The capped backoff for an unknown reset.
pub fn backoff(attempt: u32) -> Duration {
    let factor = 1i64.checked_shl(attempt.min(10)).unwrap_or(1024);
    (INITIAL_BACKOFF * factor as i32).min(MAX_BACKOFF)
}

/// Whether a schedule should fire at `now`.
pub fn is_due(schedule: &ResumeSchedule, now: DateTime<Utc>) -> bool {
    matches!(
        schedule.state,
        ResumeState::Scheduled | ResumeState::Waiting
    ) && schedule.resume_at <= now
}

fn deterministic_jitter(thread_id: &ThreadId, attempt: u32, max_ms: u64) -> i64 {
    if max_ms == 0 {
        return 0;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in thread_id.as_str().bytes().chain(attempt.to_be_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % max_ms.max(1)) as i64
}

fn human_duration(duration: Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    fn policy() -> AutoResumePolicy {
        AutoResumePolicy {
            failover: false,
            max_wait_ms: 3_600_000,
            prompt: "continue".into(),
            jitter_ms: 0,
        }
    }

    #[test]
    fn a_known_reset_is_used_verbatim_without_jitter() {
        let plan = plan_resume(&ThreadId::new("th_1"), at(0), Some(at(600)), 0, &policy());
        assert_eq!(plan.resume_at, at(600));
        assert_eq!(plan.state, ResumeState::Scheduled);
        assert!(plan.reason.contains("provider reset"));
    }

    #[test]
    fn an_unknown_reset_uses_capped_backoff_and_says_so() {
        let plan = plan_resume(&ThreadId::new("th_1"), at(0), None, 0, &policy());
        assert_eq!(plan.resume_at, at(30));
        assert_eq!(plan.state, ResumeState::Waiting);
        assert!(plan.reason.contains("did not report a reset time"));
        assert_eq!(backoff(0), Duration::seconds(30));
        assert_eq!(backoff(20), MAX_BACKOFF);
    }

    #[test]
    fn a_reset_beyond_max_wait_is_capped() {
        let plan = plan_resume(&ThreadId::new("th_1"), at(0), Some(at(7_200)), 0, &policy());
        assert_eq!(plan.resume_at, at(3_600));
        assert_eq!(plan.state, ResumeState::Waiting);
        assert!(plan.reason.contains("beyond"));
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let mut policy = policy();
        policy.jitter_ms = 5_000;
        let one = plan_resume(&ThreadId::new("th_1"), at(0), Some(at(100)), 0, &policy);
        let two = plan_resume(&ThreadId::new("th_1"), at(0), Some(at(100)), 0, &policy);
        assert_eq!(one.resume_at, two.resume_at);
        assert!(one.resume_at >= at(100));
        assert!(one.resume_at < at(100) + Duration::seconds(5));
    }
}
