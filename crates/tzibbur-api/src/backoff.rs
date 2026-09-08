//! Exponential backoff with jitter, shared by the socket reconnect loop and the
//! outbox dispatcher: `min(BACKOFF_CAP, BACKOFF_BASE << cappedExponent)`.

use crate::constants::{BACKOFF_BASE, BACKOFF_CAP};
use rand::Rng;
use std::time::Duration;

/// Deterministic (un-jittered) delay for the given attempt number (0-based).
pub fn backoff_base(attempt: u32) -> Duration {
    let exp = attempt.min(16);
    let ms = (BACKOFF_BASE.as_millis() as u64).saturating_mul(1u64 << exp);
    Duration::from_millis(ms).min(BACKOFF_CAP)
}

/// Jittered delay: uniformly random in `[base/2, base]`.
pub fn backoff_delay(attempt: u32) -> Duration {
    let base = backoff_base(attempt).as_millis() as u64;
    let lo = base / 2;
    let ms = rand::thread_rng().gen_range(lo..=base.max(lo + 1));
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_at_five_minutes() {
        assert_eq!(backoff_base(0), Duration::from_secs(2));
        assert_eq!(backoff_base(1), Duration::from_secs(4));
        assert_eq!(backoff_base(7), Duration::from_secs(256));
        assert_eq!(backoff_base(8), BACKOFF_CAP);
        assert_eq!(backoff_base(100), BACKOFF_CAP);
        let j = backoff_delay(3);
        assert!(j >= Duration::from_secs(8) && j <= Duration::from_secs(16));
    }
}
