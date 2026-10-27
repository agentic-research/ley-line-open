//! cgroup v2 ceilings for the native tier (ADR-0035, "OCI resources on
//! execution/v1").
//!
//! The operator delegates one cgroup v2 directory, the *root*; the native
//! backend enforces exactly the controllers that root's
//! `cgroup.subtree_control` enables, and refuses the rest by name. The daemon
//! never discovers a root or moves itself between cgroups: where the boundary
//! is, and who may write it, is an operator decision.
//!
//! Per run the backend creates one child of the root, writes the requested
//! ceilings into it, and places the worker in it before the worker has read
//! its request, so the worker and everything it spawns are inside the
//! cgroup before any of them executes the workload.
//!
//! The file protocol is plain file I/O on a directory, so everything except
//! [`CgroupRoot::open`]'s filesystem check runs on any platform. Only Linux
//! has cgroupfs, and `open` refuses a root that is not one.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{CeilingMechanism, EnforcedCeilings, ExecutionError, ResourceLimits};

/// `CGROUP2_SUPER_MAGIC` from `linux/magic.h`.
pub(super) const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

// The one platform-specific step, the filesystem's magic, lives in a file of
// its own per platform. cargo-mutants does not evaluate `cfg`, so a gated item
// in this file would be enumerated on the other platform, build unchanged, and
// read as a surviving mutant; a file can be excluded from the slice of the
// platform that does not compile it (tools/mutants_diff.sh).
#[cfg(target_os = "linux")]
#[path = "cgroup_fs_linux.rs"]
mod platform;
#[cfg(not(target_os = "linux"))]
#[path = "cgroup_fs_other.rs"]
mod platform;
use platform::filesystem_magic;

/// How long teardown waits for `cgroup.kill` to empty the run's cgroup.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// An operator-delegated cgroup v2 directory, and which of the controllers
/// LLO uses it delegates to its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupRoot {
    path: PathBuf,
    memory: bool,
    cpu: bool,
    pids: bool,
    /// True only for the unit tests' plain-directory stand-in, which has no
    /// kernel to create control files or to let `rmdir` pass them.
    stand_in: bool,
}

impl CgroupRoot {
    /// Open a delegated cgroup v2 directory.
    ///
    /// Refused when the path is not on a cgroup2 filesystem (and on every
    /// platform but Linux, which has none), or when its
    /// `cgroup.subtree_control` cannot be read.
    pub fn open(path: &Path) -> Result<Self, ExecutionError> {
        Self::on_filesystem(path, filesystem_magic(path)?)
    }

    /// The decision `open` makes once it knows the filesystem's magic,
    /// separated from the platform's `statfs` so it is the same code, and the
    /// same tests, on every platform.
    fn on_filesystem(path: &Path, magic: u64) -> Result<Self, ExecutionError> {
        if magic != CGROUP2_SUPER_MAGIC {
            return Err(ExecutionError::backend(format!(
                "cgroup root {} is not on a cgroup v2 filesystem",
                path.display()
            )));
        }
        Self::inspect(path, false)
    }

