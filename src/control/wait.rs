use std::time::Duration;

const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) fn with_stop_check(
    duration: Duration,
    mut should_stop: impl FnMut() -> bool,
    mut sleep: impl FnMut(Duration),
) {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if should_stop() {
            return;
        }

        let slice = remaining.min(STOP_POLL_INTERVAL);
        sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn with_stop_check_finishes_when_stop_is_not_requested() {
        let slept = Cell::new(Duration::ZERO);

        with_stop_check(
            Duration::from_millis(25),
            || false,
            |slice| {
                slept.set(slept.get() + slice);
            },
        );

        assert_eq!(slept.get(), Duration::from_millis(25));
    }

    #[test]
    fn with_stop_check_stops_early_when_stop_is_requested() {
        let slept = Cell::new(Duration::ZERO);
        let polls = Cell::new(0usize);

        with_stop_check(
            Duration::from_millis(50),
            || {
                let next = polls.get() + 1;
                polls.set(next);
                next >= 3
            },
            |slice| slept.set(slept.get() + slice),
        );

        assert_eq!(slept.get(), Duration::from_millis(20));
    }
}
