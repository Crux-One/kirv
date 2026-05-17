use super::types::{ControlDecision, EstimatedState};
use std::time::Duration;

const DEAD_BAND_PERCENT: f32 = 0.35;
const INTEGRAL_LIMIT: f32 = 40.0;
const MAX_STOP_FRACTION: f32 = 1.0;
const DEFAULT_KP: f32 = 0.001;
const DEFAULT_KI: f32 = 0.0001;
const MAX_OUTPUT_STEP_UP: f32 = 0.15;
const MAX_OUTPUT_STEP_DOWN: f32 = 0.08;
const MIN_ACTIVE_FRACTION: f32 = 0.05;

pub struct Marshal {
    kp: f32,
    ki: f32,
    setpoint: f32,
    integral: f32,
    prev_output: f32,
    control_period: Duration,
}

impl Marshal {
    pub fn new(setpoint: f32, control_period: Duration) -> Self {
        Self {
            kp: DEFAULT_KP,
            ki: DEFAULT_KI,
            setpoint,
            integral: 0.0,
            prev_output: 0.0,
            control_period,
        }
    }

    pub fn decide(&mut self, estimated: &EstimatedState) -> ControlDecision {
        if !estimated.valid {
            self.prev_output = 0.0;
            return ControlDecision {
                stop_duration: Duration::ZERO,
            };
        }

        let dt_secs = estimated.dt.as_secs_f32();
        if dt_secs <= 0.0 {
            self.prev_output = 0.0;
            return ControlDecision {
                stop_duration: Duration::ZERO,
            };
        }

        let target_output = if estimated.filtered_cpu < self.setpoint - DEAD_BAND_PERCENT {
            self.integral = 0.0;
            0.0
        } else if estimated.filtered_cpu <= self.setpoint + DEAD_BAND_PERCENT {
            self.prev_output
        } else {
            let error = estimated.filtered_cpu - self.setpoint;
            self.integral =
                (self.integral + error * dt_secs).clamp(-INTEGRAL_LIMIT, INTEGRAL_LIMIT);
            let base_stop_fraction =
                proportional_stop_fraction(self.setpoint, estimated.filtered_cpu, self.prev_output);
            let correction = self.kp * error + self.ki * self.integral;
            (base_stop_fraction + correction).clamp(0.0, MAX_STOP_FRACTION)
        };
        let output = limit_output_step(self.prev_output, target_output);
        self.prev_output = output;

        ControlDecision {
            stop_duration: Duration::from_secs_f32(self.control_period.as_secs_f32() * output),
        }
    }
}

fn limit_output_step(prev_output: f32, target_output: f32) -> f32 {
    let delta = target_output - prev_output;
    let limited_delta = delta.clamp(-MAX_OUTPUT_STEP_DOWN, MAX_OUTPUT_STEP_UP);
    (prev_output + limited_delta).clamp(0.0, MAX_STOP_FRACTION)
}

