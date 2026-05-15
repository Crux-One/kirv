use super::types::{ControlDecision, EstimatedState, RawObservation, TargetGroup};
use std::time::{Duration, Instant};

pub struct Reporter {
    enabled: bool,
    last_emit_at: Instant,
}

impl Reporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn report(
        &mut self,
        target: &TargetGroup,
        observation: Option<&RawObservation>,
        estimated: &EstimatedState,
        decision: &ControlDecision,
    ) {
        if !self.enabled || self.last_emit_at.elapsed() < Duration::from_secs(1) {
            return;
        }

        self.last_emit_at = Instant::now();

        println!(
            "report pid={} pgid={} procs={} normalized_cpu={:.1} filtered_cpu={:.1} valid={} warmed_up={} timed_out={} stop_ms={:.1}",
            target.root.pid,
            target.pgid,
            observation.map(|obs| obs.process_count).unwrap_or(0),
            estimated.normalized_cpu,
            estimated.filtered_cpu,
            estimated.valid,
            estimated.warmed_up,
            estimated.timed_out,
            decision.stop_duration.as_secs_f64() * 1_000.0,
        );
    }
}

impl Default for Reporter {
    fn default() -> Self {
        Self {
            enabled: std::env::var("KIRV_REPORT")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            last_emit_at: Instant::now(),
        }
    }
}
