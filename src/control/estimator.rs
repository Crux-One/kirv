use super::types::{EstimatedState, RawObservation};
use std::time::{Duration, Instant};

const WARM_UP_SAMPLES: usize = 2;
const TIMEOUT_THRESHOLD_CYCLES: usize = 3;
const HOLD_LIMIT_CYCLES: usize = 2;
const MAX_JUMP_PER_SAMPLE: f32 = 30.0;
const LOW_PASS_TAU: Duration = Duration::from_millis(500);

pub struct Estimator {
    control_period: Duration,
    prev_timestamp: Option<Instant>,
    prev_filtered_cpu: f32,
    last_good_normalized: Option<f32>,
    last_good_filtered: Option<f32>,
    warm_up_samples: usize,
    missed_cycles: usize,
}

impl Estimator {
    pub fn new(control_period: Duration) -> Self {
        Self {
            control_period,
            prev_timestamp: None,
            prev_filtered_cpu: 0.0,
            last_good_normalized: None,
            last_good_filtered: None,
            warm_up_samples: 0,
            missed_cycles: 0,
        }
    }

    pub fn update(&mut self, observation: Option<&RawObservation>) -> EstimatedState {
        match observation {
            Some(observation) => self.update_with_observation(observation),
            None => self.update_without_observation(),
        }
    }

    fn update_with_observation(&mut self, observation: &RawObservation) -> EstimatedState {
        let dt = self.next_dt(observation.timestamp);
        self.missed_cycles = 0;
        self.warm_up_samples = (self.warm_up_samples + 1).min(WARM_UP_SAMPLES);

        let normalized_cpu = observation
            .per_pid_cpu
            .iter()
            .map(|(_, cpu)| cpu.max(0.0))
            .sum::<f32>();

        let clamped_cpu = match self.last_good_normalized {
            Some(last_good_normalized) => {
                let min = (last_good_normalized - MAX_JUMP_PER_SAMPLE).max(0.0);
                let max = last_good_normalized + MAX_JUMP_PER_SAMPLE;
                normalized_cpu.clamp(min, max)
            }
            None => normalized_cpu,
        };

        let alpha = low_pass_alpha(dt);
        let filtered_cpu = alpha * clamped_cpu + (1.0 - alpha) * self.prev_filtered_cpu;
        self.prev_filtered_cpu = filtered_cpu;

        let warmed_up = self.warm_up_samples >= WARM_UP_SAMPLES;
        let valid = warmed_up;
        let timed_out = false;

        self.last_good_normalized = Some(clamped_cpu);
        if valid {
            self.last_good_filtered = Some(filtered_cpu);
        }

        EstimatedState {
            normalized_cpu: clamped_cpu,
            filtered_cpu,
            dt,
            valid,
            warmed_up,
            timed_out,
        }
    }

    fn update_without_observation(&mut self) -> EstimatedState {
        self.missed_cycles += 1;

        let dt = self.control_period;
        let warmed_up = self.warm_up_samples >= WARM_UP_SAMPLES;
        let timed_out = self.missed_cycles >= TIMEOUT_THRESHOLD_CYCLES;
        let can_hold = self.missed_cycles <= HOLD_LIMIT_CYCLES;
        let held_normalized = self.last_good_normalized.unwrap_or(0.0);
        let held_filtered = self.last_good_filtered.unwrap_or(0.0);
        let valid = warmed_up
            && !timed_out
            && can_hold
            && self.last_good_normalized.is_some()
            && self.last_good_filtered.is_some();

        EstimatedState {
            normalized_cpu: held_normalized,
            filtered_cpu: held_filtered,
            dt,
            valid,
            warmed_up,
            timed_out,
        }
    }

    fn next_dt(&mut self, timestamp: Instant) -> Duration {
        let dt = self
            .prev_timestamp
            .map(|prev| timestamp.saturating_duration_since(prev))
            .unwrap_or(self.control_period);
        self.prev_timestamp = Some(timestamp);
        dt
    }
}