fn proportional_stop_fraction(setpoint: f32, measured_cpu: f32, current_stop_fraction: f32) -> f32 {
    if measured_cpu <= 0.0 {
        return 0.0;
    }

    let active_fraction = (1.0 - current_stop_fraction).clamp(MIN_ACTIVE_FRACTION, 1.0);
    let estimated_unthrottled_cpu = measured_cpu / active_fraction;

    (1.0 - setpoint / estimated_unthrottled_cpu).clamp(0.0, MAX_STOP_FRACTION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_zero_when_state_is_invalid() {
        let mut marshal = Marshal::new(50.0, Duration::from_millis(500));
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 0.0,
            filtered_cpu: 80.0,
            dt: Duration::from_millis(500),
            valid: false,
            warmed_up: true,
            timed_out: false,
        });

        assert_eq!(decision.stop_duration, Duration::ZERO);
    }

    #[test]
    fn clamps_output_to_control_period() {
        let mut marshal = Marshal::new(50.0, Duration::from_millis(500));
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 0.0,
            filtered_cpu: 500.0,
            dt: Duration::from_millis(500),
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        assert!(
            decision.stop_duration
                <= Duration::from_secs_f32(500.0_f32 / 1_000.0 * MAX_STOP_FRACTION)
        );
    }

    #[test]
    fn returns_zero_when_usage_is_below_setpoint() {
        let control_period = Duration::from_millis(500);
        let mut marshal = Marshal::new(20.0, control_period);
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 5.0,
            filtered_cpu: 5.0,
            dt: control_period,
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        assert_eq!(decision.stop_duration, Duration::ZERO);
    }

    #[test]
    fn limits_output_step_up_toward_base_stop_fraction() {
        let control_period = Duration::from_millis(500);
        let mut marshal = Marshal {
            kp: 0.0,
            ki: 0.0,
            setpoint: 20.0,
            integral: 0.0,
            prev_output: 0.0,
            control_period,
        };
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 100.0,
            filtered_cpu: 100.0,
            dt: control_period,
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        let expected = Duration::from_millis(75);
        let delta = decision.stop_duration.abs_diff(expected);
        assert!(delta <= Duration::from_micros(100));
    }

    #[test]
    fn base_stop_fraction_accounts_for_current_stop_fraction() {
        let control_period = Duration::from_millis(600);
        let mut marshal = Marshal {
            kp: 0.0,
            ki: 0.0,
            setpoint: 50.0,
            integral: 0.0,
            prev_output: 0.2,
            control_period,
        };
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 60.0,
            filtered_cpu: 60.0,
            dt: control_period,
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        let expected = Duration::from_millis(200);
        let delta = decision.stop_duration.abs_diff(expected);
        assert!(delta <= Duration::from_micros(100));
    }

    #[test]
    fn proportional_stop_fraction_is_zero_at_or_below_setpoint() {
        assert_eq!(proportional_stop_fraction(50.0, 50.0, 0.0), 0.0);
        assert_eq!(proportional_stop_fraction(50.0, 40.0, 0.0), 0.0);
    }

    #[test]
    fn proportional_stop_fraction_estimates_unthrottled_cpu() {
        let output = proportional_stop_fraction(50.0, 65.0, 0.32);

        assert!((output - 0.4769).abs() < 0.0001);
    }

    #[test]
    fn keeps_previous_output_and_integral_inside_dead_band() {
        let control_period = Duration::from_millis(500);
        let mut marshal = Marshal {
            kp: 0.0,
            ki: 0.0,
            setpoint: 50.0,
            integral: 10.0,
            prev_output: 0.18,
            control_period,
        };
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 50.1,
            filtered_cpu: 50.1,
            dt: control_period,
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        let expected = Duration::from_millis(90);
        let delta = decision.stop_duration.abs_diff(expected);
        assert!(delta <= Duration::from_micros(100));
        assert_eq!(marshal.integral, 10.0);
    }

    #[test]
    fn limits_output_step_down_when_usage_drops_below_setpoint() {
        let control_period = Duration::from_millis(500);
        let mut marshal = Marshal {
            kp: 0.0,
            ki: 0.0,
            setpoint: 20.0,
            integral: 0.0,
            prev_output: 0.8,
            control_period,
        };
        let decision = marshal.decide(&EstimatedState {
            normalized_cpu: 5.0,
            filtered_cpu: 5.0,
            dt: control_period,
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        let expected = Duration::from_millis(360);
        let delta = decision.stop_duration.abs_diff(expected);
        assert!(delta <= Duration::from_micros(100));
    }

    #[test]
    fn resets_integral_when_usage_drops_below_dead_band() {
        let control_period = Duration::from_millis(500);
        let mut marshal = Marshal {
            kp: 0.0,
            ki: 0.0,
            setpoint: 20.0,
            integral: 10.0,
            prev_output: 0.8,
            control_period,
        };
        let _ = marshal.decide(&EstimatedState {
            normalized_cpu: 5.0,
            filtered_cpu: 5.0,
            dt: control_period,
            valid: true,
            warmed_up: true,
            timed_out: false,
        });

        assert_eq!(marshal.integral, 0.0);
    }
}
