use super::types::{ProcessIdentity, RawObservation, TargetGroup};
use darwin_libproc::{pgrp_only_pids, task_all_info};
use std::{error::Error, io, time::Instant};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

pub struct Recon {
    sys: System,
}

impl Recon {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve_target_group(&mut self, pid: i32) -> Result<TargetGroup, Box<dyn Error>> {
        let identity = self.process_identity(pid)?;
        let pgid = identity.pgid;

        if pgid <= 0 {
            return Err("invalid process group".into());
        }

        Ok(TargetGroup {
            root: identity,
            pgid,
        })
    }

    pub fn process_identity(&self, pid: i32) -> io::Result<ProcessIdentity> {
        let info = task_all_info(pid)?;
        Ok(identity_from_task_info(&info))
    }

    pub fn validate_target_group(&self, target: &TargetGroup) -> io::Result<bool> {
        let Ok(identity) = self.process_identity(target.root.pid) else {
            return Ok(false);
        };

        if !same_process_identity(target.root, identity) {
            return Ok(false);
        }

        Ok(raw_group_pids(target.pgid)?.is_some_and(|pids| group_has_live_members(&pids)))
    }

    pub fn observe_group(&mut self, target: &TargetGroup) -> io::Result<Option<RawObservation>> {
        let Some(pids) = raw_group_pids(target.pgid)? else {
            return Ok(None);
        };
        if pids.is_empty() {
            return Ok(None);
        }

        let sys_pids: Vec<Pid> = pids
            .iter()
            .copied()
            .filter(|pid| *pid > 0)
            .map(|pid| Pid::from(pid as usize))
            .collect();

        if sys_pids.is_empty() {
            return Ok(None);
        }

        self.sys
            .refresh_pids_specifics(&sys_pids, ProcessRefreshKind::new().with_cpu());

        let per_pid_cpu = pids
            .into_iter()
            .filter(|pid| *pid > 0)
            .filter_map(|pid| {
                let process = self.sys.process(Pid::from(pid as usize))?;
                Some((pid, process.cpu_usage()))
            })
            .collect::<Vec<_>>();

        if per_pid_cpu.is_empty() {
            return Ok(None);
        }

        Ok(Some(RawObservation {
            timestamp: Instant::now(),
            process_count: per_pid_cpu.len(),
            per_pid_cpu,
        }))
    }
}

impl Default for Recon {
    fn default() -> Self {
        Self {
            sys: System::new_with_specifics(RefreshKind::new().without_memory().without_cpu()),
        }
    }
}

fn group_has_live_members(pids: &[i32]) -> bool {
    pids.iter().any(|pid| *pid > 0)
}

fn same_process_identity(expected: ProcessIdentity, actual: ProcessIdentity) -> bool {
    expected == actual
}

fn raw_group_pids(pgid: i32) -> io::Result<Option<Vec<i32>>> {
    normalize_group_pids_result(pgrp_only_pids(pgid))
}

pub(super) fn group_pids(pgid: i32) -> io::Result<Vec<i32>> {
    Ok(raw_group_pids(pgid)?
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| *pid > 0)
        .collect())
}

fn normalize_group_pids_result(result: io::Result<Vec<i32>>) -> io::Result<Option<Vec<i32>>> {
    match result {
        Ok(pids) => Ok(Some(pids)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn identity_from_task_info(info: &darwin_libproc::proc_taskallinfo) -> ProcessIdentity {
    ProcessIdentity {
        pid: info.pbsd.pbi_pid as i32,
        pgid: info.pbsd.pbi_pgid as i32,
        start_tvsec: info.pbsd.pbi_start_tvsec,
        start_tvusec: info.pbsd.pbi_start_tvusec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::libc;

    #[test]
    fn group_with_positive_pid_is_treated_as_alive() {
        assert!(group_has_live_members(&[0, -1, 123]));
    }

    #[test]
    fn group_without_positive_pids_is_treated_as_dead() {
        assert!(!group_has_live_members(&[]));
        assert!(!group_has_live_members(&[0, -1]));
    }

    #[test]
    fn not_found_group_is_treated_as_missing() {
        let result = normalize_group_pids_result(Err(io::Error::from(io::ErrorKind::NotFound)));

        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn missing_group_has_no_members() {
        let members = normalize_group_pids_result(Err(io::Error::from(io::ErrorKind::NotFound)))
            .unwrap()
            .unwrap_or_default();

        assert!(members.is_empty());
    }

    #[test]
    fn converts_task_info_to_process_identity() {
        let info = darwin_libproc::proc_taskallinfo {
            pbsd: darwin_libproc::proc_bsdinfo {
                pbi_flags: 0,
                pbi_status: 0,
                pbi_xstatus: 0,
                pbi_pid: 42,
                pbi_ppid: 1,
                pbi_uid: 0,
                pbi_gid: 0,
                pbi_ruid: 0,
                pbi_rgid: 0,
                pbi_svuid: 0,
                pbi_svgid: 0,
                rfu_1: 0,
                pbi_comm: [0; libc::MAXCOMLEN],
                pbi_name: [0; 2 * libc::MAXCOMLEN],
                pbi_nfiles: 0,
                pbi_pgid: 77,
                pbi_pjobc: 0,
                e_tdev: 0,
                e_tpgid: 0,
                pbi_nice: 0,
                pbi_start_tvsec: 123,
                pbi_start_tvusec: 456,
            },
            ptinfo: darwin_libproc::proc_taskinfo {
                pti_virtual_size: 0,
                pti_resident_size: 0,
                pti_total_user: 0,
                pti_total_system: 0,
                pti_threads_user: 0,
                pti_threads_system: 0,
                pti_policy: 0,
                pti_faults: 0,
                pti_pageins: 0,
                pti_cow_faults: 0,
                pti_messages_sent: 0,
                pti_messages_received: 0,
                pti_syscalls_mach: 0,
                pti_syscalls_unix: 0,
                pti_csw: 0,
                pti_threadnum: 0,
                pti_numrunning: 0,
                pti_priority: 0,
            },
        };

        assert_eq!(
            identity_from_task_info(&info),
            ProcessIdentity {
                pid: 42,
                pgid: 77,
                start_tvsec: 123,
                start_tvusec: 456,
            }
        );
    }

    #[test]
    fn matching_identity_is_treated_as_same_process() {
        let target = ProcessIdentity {
            pid: 42,
            pgid: 77,
            start_tvsec: 123,
            start_tvusec: 456,
        };

        assert!(same_process_identity(target, target));
    }

    #[test]
    fn reused_pid_with_different_start_time_is_treated_as_different_process() {
        let original = ProcessIdentity {
            pid: 42,
            pgid: 77,
            start_tvsec: 123,
            start_tvusec: 456,
        };
        let reused = ProcessIdentity {
            start_tvsec: 999,
            ..original
        };

        assert!(!same_process_identity(original, reused));
    }
}
