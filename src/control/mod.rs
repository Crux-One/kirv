mod config;
mod enforcer;
mod estimator;
mod marshal;
mod recon;
mod reporter;
mod types;
mod wait;

use nix::unistd::getpgrp;
use std::{
    error::Error,
    fmt,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use config::ControlConfig;
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
pub enum ControlError {
    InvalidArguments(&'static str),
    InvalidPid(std::num::ParseIntError),
    InvalidThrottle(std::num::ParseFloatError),
    ActiveTargetAlreadySet,
    ResumeFailed(std::io::Error),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArguments(msg) => write!(f, "{msg}"),
            Self::InvalidPid(err) => write!(f, "invalid pid: {err}"),
            Self::InvalidThrottle(err) => write!(f, "invalid throttle percentage: {err}"),
            Self::ActiveTargetAlreadySet => write!(f, "control loop is already active"),
            Self::ResumeFailed(err) => write!(f, "failed to resume target process group: {err}"),
        }
    }
}

impl Error for ControlError {}

struct ControlSession {
    target: types::TargetGroup,
    throttle: f32,
    recon: Recon,
    estimator: Estimator,
    marshal: Marshal,
    enforcer: Enforcer,
    reporter: Reporter,
}

impl ControlSession {
    fn begin(config: ControlConfig) -> Result<Self, Box<dyn Error>> {
        let mut recon = Recon::new();
        let target = recon.resolve_target_group(config.pid)?;
        guard_target_group(target.pgid)?;
        try_set_active_target(ActiveTarget {
            pgid: target.pgid,
            stopped_pgid: None,
        })?;
        reset_stop_signal();

        Ok(Self {
            target,
            throttle: config.throttle,
            recon,
            estimator: Estimator::new(CONTROL_PERIOD),
            marshal: Marshal::new(config.throttle, CONTROL_PERIOD),
            enforcer: Enforcer::new(),
            reporter: Reporter::new(),
        })
    }

    fn run(&mut self) -> Result<(), Box<dyn Error>> {
        println!("target pid: {}", self.target.root.pid);
        println!("target pgid: {}", self.target.pgid);
        println!("throttle: {}% group CPU (ps/top-style)", self.throttle);

        loop {
            if STOP_SIGNAL.load(Ordering::SeqCst) {
                break;
            }

            let loop_start = std::time::Instant::now();
            if !self.recon.validate_target_group(&self.target)? {
                eprintln!(
                    "target pgid {} has no live processes; stopping control loop",
                    self.target.pgid
                );
                break;
            }
            let observation = self.recon.observe_group(&self.target)?;
            let (estimated, decision) = compute_control_decision(
                &mut self.estimator,
                &mut self.marshal,
                observation.as_ref(),
            );
            self.reporter
                .report(&self.target, observation.as_ref(), &estimated, &decision);
            self.enforcer.apply(&self.target, &decision)?;

            let elapsed = loop_start.elapsed();
            if elapsed < CONTROL_PERIOD {
                wait::with_stop_check(CONTROL_PERIOD - elapsed, stop_requested, thread::sleep);
            }
        }

        Ok(())
    }
}

impl Drop for ControlSession {
    fn drop(&mut self) {
        if let Err(err) = try_resume_active_target() {
            eprintln!("failed to resume stopped target members during shutdown: {err}");
        }
    }
}

#[cfg(test)]
fn control_session_for_test(target: types::TargetGroup) -> ControlSession {
    ControlSession {
        target,
        throttle: 50.0,
        recon: Recon::new(),
        estimator: Estimator::new(CONTROL_PERIOD),
        marshal: Marshal::new(50.0, CONTROL_PERIOD),
        enforcer: Enforcer::new(),
        reporter: Reporter::new(),
    }
}

pub fn start() -> Result<(), Box<dyn Error>> {
    start_with_config(ControlConfig::from_env()?)
}

fn start_with_config(config: ControlConfig) -> Result<(), Box<dyn Error>> {
    let mut session = ControlSession::begin(config)?;
    session.run()
}

pub fn stop() {
    switch_stop_signal();
}

pub fn resume_before_forced_exit() -> Result<(), ControlError> {
    resume_before_forced_exit_with(resume_tracked_members, resume_target_group)
}

