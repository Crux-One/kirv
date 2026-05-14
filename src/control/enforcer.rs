use super::types::{ControlDecision, TargetGroup};
use nix::{
    errno::Errno,
    sys::signal::{kill, killpg, Signal},
    unistd::Pid,
};
use std::{io, thread};

pub struct Enforcer;

impl Enforcer {
    pub fn new() -> Self {
        Self
    }

    pub fn apply(&self, target: &TargetGroup, decision: &ControlDecision) -> io::Result<()> {
        if decision.stop_duration.is_zero() || super::stop_requested() {
            return Ok(());
        }

        super::set_stopped_group(target.pgid);
        if let Err(err) = self.stop_group(target.pgid) {
            super::clear_stopped_group();
            return Err(err);
        }
        super::wait::with_stop_check(decision.stop_duration, super::stop_requested, thread::sleep);
        self.resume_tracked_members(target.pgid)
    }

    pub fn resume_group(&self, pgid: i32) -> io::Result<()> {
        send_group_signal(pgid, Signal::SIGCONT)
    }

    pub fn resume_pids(&self, pids: &[i32]) -> io::Result<()> {
        for pid in pids {
            send_process_signal(*pid, Signal::SIGCONT)?;
        }
        Ok(())
    }

    fn stop_group(&self, pgid: i32) -> io::Result<()> {
        send_group_signal(pgid, Signal::SIGSTOP)
    }

    fn resume_tracked_members(&self, pgid: i32) -> io::Result<()> {
        match self.resume_group(pgid) {
            Ok(()) => {
                super::clear_stopped_group();
                Ok(())
            }
            Err(group_err) => {
                let pids = super::current_group_pids(pgid)?;
                match self.resume_pids(&pids) {
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
}
