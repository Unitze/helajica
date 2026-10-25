use std::time::Duration;

use maki_config::{
    DEFAULT_MAX_RETRIES, DEFAULT_MAX_TIMEOUT_RETRIES, DEFAULT_RETRY_BASE_MS, DEFAULT_RETRY_MAX_MS,
    DEFAULT_RETRY_MAX_WAIT_SECS, ProviderConfig,
};
use tracing::warn;

/// What kind of failure a retry is paying for. The ways out of here are not
/// the same: some errors say "the provider is having a bad day", one says "you
/// are out of money until next month", one says "we may already have billed
/// you for this", and one says "there may be nothing at that address at all".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// 5xx and network failures on a connection that was made. Nothing came
    /// back, so trying again only costs time.
    Transient,
    /// A connection that was never established: refused, or a host name that
    /// does not resolve.
    Connect,
    /// A 429. Whether the server sent a `Retry-After` decides which budget it
    /// draws from, so [`RetryState`] splits it further.
    RateLimit,
    /// A stream that went quiet. The request was accepted and output tokens may
    /// already be on the bill, so every retry costs real money.
    Timeout,
}

/// Which counter a failure draws from, and whether that counter has a ceiling.
#[derive(Debug, Clone, Copy)]
enum Budget {
    /// Server errors and interrupted connections.
    Transient,
    /// A server-directed cooldown, counted separately from unhinted limits.
    HintedRateLimit,
    /// Bounded: a 429 with no `Retry-After` is how a spend cap reads, and that
    /// one does not clear until the next billing period, so retrying is
    /// documented not to help.
    RateLimit,
    /// Bounded: see [`RetryKind::Timeout`].
    Timeout,
    /// Bounded, on the same limit as [`Self::RateLimit`] but its own counter.
    ///
    /// "Could not connect" is ambiguous: a cloud provider whose edge is down
    /// reads exactly like a local server nobody started. We bound it because
    /// nothing listening is by far the more common cause, and a typo in a
    /// `base_url` has to end in an error rather than a run that never stops
    /// trying.
    Connect,
}

const BUDGETS: usize = 5;

impl Budget {
    fn of(kind: RetryKind, hint: Option<Duration>) -> Self {
        match kind {
            RetryKind::Transient => Self::Transient,
            RetryKind::Connect => Self::Connect,
            RetryKind::RateLimit if hint.is_some() => Self::HintedRateLimit,
            RetryKind::RateLimit => Self::RateLimit,
            RetryKind::Timeout => Self::Timeout,
        }
    }

    fn limit(self, policy: &RetryPolicy) -> u32 {
        match self {
            Self::Transient | Self::HintedRateLimit | Self::RateLimit | Self::Connect => {
                policy.max_retries
            }
            Self::Timeout => policy.max_timeout_retries,
        }
    }
}

/// How patient a run is with a provider that keeps failing. Read from user
/// config, because a rate limited cloud endpoint and a local llama.cpp want
/// very different numbers.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub max_wait: Duration,
    /// Retry budget per failure kind. `0` disables automatic retries.
    pub max_retries: u32,
    pub max_timeout_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(DEFAULT_RETRY_BASE_MS),
            max_delay: Duration::from_millis(DEFAULT_RETRY_MAX_MS),
            max_wait: Duration::from_secs(DEFAULT_RETRY_MAX_WAIT_SECS),
            max_retries: DEFAULT_MAX_RETRIES,
            max_timeout_retries: DEFAULT_MAX_TIMEOUT_RETRIES,
        }
    }
}

impl From<&ProviderConfig> for RetryPolicy {
    fn from(config: &ProviderConfig) -> Self {
        Self {
            base_delay: Duration::from_millis(config.retry_base_ms),
            max_delay: Duration::from_millis(config.retry_max_ms),
            max_wait: Duration::from_secs(config.retry_max_wait_secs),
            max_retries: config.max_retries,
            max_timeout_retries: config.max_timeout_retries,
        }
    }
}

#[derive(Debug)]
pub struct RetryState {
    policy: RetryPolicy,
    spent: [u32; BUDGETS],
    /// Keys left to try, which is not a budget: a fresh key cures a different
    /// problem than waiting does, so a rotation costs no time and no retry.
    ///
    /// Counted per request because `KeyPool`'s index is shared. Two turns
    /// rotating at once shove that index around, and neither could tell it had
    /// been all the way around the pool. A local count bounds the walk
    /// regardless: the worst a jumping index costs is a skipped key, never a
    /// spin.
    rotations_left: u32,
}

