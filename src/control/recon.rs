use super::types::{ProcessIdentity, RawObservation, TargetGroup};
use darwin_libproc::pgrp_only_pids;
use nix::libc;
use std::{error::Error, io, mem, time::Instant};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

pub struct Recon {
    sys: System,
    cpu_primed: bool,
}

impl Recon {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve_target_group(&mut self, pid: i32) -> Result<TargetGroup, Box<dyn Error>> {
        let info = bsd_info(pid).map_err(|err| {
            if err.kind() == io::ErrorKind::PermissionDenied {
                Box::new(ProcessAccessError {
                    pid,
                    current_uid: current_uid(),
                }) as Box<dyn Error>
            } else {
                Box::new(err) as Box<dyn Error>
            }
        })?;
        ensure_process_owner(&info)?;
        let identity = identity_from_bsd_info(&info);
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
        let info = bsd_info(pid)?;
        Ok(identity_from_bsd_info(&info))
    }

    pub fn validate_target_group(&self, target: &TargetGroup) -> io::Result<bool> {
        let Some(identity) =
            normalize_process_identity_result(self.process_identity(target.root.pid))?
        else {
            return Ok(false);
        };

        if !same_process_identity(target.root, identity) {
            return Ok(false);
        }

        let Some(pids) = raw_group_pids(target.pgid)? else {
            return Ok(false);
        };

        Ok(group_contains_pid(&pids, target.root.pid) && group_has_live_members(&pids))
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

        if should_discard_cpu_sample(&mut self.cpu_primed) {
            return Ok(None);
        }

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
            cpu_primed: false,
        }
    }
}

#[derive(Debug)]
struct ProcessOwnerError {
    target_uid: u32,
    current_uid: u32,
}

impl std::fmt::Display for ProcessOwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "target process is owned by uid {}, current user is uid {}",
            self.target_uid, self.current_uid
        )
    }
}

impl Error for ProcessOwnerError {}

#[derive(Debug)]
struct ProcessAccessError {
    pid: i32,
    current_uid: u32,
}

impl std::fmt::Display for ProcessAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "target process {} is not readable by current user uid {}; it may be owned by another user",
            self.pid, self.current_uid
        )
    }
}

impl Error for ProcessAccessError {}

fn ensure_process_owner(info: &darwin_libproc::proc_bsdinfo) -> Result<(), ProcessOwnerError> {
    let target_uid = info.pbi_uid;
    let current_uid = current_uid();

    if target_uid != current_uid {
        return Err(ProcessOwnerError {
            target_uid,
            current_uid,
        });
    }

    Ok(())
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn bsd_info(pid: i32) -> io::Result<darwin_libproc::proc_bsdinfo> {
    let mut info = mem::MaybeUninit::<darwin_libproc::proc_bsdinfo>::uninit();
    let size = mem::size_of::<darwin_libproc::proc_bsdinfo>() as libc::c_int;

    let result = unsafe {
        darwin_libproc_sys::proc_pidinfo(
            pid,
            darwin_libproc_sys::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };

    normalize_bsd_info_result(result, size)?;

    unsafe { Ok(info.assume_init()) }
}

fn normalize_bsd_info_result(result: libc::c_int, expected_size: libc::c_int) -> io::Result<()> {
    match result {
        value if value < 0 => Err(io::Error::last_os_error()),
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "proc_pidinfo returned no BSD process info",
        )),
        value if value != expected_size => Err(io::Error::other("invalid value returned")),
        _ => Ok(()),
    }
}

fn group_has_live_members(pids: &[i32]) -> bool {
    pids.iter().any(|pid| *pid > 0)
}

fn group_contains_pid(pids: &[i32], target_pid: i32) -> bool {
    pids.contains(&target_pid)
}

fn same_process_identity(expected: ProcessIdentity, actual: ProcessIdentity) -> bool {
    expected == actual
}

