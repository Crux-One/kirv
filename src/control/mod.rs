mod enforcer;
mod estimator;
mod marshal;
mod recon;
mod reporter;
mod types;
mod wait;

use nix::unistd::getpgrp;
use std::{
    env,
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::Duration,
};

use enforcer::Enforcer;
use estimator::Estimator;
use marshal::Marshal;
use recon::Recon;
use reporter::Reporter;
use types::{ActiveTarget, ControlDecision, EstimatedState, RawObservation};

static STOP_SIGNAL: AtomicBool = AtomicBool::new(false);
static ACTIVE_TARGET: Mutex<Option<ActiveTarget>> = Mutex::new(None);
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

const CONTROL_PERIOD: Duration = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;

#[derive(Debug)]
struct Args {
    pid: i32,
    throttle: f32,
}

#[derive(Debug)]
pub enum ControlError {
    InvalidArguments(&'static str),
    InvalidPid(std::num::ParseIntError),
    InvalidThrottle(std::num::ParseFloatError),
    ResumeFailed(std::io::Error),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArguments(msg) => write!(f, "{msg}"),
            Self::InvalidPid(err) => write!(f, "invalid pid: {err}"),
            Self::InvalidThrottle(err) => write!(f, "invalid throttle percentage: {err}"),
            Self::ResumeFailed(err) => write!(f, "failed to resume target process group: {err}"),
        }
    }
}

impl Error for ControlError {}

impl Args {
    fn new(pid: i32, throttle: f32) -> Self {
        Self { pid, throttle }
    }
}

struct ActiveGroupGuard;

impl Drop for ActiveGroupGuard {
    fn drop(&mut self) {
        if let Err(err) = try_resume_active_target() {
            eprintln!("failed to resume stopped target members during shutdown: {err}");
        }
    }
}

pub fn start() -> Result<(), Box<dyn Error>> {
    let args = get_args()?;

    let mut recon = Recon::new();
    let target = recon.resolve_target_group(args.pid)?;
    guard_target_group(target.pgid)?;
    set_active_target(ActiveTarget { stopped_pgid: None });
    let _active_group_guard = ActiveGroupGuard;

    let mut estimator = Estimator::new(CONTROL_PERIOD);
    let mut marshal = Marshal::new(args.throttle, CONTROL_PERIOD);
    let enforcer = Enforcer::new();
    let mut reporter = Reporter::new();

    println!("target pid: {}", target.root.pid);
    println!("target pgid: {}", target.pgid);
    println!("throttle: {}% of one logical CPU", args.throttle);

    loop {
        if STOP_SIGNAL.load(Ordering::SeqCst) {
            break;
        }

        let loop_start = std::time::Instant::now();
        if !recon.validate_target_group(&target)? {
            eprintln!(
                "target pgid {} has no live processes; stopping control loop",
                target.pgid
            );
            break;
        }
        let observation = recon.observe_group(&target)?;
        let (estimated, decision) =
            compute_control_decision(&mut estimator, &mut marshal, observation.as_ref());
        reporter.report(&target, observation.as_ref(), &estimated, &decision);
        enforcer.apply(&target, &decision)?;

        let elapsed = loop_start.elapsed();
        if elapsed < CONTROL_PERIOD {
            wait::with_stop_check(CONTROL_PERIOD - elapsed, stop_requested, thread::sleep);
        }
    }

    Ok(())
}

pub fn stop() -> Result<(), ControlError> {
    switch_stop_signal();

    try_resume_active_target().map_err(ControlError::ResumeFailed)?;

    Ok(())
}

fn get_args() -> Result<Args, ControlError> {
    let args: Vec<String> = env::args().collect();
    parse_args(&args)
}

