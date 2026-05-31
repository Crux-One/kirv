use super::types::{ControlDecision, TargetGroup};
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use std::{io, thread, time::Duration};

#[derive(Default)]
pub struct Enforcer;

impl Enforcer {
    pub fn new() -> Self {
        Self
    }

    pub fn apply(&self, target: &TargetGroup, decision: &ControlDecision) -> io::Result<()> {
        self.apply_with(target, decision, &mut SystemSignalSender, thread::sleep)
    }

    fn apply_with<S, Sleep>(
        &self,
        target: &TargetGroup,
        decision: &ControlDecision,
        sender: &mut S,
        sleep: Sleep,
    ) -> io::Result<()>
    where
        S: SignalSender,
        Sleep: FnMut(Duration),
    {
        if decision.stop_duration.is_zero() || super::stop_requested() {
            return Ok(());
        }

        super::try_set_stopped_group(target.pgid)?;
        if let Err(err) = self.stop_group_with(target.pgid, sender) {
            super::clear_stopped_group();
            return Err(err);
        }
        super::wait::with_stop_check(decision.stop_duration, super::stop_requested, sleep);
        self.resume_tracked_members_with(target.pgid, sender)
    }

    pub fn resume_group(&self, pgid: i32) -> io::Result<()> {
        let mut sender = SystemSignalSender;
        sender.send_group(pgid, Signal::SIGCONT)
    }

    pub fn resume_pids(&self, pids: &[i32]) -> io::Result<()> {
        let mut sender = SystemSignalSender;
        for pid in pids {
            sender.send_process(*pid, Signal::SIGCONT)?;
        }
        Ok(())
    }

    fn stop_group_with<S: SignalSender>(&self, pgid: i32, sender: &mut S) -> io::Result<()> {
        sender.send_group(pgid, Signal::SIGSTOP)
    }

    fn resume_tracked_members_with<S: SignalSender>(
        &self,
        pgid: i32,
        sender: &mut S,
    ) -> io::Result<()> {
        match sender.send_group(pgid, Signal::SIGCONT) {
            Ok(()) => {
                super::clear_stopped_group();
                Ok(())
            }
            Err(group_err) => {
                let pids = super::current_group_pids(pgid)?;
                match resume_pids_with(&pids, sender) {
                    Ok(()) => {
                        super::clear_stopped_group();
                        Ok(())
                    }
                    Err(pid_err) => Err(combine_resume_errors(group_err, pid_err)),
                }
            }
        }
    }
}

trait SignalSender {
    fn send_group(&mut self, pgid: i32, signal: Signal) -> io::Result<()>;

    fn send_process(&mut self, pid: i32, signal: Signal) -> io::Result<()>;
}

struct SystemSignalSender;

impl SignalSender for SystemSignalSender {
    fn send_group(&mut self, pgid: i32, signal: Signal) -> io::Result<()> {
        send_group_signal(pgid, signal)
    }

    fn send_process(&mut self, pid: i32, signal: Signal) -> io::Result<()> {
        send_process_signal(pid, signal)
    }
}

fn resume_pids_with<S: SignalSender>(pids: &[i32], sender: &mut S) -> io::Result<()> {
    for pid in pids {
        sender.send_process(*pid, Signal::SIGCONT)?;
    }
    Ok(())
}

fn combine_resume_errors(group_err: io::Error, pid_err: io::Error) -> io::Error {
    io::Error::other(format!(
        "failed to resume target group: {group_err}; fallback resume by pid failed: {pid_err}"
    ))
}

fn send_group_signal(pgid: i32, signal: Signal) -> io::Result<()> {
    match killpg(Pid::from_raw(pgid), signal) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(io::Error::other(err)),
    }
}

fn send_process_signal(pid: i32, signal: Signal) -> io::Result<()> {
    match kill(Pid::from_raw(pid), signal) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(io::Error::other(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;
    use std::sync::{MutexGuard, atomic::Ordering};

    use super::super::types::{ActiveTarget, ProcessIdentity};

    struct GlobalStateGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl GlobalStateGuard {
        fn acquire() -> Self {
            let lock = super::super::TEST_LOCK.lock().expect("test lock poisoned");
            reset_control_globals();
            Self { _lock: lock }
        }
    }

    impl Drop for GlobalStateGuard {
        fn drop(&mut self) {
            reset_control_globals();
        }
    }

    #[derive(Default)]
    struct FakeSignalSender {
        group_signals: Vec<(i32, Signal)>,
        process_signals: Vec<(i32, Signal)>,
    }

    impl SignalSender for FakeSignalSender {
        fn send_group(&mut self, pgid: i32, signal: Signal) -> io::Result<()> {
            self.group_signals.push((pgid, signal));
            Ok(())
        }

        fn send_process(&mut self, pid: i32, signal: Signal) -> io::Result<()> {
            self.process_signals.push((pid, signal));
            Ok(())
        }
    }

    fn reset_control_globals() {
        super::super::STOP_SIGNAL.store(false, Ordering::SeqCst);
        let mut slot = super::super::ACTIVE_TARGET
            .lock()
            .expect("active target mutex poisoned");
        *slot = None;
    }

    fn target_group(pgid: i32) -> TargetGroup {
        TargetGroup {
            root: ProcessIdentity {
                pid: 42,
                pgid,
                start_tvsec: 1,
                start_tvusec: 2,
            },
            pgid,
        }
    }

    #[test]
    fn send_process_signal_ignores_missing_process() {
        assert!(send_process_signal(i32::MAX, Signal::SIGCONT).is_ok());
    }

    #[test]
    fn combine_resume_errors_mentions_group_and_pid_failures() {
        let err =
            combine_resume_errors(io::Error::other(Errno::EPERM), io::Error::other(Errno::EIO));

        let message = err.to_string();
        assert!(message.contains("failed to resume target group"));
        assert!(message.contains("EPERM"));
        assert!(message.contains("fallback resume by pid failed"));
        assert!(message.contains("EIO"));
    }

    #[test]
    fn apply_resumes_stopped_group_and_session_clears_active_target_after_stop_request() {
        let _guard = GlobalStateGuard::acquire();
        let target = target_group(1234);
        let decision = ControlDecision {
            stop_duration: Duration::from_millis(25),
        };
        let mut sender = FakeSignalSender::default();
        super::super::set_active_target(ActiveTarget {
            pgid: target.pgid,
            stopped_pgid: None,
        });
        let control_session = super::super::control_session_for_test(target);

        Enforcer::new()
            .apply_with(&target, &decision, &mut sender, |_| {
                super::super::switch_stop_signal();
            })
            .expect("apply should resume the stopped group");

        assert_eq!(
            sender.group_signals,
            vec![
                (target.pgid, Signal::SIGSTOP),
                (target.pgid, Signal::SIGCONT)
            ]
        );
        assert!(sender.process_signals.is_empty());
        assert_eq!(
            super::super::current_active_target(),
            Some(ActiveTarget {
                pgid: target.pgid,
                stopped_pgid: None
            })
        );

        drop(control_session);

        assert!(super::super::current_active_target().is_none());
    }

    #[test]
    fn apply_fails_without_active_target_before_sending_stop() {
        let _guard = GlobalStateGuard::acquire();
        let target = target_group(1234);
        let decision = ControlDecision {
            stop_duration: Duration::from_millis(25),
        };
        let mut sender = FakeSignalSender::default();

        let result = Enforcer::new().apply_with(&target, &decision, &mut sender, |_| {});

        assert!(result.is_err());
        assert!(sender.group_signals.is_empty());
        assert!(sender.process_signals.is_empty());
        assert!(super::super::current_active_target().is_none());
    }
}