fn should_discard_cpu_sample(cpu_primed: &mut bool) -> bool {
    if *cpu_primed {
        false
    } else {
        *cpu_primed = true;
        true
    }
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

fn normalize_process_identity_result(
    result: io::Result<ProcessIdentity>,
) -> io::Result<Option<ProcessIdentity>> {
    match result {
        Ok(identity) => Ok(Some(identity)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn identity_from_bsd_info(info: &darwin_libproc::proc_bsdinfo) -> ProcessIdentity {
    ProcessIdentity {
        pid: info.pbi_pid as i32,
        pgid: info.pbi_pgid as i32,
        start_tvsec: info.pbi_start_tvsec,
        start_tvusec: info.pbi_start_tvusec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn group_contains_pid_requires_target_pid() {
        assert!(group_contains_pid(&[1, 42, 99], 42));
        assert!(!group_contains_pid(&[1, 99], 42));
    }

    #[test]
    fn recon_starts_with_unprimed_cpu_samples() {
        let recon = Recon::default();

        assert!(!recon.cpu_primed);
    }

    #[test]
    fn discards_only_first_cpu_sample_after_refresh() {
        let mut cpu_primed = false;

        assert!(should_discard_cpu_sample(&mut cpu_primed));
        assert!(cpu_primed);
        assert!(!should_discard_cpu_sample(&mut cpu_primed));
    }

    #[test]
    fn not_found_group_is_treated_as_missing() {
        let result = normalize_group_pids_result(Err(io::Error::from(io::ErrorKind::NotFound)));

        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn not_found_process_identity_is_treated_as_missing() {
        let result =
            normalize_process_identity_result(Err(io::Error::from(io::ErrorKind::NotFound)));

        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn process_identity_errors_other_than_not_found_are_propagated() {
        let err = normalize_process_identity_result(Err(io::Error::from(
            io::ErrorKind::PermissionDenied,
        )))
        .expect_err("permission errors should propagate");

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn zero_byte_bsd_info_result_is_deterministic_not_found() {
        let err = normalize_bsd_info_result(0, 128)
            .expect_err("zero-byte result should be treated as missing process info");

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "proc_pidinfo returned no BSD process info");
    }

    #[test]
    fn short_bsd_info_result_is_rejected() {
        let err =
            normalize_bsd_info_result(64, 128).expect_err("short result should not be accepted");

        assert_eq!(err.to_string(), "invalid value returned");
    }

    #[test]
    fn missing_group_has_no_members() {
        let members = normalize_group_pids_result(Err(io::Error::from(io::ErrorKind::NotFound)))
            .unwrap()
            .unwrap_or_default();

        assert!(members.is_empty());
    }

    fn bsd_info_fixture(uid: u32) -> darwin_libproc::proc_bsdinfo {
        darwin_libproc::proc_bsdinfo {
            pbi_flags: 0,
            pbi_status: 0,
            pbi_xstatus: 0,
            pbi_pid: 42,
            pbi_ppid: 1,
            pbi_uid: uid,
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
        }
    }

    #[test]
    fn converts_bsd_info_to_process_identity() {
        let info = bsd_info_fixture(0);

        assert_eq!(
            identity_from_bsd_info(&info),
            ProcessIdentity {
                pid: 42,
                pgid: 77,
                start_tvsec: 123,
                start_tvusec: 456,
            }
        );
    }

    #[test]
    fn accepts_process_owned_by_current_user() {
        let mut info = bsd_info_fixture(current_uid());

        assert!(ensure_process_owner(&info).is_ok());

        info.pbi_uid = current_uid().saturating_add(1);
        let err = ensure_process_owner(&info).expect_err("owner mismatch should be rejected");

        assert_eq!(
            err.to_string(),
            format!(
                "target process is owned by uid {}, current user is uid {}",
                info.pbi_uid,
                current_uid()
            )
        );
    }

    #[test]
    fn process_access_error_explains_permission_denied_context() {
        let err = ProcessAccessError {
            pid: 42,
            current_uid: 501,
        };

        assert_eq!(
            err.to_string(),
            "target process 42 is not readable by current user uid 501; it may be owned by another user"
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
