/// Exponential backoff calculator for reconnection logic.
///
/// Tracks the current delay and attempt count. The delay doubles
/// after each failure, capped at `max_delay`. Calling `reset()`
/// returns the delay to `initial_delay` (used after a stable connection).
use std::time::Duration;

pub struct Backoff {
    initial_delay: Duration,
    max_delay: Duration,
    multiplier: u32,
    current_delay: Duration,
    /// Number of consecutive attempts (resets on `reset()`).
    pub attempt: u32,
}

impl Backoff {
    pub fn new(initial_delay: Duration, max_delay: Duration, multiplier: u32) -> Self {
        Self {
            initial_delay,
            max_delay,
            multiplier,
            current_delay: initial_delay,
            attempt: 0,
        }
    }

    /// Returns the current delay and advances the state.
    /// The delay is multiplied (up to `max_delay`) for the next call.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current_delay;
        self.attempt += 1;
        self.current_delay = (self.current_delay * self.multiplier).min(self.max_delay);
        delay
    }

    /// Resets the backoff to initial state.
    /// Called when a connection has been stable long enough.
    pub fn reset(&mut self) {
        self.current_delay = self.initial_delay;
        self.attempt = 0;
    }

    /// Returns true if the consecutive attempt count has reached `max`.
    pub fn exceeded_max_attempts(&self, max: u32) -> bool {
        self.attempt >= max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exponential_growth() {
        let mut b = Backoff::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            2,
        );
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_secs(8));
        assert_eq!(b.next_delay(), Duration::from_secs(16));
        assert_eq!(b.next_delay(), Duration::from_secs(32));
    }

    #[test]
    fn test_max_delay_cap() {
        let mut b = Backoff::new(
            Duration::from_secs(2),
            Duration::from_secs(10),
            2,
        );
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_secs(8));
        // 8 * 2 = 16, capped at 10
        assert_eq!(b.next_delay(), Duration::from_secs(10));
        assert_eq!(b.next_delay(), Duration::from_secs(10));
        assert_eq!(b.next_delay(), Duration::from_secs(10));
    }

    #[test]
    fn test_reset() {
        let mut b = Backoff::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            2,
        );
        b.next_delay(); // 2
        b.next_delay(); // 4
        b.next_delay(); // 8
        assert_eq!(b.attempt, 3);

        b.reset();
        assert_eq!(b.attempt, 0);
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.attempt, 1);
    }

    #[test]
    fn test_exceeded_max_attempts() {
        let mut b = Backoff::new(
            Duration::from_secs(1),
            Duration::from_secs(60),
            2,
        );
        assert!(!b.exceeded_max_attempts(3));
        b.next_delay();
        assert!(!b.exceeded_max_attempts(3));
        b.next_delay();
        assert!(!b.exceeded_max_attempts(3));
        b.next_delay();
        assert!(b.exceeded_max_attempts(3));
    }

    #[test]
    fn test_attempt_counter() {
        let mut b = Backoff::new(
            Duration::from_secs(1),
            Duration::from_secs(60),
            2,
        );
        assert_eq!(b.attempt, 0);
        b.next_delay();
        assert_eq!(b.attempt, 1);
        b.next_delay();
        assert_eq!(b.attempt, 2);
    }

    #[test]
    fn test_multiplier_three() {
        let mut b = Backoff::new(
            Duration::from_secs(1),
            Duration::from_secs(100),
            3,
        );
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(3));
        assert_eq!(b.next_delay(), Duration::from_secs(9));
        assert_eq!(b.next_delay(), Duration::from_secs(27));
        assert_eq!(b.next_delay(), Duration::from_secs(81));
        // 81 * 3 = 243, capped at 100
        assert_eq!(b.next_delay(), Duration::from_secs(100));
    }
}