fn parse_args(args: &[String]) -> Result<Args, ControlError> {
    if args.len() != 3 {
        return Err(ControlError::InvalidArguments(
            "usage: kirv <pid> <percentage>",
        ));
    }

    let pid: i32 = args[1].trim().parse().map_err(ControlError::InvalidPid)?;
    if pid <= 0 {
        return Err(ControlError::InvalidArguments("pid must be greater than 0"));
    }
    let percentage = args[2]
        .trim()
        .parse()
        .map_err(ControlError::InvalidThrottle)?;

    if !(1.0..=99.0).contains(&percentage) {
        return Err(ControlError::InvalidArguments(
            "percentage must be between 1 and 99",
        ));
    }

    Ok(Args::new(pid, percentage))
}

fn guard_target_group(target_pgid: i32) -> Result<(), Box<dyn Error>> {
    let current_pgid = getpgrp().as_raw();
    if current_pgid == target_pgid {
        return Err("refusing to control the current process group".into());
    }

    Ok(())
}

fn switch_stop_signal() {
    STOP_SIGNAL.store(true, Ordering::SeqCst);
}

fn stop_requested() -> bool {
    STOP_SIGNAL.load(Ordering::SeqCst)
}

fn set_active_target(active_target: ActiveTarget) {
    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    *slot = Some(active_target);
}

fn set_stopped_group(pgid: i32) {
    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    if let Some(active_target) = slot.as_mut() {
        active_target.stopped_pgid = Some(pgid);
    }
}

fn clear_stopped_group() {
    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    if let Some(active_target) = slot.as_mut() {
        active_target.stopped_pgid = None;
    }
}

fn current_active_target() -> Option<ActiveTarget> {
    ACTIVE_TARGET
        .lock()
        .expect("active target mutex poisoned")
        .clone()
}

fn try_resume_active_target() -> std::io::Result<()> {
    try_resume_active_target_with(resume_tracked_members)
}

fn try_resume_active_target_with<F>(resume: F) -> std::io::Result<()>
where
    F: FnOnce(ActiveTarget) -> std::io::Result<()>,
{
    let Some(active_target) = current_active_target() else {
        return Ok(());
    };

    resume(active_target.clone())?;

    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    if slot.as_ref() == Some(&active_target) {
        *slot = None;
    }

    Ok(())
}

fn resume_tracked_members(active_target: ActiveTarget) -> std::io::Result<()> {
    let Some(stopped_pgid) = active_target.stopped_pgid else {
        return Ok(());
    };

    let enforcer = Enforcer::new();
    match enforcer.resume_group(stopped_pgid) {
        Ok(()) => Ok(()),
        Err(group_err) => {
            let pids = current_group_pids(stopped_pgid)?;
            enforcer
                .resume_pids(&pids)
                .map_err(|pid_err| std::io::Error::other(format!(
                    "failed to resume target group: {group_err}; fallback resume by pid failed: {pid_err}"
                )))
        }
    }
}

fn current_group_pids(pgid: i32) -> std::io::Result<Vec<i32>> {
    recon::group_pids(pgid)
}