fn low_pass_alpha(dt: Duration) -> f32 {
    let dt_secs = dt.as_secs_f32().max(0.0);
    let tau_secs = LOW_PASS_TAU.as_secs_f32();

    if dt_secs <= 0.0 || tau_secs <= 0.0 {
        1.0
    } else {
        (1.0 - (-dt_secs / tau_secs).exp()).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_warmup_before_becoming_valid() {
        let mut estimator = Estimator::new(Duration::from_millis(500));
        let sample = RawObservation {
            timestamp: Instant::now(),
            per_pid_cpu: vec![(1, 60.0)],
            process_count: 1,
        };

        let first = estimator.update(Some(&sample));
        assert!(!first.valid);

        let second_sample = RawObservation {
            timestamp: sample.timestamp + Duration::from_millis(500),
            per_pid_cpu: sample.per_pid_cpu.clone(),
            process_count: sample.process_count,
        };
        let second = estimator.update(Some(&second_sample));
        assert!(second.valid);
        assert!(second.warmed_up);
    }

    #[test]
    fn times_out_after_three_missing_cycles() {
        let mut estimator = Estimator::new(Duration::from_millis(500));
        let base = Instant::now();

        let first = RawObservation {
            timestamp: base,
            per_pid_cpu: vec![(1, 10.0)],
            process_count: 1,
        };
        let second = RawObservation {
            timestamp: base + Duration::from_millis(500),
            per_pid_cpu: vec![(1, 12.0)],
            process_count: 1,
        };
        let _ = estimator.update(Some(&first));
        let _ = estimator.update(Some(&second));

        assert!(estimator.update(None).valid);
        assert!(estimator.update(None).valid);

        let timed_out = estimator.update(None);
        assert!(!timed_out.valid);
        assert!(timed_out.timed_out);
    }

    #[test]
    fn hold_keeps_normalized_and_filtered_separately() {
        let mut estimator = Estimator::new(Duration::from_millis(500));
        let base = Instant::now();

        let _ = estimator.update(Some(&RawObservation {
            timestamp: base,
            per_pid_cpu: vec![(1, 40.0)],
            process_count: 1,
        }));
        let warmed = estimator.update(Some(&RawObservation {
            timestamp: base + Duration::from_millis(500),
            per_pid_cpu: vec![(1, 60.0)],
            process_count: 1,
        }));

        let held = estimator.update(None);
        assert!(held.valid);
        assert_eq!(held.normalized_cpu, warmed.normalized_cpu);
        assert_eq!(held.filtered_cpu, warmed.filtered_cpu);
    }

    #[test]
    fn sums_group_cpu_usage_above_one_logical_cpu() {
        let mut estimator = Estimator::new(Duration::from_millis(500));
        let base = Instant::now();
        let sample = RawObservation {
            timestamp: base,
            per_pid_cpu: vec![(1, 120.0), (2, 90.0)],
            process_count: 2,
        };

        let _ = estimator.update(Some(&sample));
        let warmed = estimator.update(Some(&RawObservation {
            timestamp: base + Duration::from_millis(500),
            per_pid_cpu: sample.per_pid_cpu.clone(),
            process_count: sample.process_count,
        }));

        assert!(warmed.valid);
        assert_eq!(warmed.normalized_cpu, 210.0);
        assert!(warmed.filtered_cpu > 0.0);
    }

    #[test]
    fn clamps_second_warmup_sample_against_first_observation() {
        let mut estimator = Estimator::new(Duration::from_millis(500));
        let base = Instant::now();

        let first = RawObservation {
            timestamp: base,
            per_pid_cpu: vec![(1, 10.0)],
            process_count: 1,
        };
        let second = RawObservation {
            timestamp: base + Duration::from_millis(500),
            per_pid_cpu: vec![(1, 1000.0)],
            process_count: 1,
        };

        let _ = estimator.update(Some(&first));
        let warmed = estimator.update(Some(&second));

        assert!(warmed.valid);
        assert_eq!(warmed.normalized_cpu, 10.0 + MAX_JUMP_PER_SAMPLE);
    }

    #[test]
    fn low_pass_alpha_increases_with_dt() {
        let short = low_pass_alpha(Duration::from_millis(100));
        let long = low_pass_alpha(Duration::from_millis(500));

        assert!(short > 0.0);
        assert!(long > short);
        assert!(long <= 1.0);
    }
}