fn resume_before_forced_exit_with<F, G>(
    resume_tracked: F,
    resume_target: G,
) -> Result<(), ControlError>
where
    F: FnOnce(ActiveTarget) -> std::io::Result<()>,
    G: FnOnce(i32) -> std::io::Result<()>,
{
    try_resume_active_target_before_forced_exit_with(resume_tracked, resume_target)
        .map_err(ControlError::ResumeFailed)
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

fn reset_stop_signal() {
    STOP_SIGNAL.store(false, Ordering::SeqCst);
}

fn stop_requested() -> bool {
    STOP_SIGNAL.load(Ordering::SeqCst)
}

#[cfg(test)]
fn set_active_target(active_target: ActiveTarget) {
    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    *slot = Some(active_target);
}

fn try_set_active_target(active_target: ActiveTarget) -> Result<(), ControlError> {
    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    if slot.is_some() {
        return Err(ControlError::ActiveTargetAlreadySet);
    }

    *slot = Some(active_target);
    Ok(())
}

fn try_set_stopped_group(pgid: i32) -> std::io::Result<()> {
    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    let Some(active_target) = slot.as_mut() else {
        return Err(std::io::Error::other(
            "active target must be set before stopping a group",
        ));
    };

    active_target.stopped_pgid = Some(pgid);
    Ok(())
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

fn resume_target_group(pgid: i32) -> std::io::Result<()> {
    Enforcer::new().resume_group(pgid)
}

fn try_resume_active_target_before_forced_exit_with<F, G>(
    resume_tracked: F,
    resume_target: G,
) -> std::io::Result<()>
where
    F: FnOnce(ActiveTarget) -> std::io::Result<()>,
    G: FnOnce(i32) -> std::io::Result<()>,
{
    let Some(active_target) = current_active_target() else {
        return Ok(());
    };

    let target_pgid = active_target.pgid;
    let resume_result = if active_target.stopped_pgid.is_some() {
        match resume_tracked(active_target.clone()) {
            Ok(()) => Ok(()),
            Err(tracked_err) => resume_target(target_pgid).map_err(|target_err| {
                std::io::Error::other(format!(
                    "failed to resume tracked target members: {tracked_err}; forced target group resume failed: {target_err}"
                ))
            }),
        }
    } else {
        resume_target(target_pgid)
    };

    resume_result?;

    let mut slot = ACTIVE_TARGET.lock().expect("active target mutex poisoned");
    if slot.as_ref() == Some(&active_target) {
        *slot = None;
    }

    Ok(())
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
    fn stop_sets_stop_signal() {
        let _guard = GlobalStateGuard::acquire();
        assert!(!stop_requested());

        stop();
        assert!(stop_requested());
    }

    #[test]
    fn resume_tracked_members_skips_empty_stopped_group() {
        assert!(
            resume_tracked_members(ActiveTarget {
                pgid: 1,
                stopped_pgid: None
            })
            .is_ok()
        );
    }

    #[test]
    fn try_resume_active_target_clears_slot_on_success() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            pgid: 1,
            stopped_pgid: Some(1),
        });

        assert!(try_resume_active_target_with(|_| Ok(())).is_ok());
        assert!(current_active_target().is_none());
    }

    #[test]
    fn try_resume_active_target_keeps_slot_on_failure() {
        let _guard = GlobalStateGuard::acquire();
        let active_target = ActiveTarget {
            pgid: 1,
            stopped_pgid: Some(1),
        };
        set_active_target(active_target.clone());

        let result = try_resume_active_target_with(|_| Err(std::io::Error::other("resume failed")));

        assert!(result.is_err());
        assert_eq!(current_active_target(), Some(active_target));
    }

    #[test]
    fn try_set_active_target_rejects_existing_active_target() {
        let _guard = GlobalStateGuard::acquire();
        let active_target = ActiveTarget {
            pgid: 1,
            stopped_pgid: Some(1),
        };
        set_active_target(active_target.clone());

        let result = try_set_active_target(ActiveTarget {
            pgid: 2,
            stopped_pgid: None,
        });

        assert!(matches!(result, Err(ControlError::ActiveTargetAlreadySet)));
        assert_eq!(current_active_target(), Some(active_target));
    }

    #[test]
    fn reset_stop_signal_allows_new_run_after_active_target_is_set() {
        let _guard = GlobalStateGuard::acquire();
        stop();
        assert!(stop_requested());

        try_set_active_target(ActiveTarget {
            pgid: 1,
            stopped_pgid: None,
        })
        .expect("active target should be set");
        reset_stop_signal();

        assert!(!stop_requested());
    }

    #[test]
    fn failed_reentry_does_not_clear_pending_stop_signal() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            pgid: 1,
            stopped_pgid: None,
        });
        stop();

        let result = try_set_active_target(ActiveTarget {
            pgid: 2,
            stopped_pgid: None,
        });

        assert!(matches!(result, Err(ControlError::ActiveTargetAlreadySet)));
        assert!(stop_requested());
    }

    #[test]
    fn resume_before_forced_exit_clears_active_target_on_success() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            pgid: 1,
            stopped_pgid: Some(1),
        });

        assert!(resume_before_forced_exit_with(|_| Ok(()), |_| Ok(())).is_ok());
        assert!(current_active_target().is_none());
    }

    #[test]
    fn resume_before_forced_exit_skips_target_resume_after_tracked_success() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            pgid: 42,
            stopped_pgid: Some(42),
        });
        let mut tracked_resume = None;
        let mut target_resume = None;

        assert!(
            resume_before_forced_exit_with(
                |active_target| {
                    tracked_resume = active_target.stopped_pgid;
                    Ok(())
                },
                |pgid| {
                    target_resume = Some(pgid);
                    Ok(())
                }
            )
            .is_ok()
        );

        assert_eq!(tracked_resume, Some(42));
        assert_eq!(target_resume, None);
        assert!(current_active_target().is_none());
    }

    #[test]
    fn resume_before_forced_exit_falls_back_to_target_after_tracked_failure() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            pgid: 42,
            stopped_pgid: Some(42),
        });
        let mut target_resume = None;

        assert!(
            resume_before_forced_exit_with(
                |_| Err(std::io::Error::other("tracked resume failed")),
                |pgid| {
                    target_resume = Some(pgid);
                    Ok(())
                }
            )
            .is_ok()
        );

        assert_eq!(target_resume, Some(42));
        assert!(current_active_target().is_none());
    }

    #[test]
    fn resume_before_forced_exit_resumes_target_group_even_without_stopped_group() {
        let _guard = GlobalStateGuard::acquire();
        set_active_target(ActiveTarget {
            pgid: 42,
            stopped_pgid: None,
        });
        let mut resumed_target = None;

        assert!(
            resume_before_forced_exit_with(
                |active_target| {
                    assert_eq!(active_target.stopped_pgid, None);
                    Ok(())
                },
                |pgid| {
                    resumed_target = Some(pgid);
                    Ok(())
                }
            )
            .is_ok()
        );

        assert_eq!(resumed_target, Some(42));
        assert!(current_active_target().is_none());
    }
}
