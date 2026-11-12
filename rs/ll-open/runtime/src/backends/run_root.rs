//! The run root both tiers make each run's directory beneath (bead
//! `ley-line-open-dac894`): who owns a run directory, removing the ones a
//! gone daemon left, and the free space a run needs before its copy is
//! made.

use std::path::{Path, PathBuf};

use crate::ExecutionError;

/// The two run-directory kinds: the microVM tier's and the native tier's.
pub const RUN_DIR_BASES: [&str; 2] = ["leyline-run-", "leyline-native-run-"];

/// The free space, in bytes, a run root must have before a run's copy is
/// made beneath it: on a CAS volume, runs and the CAS share one size cap,
/// so a run that would leave less refuses at start instead of starving the
/// next import or run.
pub const RUN_ROOT_FREE_FLOOR_BYTES: u64 = 1 << 30;

/// The prefix of a run directory of kind `base` this process creates:
/// `base` followed by this daemon's pid, so a later daemon knows whose it
/// is.
pub(crate) fn run_dir_prefix(base: &str) -> String {
    format!("{base}{}-", std::process::id())
}

/// The daemon pid a run directory's name records, if it is one.
fn owner(name: &str) -> Option<libc::pid_t> {
    let rest = RUN_DIR_BASES
        .iter()
        .find_map(|base| name.strip_prefix(base))?;
    let (pid, _random) = rest.split_once('-')?;
    pid.parse::<std::num::NonZeroU32>()
        .ok()
        .and_then(|pid| libc::pid_t::try_from(pid.get()).ok())
}

/// Remove every run directory beneath `run_root` whose daemon is gone (a
/// crash, a kill, a reboot), owner permissions restored first so a tree the
/// guest chmod'd shut goes too; return what was removed. A live daemon's
/// directories, and anything not named as a run directory with an owner
/// (including the pid-less names earlier releases made), are left alone.
pub fn sweep_stale_runs(run_root: &Path) -> Result<Vec<PathBuf>, ExecutionError> {
    let read = std::fs::read_dir(run_root).map_err(|error| {
        ExecutionError::backend(format!("list run root {}: {error}", run_root.display()))
    })?;
    let mut removed = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| {
            ExecutionError::backend(format!("list run root {}: {error}", run_root.display()))
        })?;
        let Some(pid) = entry.file_name().to_str().and_then(owner) else {
            continue;
        };
        if crate::cas_volume::process_exists(pid) {
            continue;
        }
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        super::process::make_tree_removable(
            &path,
            super::process::SupervisionLabels {
                worker: "stale run",
                ephemeral: "stale run directory",
            },
        )?;
        std::fs::remove_dir_all(&path).map_err(|error| {
            ExecutionError::backend(format!(
                "remove stale run directory {}: {error}",
                path.display()
            ))
        })?;
        removed.push(path);
    }
    Ok(removed)
}

/// Refuse a run when `run_root`'s filesystem has less than `floor` bytes
/// available to this user, naming the run root and both figures.
pub(crate) fn ensure_free_space(run_root: &Path, floor: u64) -> Result<(), ExecutionError> {
    let stat = rustix::fs::statvfs(run_root).map_err(|errno| {
        ExecutionError::backend(format!(
            "inspect free space of run root {}: {errno}",
            run_root.display()
        ))
    })?;
    #[allow(clippy::unnecessary_cast)]
    let available = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    if available < floor {
        return Err(ExecutionError::resource_exhausted(format!(
            "run root {} has {available} bytes free, below the {floor} a run needs before its rootfs is copied",
            run_root.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn dead_pid() -> u32 {
        let mut child = Command::new("/usr/bin/true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        pid
    }

    #[test]
    fn a_gone_daemons_runs_are_swept_and_a_live_ones_kept() {
        let root = tempfile::tempdir().unwrap();
        let dead = dead_pid();
        let gone = [
            root.path().join(format!("leyline-run-{dead}-aB3x9Q")),
            root.path()
                .join(format!("leyline-native-run-{dead}-Zz01kk")),
        ];
        for dir in &gone {
            std::fs::create_dir_all(dir.join("rootfs/locked")).unwrap();
            std::fs::write(dir.join("rootfs/locked/file"), b"x").unwrap();
            // A guest chmod'd its tree shut before the daemon died.
            std::fs::set_permissions(
                dir.join("rootfs/locked"),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
        }
        let live = root
            .path()
            .join(format!("{}Q1w2e3", run_dir_prefix(RUN_DIR_BASES[0])));
        let kept = [
            live.clone(),
            // Earlier releases' names record no owner.
            root.path().join("leyline-run-aB3x9Q"),
            root.path().join("leyline-native-run-Zz01kk"),
            root.path().join("something-else"),
            root.path().join(format!("leyline-run-0-aaaaaa")),
        ];
        for dir in &kept {
            std::fs::create_dir(dir).unwrap();
        }
        let mut removed = sweep_stale_runs(root.path()).unwrap();
        removed.sort();
        let mut expected = gone.to_vec();
        expected.sort();
        assert_eq!(removed, expected);
        for dir in &gone {
            assert!(dir.symlink_metadata().is_err(), "{}", dir.display());
        }
        for dir in &kept {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        assert!(sweep_stale_runs(&root.path().join("missing")).is_err());
    }

    #[test]
    fn a_run_directory_name_records_this_daemon() {
        let prefix = run_dir_prefix("leyline-run-");
        assert_eq!(prefix, format!("leyline-run-{}-", std::process::id()));
        assert_eq!(
            owner(&format!("{prefix}abcdef")),
            libc::pid_t::try_from(std::process::id()).ok()
        );
        assert_eq!(owner("leyline-native-run-42-x"), Some(42));
        for name in [
            "leyline-run-x",
            "leyline-run-42",
            "leyline-run-0-x",
            "other-42-x",
        ] {
            assert_eq!(owner(name), None, "{name}");
        }
    }

    #[test]
    fn a_run_root_below_the_free_space_floor_is_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        ensure_free_space(root.path(), 0).unwrap();
        let error = ensure_free_space(root.path(), u64::MAX).unwrap_err();
        assert!(
            error.detail.contains(&root.path().display().to_string())
                && error.detail.contains("bytes free"),
            "{error:?}"
        );
        assert!(ensure_free_space(&root.path().join("missing"), 0).is_err());
    }
}