    /// Read which controllers the directory delegates, without checking the
    /// filesystem. `open` is the public constructor; this is the file
    /// protocol it shares with the unit tests, which pass `stand_in`.
    fn inspect(path: &Path, stand_in: bool) -> Result<Self, ExecutionError> {
        let controllers =
            fs::read_to_string(path.join("cgroup.subtree_control")).map_err(|error| {
                ExecutionError::backend(format!(
                    "cgroup root {}: read cgroup.subtree_control: {error}",
                    path.display()
                ))
            })?;
        let enabled = |name: &str| controllers.split_whitespace().any(|c| c == name);
        Ok(Self {
            path: path.to_path_buf(),
            memory: enabled("memory"),
            cpu: enabled("cpu"),
            pids: enabled("pids"),
            stand_in,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The ceilings the native tier enforces with this root: the wall clock
    /// as always, and each of memory, CPU bandwidth and pids exactly when the
    /// root delegates its controller.
    pub fn enforced(&self) -> EnforcedCeilings {
        let mechanism = |enabled: bool| {
            if enabled {
                CeilingMechanism::Cgroup
            } else {
                CeilingMechanism::Unenforced
            }
        };
        EnforcedCeilings {
            wall_time: CeilingMechanism::Supervisor,
            output_bytes: CeilingMechanism::Unenforced,
            memory: mechanism(self.memory),
            cpu: mechanism(self.cpu),
            pids: mechanism(self.pids),
        }
    }

    /// Create the run's cgroup and write every ceiling `limits` requests.
    ///
    /// The caller has already refused, through [`EnforcedCeilings::check`], any
    /// ceiling this root cannot enforce, so a requested ceiling whose
    /// controller is missing here is an internal error rather than a silent
    /// skip.
    pub fn create_run(
        &self,
        run_id: &str,
        limits: &ResourceLimits,
    ) -> Result<RunCgroup, ExecutionError> {
        self.enforced().check(limits)?;
        let name = run_cgroup_name(run_id)?;
        let path = self.path.join(name);
        fs::create_dir(&path).map_err(|error| {
            ExecutionError::backend(format!("create run cgroup {}: {error}", path.display()))
        })?;
        // From here the directory exists, so every failure must remove it.
        let run = RunCgroup {
            path,
            removed: false,
            stand_in: self.stand_in,
            kill_timeout: KILL_TIMEOUT,
        };
        let ResourceLimits {
            wall_time_ms: _,
            output_bytes: _,
            memory,
            cpu,
            pids,
        } = *limits;
        if memory.limit != 0 {
            run.write("memory.max", &memory.limit.to_string())?;
            // `memory.max` alone leaves swap unbounded, so a workload could
            // exceed the ceiling by swapping; and without `oom.group` an OOM
            // kills one task and leaves the rest of the tree running. Both
            // files are absent on kernels without swap accounting or before
            // 4.19, where there is nothing further to tighten.
            run.write_if_present("memory.swap.max", "0")?;
            run.write_if_present("memory.oom.group", "1")?;
        }
        if let Some(line) = cpu.cgroup_max()? {
            run.write("cpu.max", &line)?;
        }
        if pids.limit != 0 {
            run.write("pids.max", &pids.limit.to_string())?;
        }
        Ok(run)
    }
}

/// One run's cgroup: a child of the delegated root, removed on drop.
#[derive(Debug)]
pub struct RunCgroup {
    path: PathBuf,
    removed: bool,
    stand_in: bool,
    /// How long teardown waits for the cgroup to empty; [`KILL_TIMEOUT`]
    /// except in the unit tests, which shorten it.
    kill_timeout: Duration,
}

impl RunCgroup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Move `pid` (with all its threads) into this cgroup.
    ///
    /// The kernel refuses unless the writer may write `cgroup.procs` in the
    /// common ancestor of the process's current cgroup and this one; that
    /// refusal fails the start rather than running the workload unplaced.
    pub fn place(&self, pid: u32) -> Result<(), ExecutionError> {
        self.write("cgroup.procs", &pid.to_string())
    }

    /// Kill everything in the cgroup, wait until it is empty, and remove it.
    pub fn remove(mut self) -> Result<(), ExecutionError> {
        self.removed = true;
        remove_run_cgroup(&self.path, self.stand_in, self.kill_timeout)
    }

    fn write(&self, file: &str, value: &str) -> Result<(), ExecutionError> {
        let path = self.path.join(file);
        // cgroupfs creates every control file with the directory, so a
        // missing one is an error, never something to create: creating it
        // would fake a ceiling the kernel does not apply. Only the stand-in
        // directory, which has no kernel behind it, creates.
        let result = fs::OpenOptions::new()
            .write(true)
            .create(self.stand_in)
            .truncate(self.stand_in)
            .open(&path)
            .and_then(|mut handle| handle.write_all(value.as_bytes()));
        result.map_err(|error| {
            ExecutionError::backend(format!("write {value:?} to {}: {error}", path.display()))
        })
    }

    fn write_if_present(&self, file: &str, value: &str) -> Result<(), ExecutionError> {
        if self.path.join(file).exists() {
            self.write(file, value)
        } else {
            Ok(())
        }
    }
}

impl Drop for RunCgroup {
    fn drop(&mut self) {
        if !self.removed {
            let _ = remove_run_cgroup(&self.path, self.stand_in, self.kill_timeout);
        }
    }
}

fn remove_run_cgroup(
    path: &Path,
    stand_in: bool,
    kill_timeout: Duration,
) -> Result<(), ExecutionError> {
    if !path.exists() {
        return Ok(());
    }
    // `cgroup.kill` (5.14+) SIGKILLs every process in the subtree, including
    // one that left the worker's process group, which the group kill misses.
    let kill = path.join("cgroup.kill");
    if kill.exists() {
        fs::write(&kill, "1").map_err(|error| {
            ExecutionError::backend(format!("write {}: {error}", kill.display()))
        })?;
    }
    // A cgroup can be removed only once it holds no process. `cgroup.events`
    // reports `populated 0` when the kill has finished.
    let events = path.join("cgroup.events");
    let deadline = Instant::now() + kill_timeout;
    while events.exists() {
        let populated = fs::read_to_string(&events)
            .map(|text| text.lines().any(|line| line == "populated 1"))
            .unwrap_or(false);
        if !populated {
            break;
        }
        if Instant::now() >= deadline {
            return Err(ExecutionError::backend(format!(
                "run cgroup {} still holds processes after cgroup.kill",
                path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // cgroupfs removes a cgroup with `rmdir` while its control files are
    // still listed: they are not removable entries. The stand-in's files are.
    let removed = if stand_in {
        fs::remove_dir_all(path)
    } else {
        fs::remove_dir(path)
    };
    removed.map_err(|error| {
        ExecutionError::backend(format!("remove run cgroup {}: {error}", path.display()))
    })
}

/// A run id as a single cgroup directory name.
///
/// Run ids are derived (`run-<blake3 hex>`) for authorized runs, but the
/// backend also accepts a direct `ExecutionRequest`, so the name is checked
/// rather than trusted: one path component, no separator, no dot entry.
fn run_cgroup_name(run_id: &str) -> Result<String, ExecutionError> {
    let valid = !run_id.is_empty()
        && run_id != "."
        && run_id != ".."
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(ExecutionError::invalid(format!(
            "run_id {run_id:?} cannot name a cgroup directory"
        )));
    }
    Ok(format!("leyline-{run_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CpuLimits, MemoryLimits, PidsLimits};

    fn root(controllers: &str) -> (tempfile::TempDir, CgroupRoot) {
        let dir = tempfile::tempdir().expect("fake cgroup root");
        fs::write(dir.path().join("cgroup.subtree_control"), controllers).expect("controllers");
        let root = CgroupRoot::inspect(dir.path(), true).expect("inspect");
        (dir, root)
    }

    #[test]
    fn each_dimension_is_enforced_exactly_when_its_controller_is_delegated() {
        let (_dir, all) = root("cpuset cpu io memory pids\n");
        assert_eq!(
            all.enforced(),
            EnforcedCeilings {
                wall_time: CeilingMechanism::Supervisor,
                output_bytes: CeilingMechanism::Unenforced,
                memory: CeilingMechanism::Cgroup,
                cpu: CeilingMechanism::Cgroup,
                pids: CeilingMechanism::Cgroup,
            }
        );
        let (_dir, memory_only) = root("memory\n");
        let enforced = memory_only.enforced();
        assert_eq!(enforced.memory, CeilingMechanism::Cgroup);
        assert_eq!(enforced.cpu, CeilingMechanism::Unenforced);
        assert_eq!(enforced.pids, CeilingMechanism::Unenforced);
        // `cpuset` is not `cpu`: a substring match would enforce nothing.
        let (_dir, cpuset) = root("cpuset\n");
        assert_eq!(cpuset.enforced().cpu, CeilingMechanism::Unenforced);
    }

    #[test]
    fn the_cgroup_quota_floor_is_inclusive() {
        // cgroup v2 accepts exactly 1000 µs; one less is refused.
        let at = CpuLimits {
            quota: 1_000,
            period: 100_000,
        };
        assert_eq!(
            at.cgroup_max().expect("1000 is the minimum"),
            Some("1000 100000".into())
        );
        let below = CpuLimits {
            quota: 999,
            period: 100_000,
        };
        assert!(below.cgroup_max().is_err());
        // The period bounds are inclusive at both ends.
        for period in [1_000, 1_000_000] {
            assert!(
                CpuLimits {
                    quota: 1_000,
                    period
                }
                .cgroup_max()
                .is_ok(),
                "{period}"
            );
        }
    }

    #[test]
    fn a_run_cgroup_carries_each_requested_ceiling_in_its_kernel_file() {
        let (_dir, root) = root("cpu memory pids\n");
        let limits = ResourceLimits {
            wall_time_ms: 1_000,
            memory: MemoryLimits { limit: 256 << 20 },
            cpu: CpuLimits {
                quota: 150_000,
                period: 0,
            },
            pids: PidsLimits { limit: 64 },
            ..ResourceLimits::default()
        };
        let run = root
            .create_run("run-abc", &limits)
            .expect("create run cgroup");
        let read = |file: &str| fs::read_to_string(run.path().join(file)).expect(file);
        assert_eq!(read("memory.max"), (256u64 << 20).to_string());
        assert_eq!(read("cpu.max"), "150000 100000");
        assert_eq!(read("pids.max"), "64");
        let path = run.path().to_path_buf();
        run.remove().expect("remove");
        assert!(!path.exists());
    }

    #[test]
    fn an_unrequested_dimension_writes_nothing() {
        let (_dir, root) = root("cpu memory pids\n");
        let limits = ResourceLimits {
            wall_time_ms: 1_000,
            pids: PidsLimits { limit: 8 },
            ..ResourceLimits::default()
        };
        let run = root.create_run("run-pids", &limits).expect("create");
        assert!(!run.path().join("memory.max").exists());
        assert!(!run.path().join("cpu.max").exists());
        assert!(run.path().join("pids.max").exists());
    }

    #[test]
    fn a_ceiling_whose_controller_is_not_delegated_is_refused_by_name() {
        let (dir, root) = root("memory\n");
        let limits = ResourceLimits {
            wall_time_ms: 1_000,
            pids: PidsLimits { limit: 8 },
            ..ResourceLimits::default()
        };
        let error = root
            .create_run("run-x", &limits)
            .expect_err("pids not delegated");
        assert_eq!(error.code, crate::ErrorCode::UnsupportedBackend);
        assert!(error.detail.contains("pids.limit"), "{}", error.detail);
        // Refused before anything was created.
        assert!(!dir.path().join("leyline-run-x").exists());
    }

    #[test]
    fn a_run_id_that_is_not_one_path_component_names_no_cgroup() {
        let (_dir, root) = root("memory\n");
        for run_id in ["", ".", "..", "a/b", "../escape", "x y"] {
            assert!(
                root.create_run(run_id, &ResourceLimits::default()).is_err(),
                "{run_id:?} must be refused"
            );
        }
    }

    /// A stand-in run cgroup the test lays out by hand, with a short kill
    /// timeout so the waiting paths are observable in milliseconds.
    fn stand_in_run(dir: &Path) -> RunCgroup {
        fs::create_dir_all(dir).expect("run dir");
        RunCgroup {
            path: dir.to_path_buf(),
            removed: false,
            stand_in: true,
            kill_timeout: Duration::from_millis(300),
        }
    }

    #[test]
    fn placing_a_process_writes_its_pid_to_cgroup_procs() {
        let dir = tempfile::tempdir().expect("dir");
        let run = stand_in_run(&dir.path().join("run"));
        run.place(4242).expect("place");
        assert_eq!(
            fs::read_to_string(run.path().join("cgroup.procs")).expect("procs"),
            "4242"
        );
    }

    #[test]
    fn an_optional_control_file_is_written_when_present_and_never_created() {
        let dir = tempfile::tempdir().expect("dir");
        let run = stand_in_run(&dir.path().join("run"));
        fs::write(run.path().join("memory.swap.max"), "max").expect("present file");
        run.write_if_present("memory.swap.max", "0").expect("write");
        assert_eq!(
            fs::read_to_string(run.path().join("memory.swap.max")).expect("read"),
            "0"
        );
        run.write_if_present("memory.oom.group", "1")
            .expect("absent is fine");
        assert!(!run.path().join("memory.oom.group").exists());
    }

    #[test]
    fn teardown_kills_then_removes_a_cgroup_that_reports_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let run = stand_in_run(&dir.path().join("run"));
        fs::write(run.path().join("cgroup.kill"), "").expect("kill file");
        fs::write(run.path().join("cgroup.events"), "populated 0\nfrozen 0\n").expect("events");
        let path = run.path().to_path_buf();
        let started = Instant::now();
        run.remove().expect("an empty cgroup is removed");
        assert!(!path.exists());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "an already-empty cgroup is removed without waiting out the timeout"
        );
    }

    /// Teardown gives the kill its whole timeout before reporting a cgroup
    /// that is still populated: it neither gives up at the first poll nor
    /// polls forever. (That a cgroup emptying mid-wait is then removed is
    /// observed against a real kernel, `tests/native_cgroup.rs`.)
    #[test]
    fn teardown_reports_a_cgroup_that_never_empties_after_its_timeout() {
        let dir = tempfile::tempdir().expect("dir");
        let run = stand_in_run(&dir.path().join("run"));
        let timeout = run.kill_timeout;
        fs::write(run.path().join("cgroup.events"), "populated 1\n").expect("events");
        let started = Instant::now();
        let error = run.remove().expect_err("still populated");
        assert!(
            started.elapsed() >= timeout,
            "gave up after {:?}, before the {timeout:?} timeout",
            started.elapsed()
        );
        assert!(
            error.detail.contains("still holds processes"),
            "{}",
            error.detail
        );
    }

    #[test]
    fn teardown_reports_a_kill_the_kernel_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let run = stand_in_run(&dir.path().join("run"));
        // A directory where the control file should be: the write fails.
        fs::create_dir(run.path().join("cgroup.kill")).expect("unwritable kill");
        let error = run.remove().expect_err("the kill write failed");
        assert!(error.detail.contains("cgroup.kill"), "{}", error.detail);
    }

    #[test]
    fn dropping_a_run_cgroup_removes_it() {
        let (_dir, root) = root("memory\n");
        let run = root
            .create_run("run-drop", &ResourceLimits::default())
            .expect("create");
        let path = run.path().to_path_buf();
        drop(run);
        assert!(!path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_that_is_not_cgroup2_is_not_a_root() {
        let dir = tempfile::tempdir().expect("plain dir");
        fs::write(dir.path().join("cgroup.subtree_control"), "memory cpu pids").expect("fake");
        let error = CgroupRoot::open(dir.path()).expect_err("not cgroup2");
        assert!(
            error.detail.contains("not on a cgroup v2"),
            "{}",
            error.detail
        );
    }

    /// On a Linux host whose hierarchy is cgroup v2, the hierarchy itself is a
    /// root: `statfs` reports the cgroup2 magic. A platform step that reported
    /// any other value would refuse every real root and pass every refusal
    /// test, so this is the assertion that it reads the right number.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_host_cgroup2_hierarchy_opens_as_a_root() {
        let mounts = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let cgroup2 = mounts.lines().any(|line| {
            line.split_whitespace().nth(1) == Some("/sys/fs/cgroup") && line.contains(" cgroup2 ")
        });
        if !cgroup2 {
            eprintln!("skipped: /sys/fs/cgroup is not a cgroup2 mount here");
            return;
        }
        CgroupRoot::open(Path::new("/sys/fs/cgroup")).expect("the cgroup2 hierarchy is a root");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn there_is_no_cgroup_root_off_linux() {
        let dir = tempfile::tempdir().expect("dir");
        let error = CgroupRoot::open(dir.path()).expect_err("no cgroups here");
        assert!(error.detail.contains("only on Linux"), "{}", error.detail);
    }

    #[test]
    fn only_the_cgroup2_magic_makes_a_root() {
        let dir = tempfile::tempdir().expect("dir");
        fs::write(dir.path().join("cgroup.subtree_control"), "memory\n").expect("controllers");
        let root = CgroupRoot::on_filesystem(dir.path(), CGROUP2_SUPER_MAGIC).expect("cgroup2");
        assert_eq!(root.enforced().memory, CeilingMechanism::Cgroup);
        // A real root writes control files and never creates them.
        assert!(!root.stand_in);
        for other in [0, 1, 0xEF53 /* ext4 */, CGROUP2_SUPER_MAGIC + 1] {
            let error = CgroupRoot::on_filesystem(dir.path(), other).expect_err("not cgroup2");
            assert!(
                error.detail.contains("not on a cgroup v2"),
                "{}",
                error.detail
            );
        }
    }
}