impl RetryState {
    /// `key_count` is how many keys the provider has; one means there is
    /// nowhere to rotate to.
    pub fn new(policy: RetryPolicy, key_count: usize) -> Self {
        Self {
            policy,
            spent: [0; BUDGETS],
            rotations_left: u32::try_from(key_count)
                .unwrap_or(u32::MAX)
                .saturating_sub(1),
        }
    }

    /// Books a step of the key walk, or `false` once every key has been tried.
    pub fn book_rotation(&mut self) -> bool {
        if self.rotations_left == 0 {
            return false;
        }
        self.rotations_left -= 1;
        true
    }

    /// Books the next retry and says how long to wait, or `None` once this
    /// failure's budget is spent.
    ///
    /// Budgets never touch: a flaky connection that burns its whole budget must
    /// not leave a later rate limit with nothing left to spend. `hint` is a
    /// `Retry-After` the server sent, and it knows its own window better than
    /// we do. Waits above [`RetryPolicy::max_wait`] return the original error.
    pub fn next_delay(&mut self, kind: RetryKind, hint: Option<Duration>) -> Option<Duration> {
        if hint.is_some_and(|after| after > self.policy.max_wait) {
            warn!(
                retry_after_ms = hint.map(|after| after.as_millis() as u64),
                max_wait_ms = self.policy.max_wait.as_millis() as u64,
                "server retry delay exceeds automatic wait limit; preserving error"
            );
            return None;
        }
        let budget = Budget::of(kind, hint);
        let spent = &mut self.spent[budget as usize];
        if *spent >= budget.limit(&self.policy) {
            return None;
        }
        *spent += 1;
        let attempt = *spent;

        let Some(after) = hint else {
            let delay = self.backoff(attempt);
            return (delay <= self.policy.max_wait).then_some(delay);
        };
        Some(after.max(self.policy.base_delay.min(self.policy.max_wait)))
    }

