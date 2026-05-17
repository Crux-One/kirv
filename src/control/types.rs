use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub pgid: i32,
    pub start_tvsec: u64,
    pub start_tvusec: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetGroup {
    pub root: ProcessIdentity,
    pub pgid: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTarget {
    pub pgid: i32,
    pub stopped_pgid: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct RawObservation {
    pub timestamp: Instant,
    pub per_pid_cpu: Vec<(i32, f32)>,
    pub process_count: usize,
}

#[derive(Clone, Debug)]
pub struct EstimatedState {
    pub normalized_cpu: f32,
    pub filtered_cpu: f32,
    pub dt: Duration,
    pub valid: bool,
    pub warmed_up: bool,
    pub timed_out: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlDecision {
    pub stop_duration: Duration,
}