fn compute_control_decision(
    estimator: &mut Estimator,
    marshal: &mut Marshal,
    observation: Option<&RawObservation>,
) -> (EstimatedState, ControlDecision) {
    // Missing samples can be transient; let the estimator's hold/timeout logic decide
    // when the controller should degrade rather than exiting the loop immediately.
    let estimated = estimator.update(observation);
    let decision = marshal.decide(&estimated);
    (estimated, decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::MutexGuard, time::Instant};

    struct GlobalStateGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl GlobalStateGuard {
        fn acquire() -> Self {
            let lock = TEST_LOCK.lock().expect("test lock poisoned");
            reset_test_globals();
            Self { _lock: lock }
        }
    }

    impl Drop for GlobalStateGuard {
        fn drop(&mut self) {
            reset_test_globals();
        }
    }

    fn reset_test_globals() {
        STOP_SIGNAL.store(false, Ordering::SeqCst);
        let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
        *slot = None;
    }

    #[test]
    fn rejects_current_process_group() {
        let current_pgid = getpgrp().as_raw();
        assert!(guard_target_group(current_pgid).is_err());
    }

    #[test]
    fn accepts_non_current_process_group() {
        let current_pgid = getpgrp().as_raw();
        assert!(guard_target_group(current_pgid.saturating_add(1)).is_ok());
    }

    #[test]
    fn parse_args_rejects_invalid_arity() {
        let args = vec!["kirv".to_string(), "123".to_string()];
        assert!(matches!(
            parse_args(&args),
            Err(ControlError::InvalidArguments(
                "usage: kirv <pid> <percentage>"
            ))
        ));
    }

    #[test]
    fn parse_args_rejects_invalid_pid() {
        let args = vec!["kirv".to_string(), "abc".to_string(), "10".to_string()];
        assert!(matches!(
            parse_args(&args),
            Err(ControlError::InvalidPid(_))
        ));
    }

    #[test]
    fn parse_args_rejects_non_positive_pid() {
        let args = vec!["kirv".to_string(), "-1".to_string(), "10".to_string()];
        assert!(matches!(
            parse_args(&args),
            Err(ControlError::InvalidArguments("pid must be greater than 0"))
        ));
    }

    #[test]
    fn parse_args_rejects_out_of_range_percentage() {
        let args = vec!["kirv".to_string(), "123".to_string(), "100".to_string()];
        assert!(matches!(
            parse_args(&args),
            Err(ControlError::InvalidArguments(
                "percentage must be between 1 and 99"
            ))
        ));
    }

    #[test]
    fn parse_args_accepts_percentage_above_fifty() {
        let args = vec!["kirv".to_string(), "123".to_string(), "99".to_string()];

        let parsed = parse_args(&args).expect("percentage should be accepted");

        assert_eq!(parsed.throttle, 99.0);
    }

    #[test]
    fn parse_args_rejects_empty_percentage() {
        let args = vec!["kirv".to_string(), "123".to_string(), "".to_string()];
        assert!(matches!(
            parse_args(&args),
            Err(ControlError::InvalidThrottle(_))
        ));
    }

    #[test]
    fn missing_observation_uses_hold_instead_of_stopping_control() {
        let mut estimator = Estimator::new(Duration::from_millis(500));
        let mut marshal = Marshal::new(50.0, Duration::from_millis(500));
        let base = Instant::now();

        let first = RawObservation {
            timestamp: base,
            per_pid_cpu: vec![(1, 60.0)],
            process_count: 1,
        };
        let second = RawObservation {
            timestamp: base + Duration::from_millis(500),
            per_pid_cpu: vec![(1, 60.0)],
            process_count: 1,
        };

        let _ = compute_control_decision(&mut estimator, &mut marshal, Some(&first));
        let _ = compute_control_decision(&mut estimator, &mut marshal, Some(&second));
        let (estimated, decision) = compute_control_decision(&mut estimator, &mut marshal, None);

        assert!(estimated.valid);
        assert!(!estimated.timed_out);
        assert!(decision.stop_duration > Duration::ZERO);
    }

    #[test]
    fn stop_requested_reflects_stop_signal_state() {
        let _guard = GlobalStateGuard::acquire();
        assert!(!stop_requested());

        switch_stop_signal();
        assert!(stop_requested());
    }

    #[test]
    fn resume_tracked_members_skips_empty_stopped_group() {
        assert!(resume_tracked_members(ActiveTarget { stopped_pgid: None }).is_ok());
    }

    #[test]
    fn try_resume_active_target_clears_slot_on_success() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            stopped_pgid: Some(1),
        });

        assert!(try_resume_active_target_with(|_| Ok(())).is_ok());
        assert!(current_active_target().is_none());
    }

    #[test]
    fn try_resume_active_target_keeps_slot_on_failure() {
        let _guard = GlobalStateGuard::acquire();
        let active_target = ActiveTarget {
            stopped_pgid: Some(1),
        };
        set_active_target(active_target.clone());

        let result = try_resume_active_target_with(|_| Err(std::io::Error::other("resume failed")));

        assert!(result.is_err());
        assert_eq!(current_active_target(), Some(active_target));
    }
}