    /// Half the backoff, plus jitter on top, so a fleet of clients knocked back
    /// by the same 429 does not all come back at the same moment.
    fn backoff(&self, attempt: u32) -> Duration {
        let half = self
            .policy
            .base_delay
            .saturating_mul(attempt)
            .min(self.policy.max_delay)
            / 2;
        let jitter = Duration::from_millis(fastrand::u64(0..=half.as_millis() as u64));
        half + jitter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const BASE: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_millis(250);
    /// Enough rounds that anything unbounded is obviously unbounded, cheap
    /// because nothing here sleeps.
    const MANY: u32 = 1_000;
    /// A pool with nowhere to rotate to, which is what every budget test wants.
    const ONE_KEY: usize = 1;
    /// A pool big enough that a walk over it is obviously more than one step.
    const KEYS: usize = 3;

    fn state(max_retries: u32, max_timeout_retries: u32) -> RetryState {
        RetryState::new(
            RetryPolicy {
                base_delay: BASE,
                max_delay: MAX,
                max_retries,
                max_timeout_retries,
                ..RetryPolicy::default()
            },
            ONE_KEY,
        )
    }

    /// Retries until the budget runs out or `MANY` rounds pass, checking on the
    /// way that every delay lands in the half-to-full band the backoff promises
    /// for that attempt.
    fn spend(state: &mut RetryState, kind: RetryKind) -> u32 {
        let mut retried = 0;
        while retried < MANY
            && let Some(delay) = state.next_delay(kind, None)
        {
            let want = BASE.saturating_mul(retried + 1).min(MAX) / 2;
            assert!((want..=want * 2).contains(&delay), "{delay:?}");
            retried += 1;
        }
        retried
    }

    #[test_case(0, RetryKind::RateLimit, 0    ; "zero_budget_never_retries_an_unhinted_rate_limit")]
    #[test_case(2, RetryKind::RateLimit, 2    ; "an_unhinted_rate_limit_stops_at_its_budget")]
    #[test_case(2, RetryKind::Timeout, 3      ; "timeouts_have_their_own_budget")]
    #[test_case(0, RetryKind::Transient, 0 ; "zero_budget_stops_server_errors")]
    #[test_case(2, RetryKind::Transient, 2 ; "server_errors_stop_at_the_budget")]
    #[test_case(0, RetryKind::Connect, 0      ; "zero_budget_never_retries_a_refused_connection")]
    #[test_case(2, RetryKind::Connect, 2      ; "a_refused_connection_stops_at_its_budget")]
    fn budget_is_spent_then_exhausted(max_retries: u32, kind: RetryKind, expected: u32) {
        assert_eq!(spend(&mut state(max_retries, 3), kind), expected);
    }

    #[test]
    fn budgets_cannot_eat_each_other() {
        let mut state = state(2, 3);
        assert_eq!(spend(&mut state, RetryKind::Timeout), 3);
        assert_eq!(spend(&mut state, RetryKind::Connect), 2);
        assert_eq!(spend(&mut state, RetryKind::RateLimit), 2);
    }

    #[test_case(RetryKind::RateLimit ; "rate_limit")]
    #[test_case(RetryKind::Transient ; "server_error")]
    fn long_server_cooldowns_stop_without_retrying_early(kind: RetryKind) {
        let mut state = state(2, 3);
        let hint = Some(Duration::from_secs(DEFAULT_RETRY_MAX_WAIT_SECS + 1));
        assert_eq!(state.next_delay(kind, hint), None);
        assert!(
            state
                .next_delay(kind, Some(Duration::from_secs(5)))
                .is_some()
        );
    }

    #[test]
    fn a_hinted_rate_limit_leaves_the_unhinted_budget_alone() {
        let mut state = state(2, 3);
        let hint = Some(Duration::from_secs(5));
        for _ in 0..2 {
            assert!(state.next_delay(RetryKind::RateLimit, hint).is_some());
        }
        assert!(state.next_delay(RetryKind::RateLimit, hint).is_none());
        assert_eq!(spend(&mut state, RetryKind::RateLimit), 2);
    }

    #[test_case(Duration::from_secs(5), Duration::from_secs(5)  ; "sane_hint_beats_the_guess")]
    // An unbounded budget plus a zero or near-zero hint is a tight loop against
    // a rate limiter, so the floor is load bearing, not decoration.
    #[test_case(Duration::ZERO, BASE                            ; "zero_hint_is_raised_to_the_base_delay")]
    #[test_case(Duration::from_millis(1), BASE                  ; "tiny_hint_is_raised_to_the_base_delay")]
    fn server_hint_is_honored_between_the_floor_and_the_cap(hint: Duration, expected: Duration) {
        assert_eq!(
            state(1, 1).next_delay(RetryKind::RateLimit, Some(hint)),
            Some(expected)
        );
    }

    /// A base delay above the cap must clamp, not panic on an inverted range.
    #[test]
    fn a_base_delay_above_the_cap_still_clamps() {
        let mut state = RetryState::new(
            RetryPolicy {
                base_delay: Duration::from_secs(DEFAULT_RETRY_MAX_WAIT_SECS * 2),
                ..RetryPolicy::default()
            },
            ONE_KEY,
        );
        let hint = Some(Duration::from_secs(1));
        assert_eq!(
            state.next_delay(RetryKind::RateLimit, hint),
            Some(Duration::from_secs(DEFAULT_RETRY_MAX_WAIT_SECS))
        );
    }

    /// Walks the pool until `book_rotation` says stop, which must be once every
    /// key has been tried: the one the request started on, plus one per step.
    fn walk(state: &mut RetryState) -> u32 {
        let mut rotations = 0;
        while rotations < MANY && state.book_rotation() {
            rotations += 1;
        }
        rotations
    }

    #[test_case(1, 0  ; "a_single_key_has_nowhere_to_rotate_to")]
    #[test_case(2, 1  ; "a_pair_of_keys_rotates_once")]
    #[test_case(10, 9 ; "ten_keys_rotate_nine_times")]
    fn the_key_walk_visits_every_key_once(key_count: usize, expected: u32) {
        assert_eq!(
            walk(&mut RetryState::new(RetryPolicy::default(), key_count)),
            expected
        );
    }

    /// Rotating is a different remedy than waiting, so a walked out pool has to
    /// leave the error every budget it started with.
    #[test]
    fn a_walked_pool_costs_no_budget() {
        const RETRIES: u32 = 2;
        let mut state = RetryState::new(
            RetryPolicy {
                base_delay: BASE,
                max_delay: MAX,
                max_retries: RETRIES,
                max_timeout_retries: RETRIES,
                ..RetryPolicy::default()
            },
            KEYS,
        );
        assert_eq!(walk(&mut state), KEYS as u32 - 1);
        assert_eq!(spend(&mut state, RetryKind::RateLimit), RETRIES);
        assert_eq!(spend(&mut state, RetryKind::Timeout), RETRIES);
    }
}
