//! ADR-0044 phase A (bead `ley-line-open-f1c46f`): a guest reads a file that
//! the linked VMM serves from its own memory, through a custom virtio-fs share.
//!
//! The VMM is this test binary. `krun_start_enter` never returns — libkrun
//! exits the process with the guest's status — so the test re-executes a
//! hypervisor-entitled copy of itself with `VMM_CHILD` set, and that child
//! builds the VM through the runtime's own API (`prepare_vm`,
//! `PreparedVm::add_virtiofs_backend`, `PreparedVm::enter`) and enters it. The
//! parent checks what the guest printed and how the VMM exited.
//!
//! The bytes the guest must read exist only in the child: the parent makes a
//! token, hands it to the child, the child's in-memory backend serves it under
//! tag `llo`, and no file anywhere holds it.

use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use leyline_krun_worker::api::{LinkedKrunApi, prepare_vm};
use leyline_runtime::DigestRef;
use leyline_runtime::backends::libkrun::plan::{KrunConfig, ResolvedRootfs};
use tempfile::TempDir;
use wait_timeout::ChildExt;

#[path = "../../runtime/tests/common/mod.rs"]
mod common;
mod hypervisor;
mod memfs;

/// Set in the child to the guest rootfs path. Its presence is what makes the
/// test body act as the VMM instead of as the test.
const VMM_CHILD: &str = "LEYLINE_TEST_VIRTIOFS_VMM_ROOTFS";
/// The token the child serves and the guest must read back.
const TOKEN: &str = "LEYLINE_TEST_VIRTIOFS_TOKEN";

const GUEST: &str = r#"
use std::ffi::{c_char, c_ulong, c_void, CString};

extern "C" {
    fn mount(
        source: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> i32;
}

fn main() {
    let tag = CString::new("llo").unwrap();
    let target = CString::new("/llo").unwrap();
    let fstype = CString::new("virtiofs").unwrap();
    // SAFETY: three live NUL-terminated strings and no mount data.
    let rc = unsafe {
        mount(tag.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
    };
    if rc != 0 {
        println!("guest mount failed: {}", std::io::Error::last_os_error());
        std::process::exit(2);
    }
    let read = match std::fs::read_to_string("/llo/hello.txt") {
        Ok(read) => read,
        Err(error) => {
            println!("guest read failed: {error}");
            std::process::exit(3);
        }
    };
    println!("guest read: {}", read.trim_end());
    let expected = format!("{}\n", std::env::var("EXPECT").unwrap());
    std::process::exit(if read == expected { 0 } else { 1 });
}
"#;

/// Set in the child when the guest init, not the guest, mounts the share:
/// the VMM lists it in `KRUN_VIRTIOFS_MOUNTS` (LLO libkrun patch 0005) and
/// makes it the working directory.
const INIT_MOUNTS: &str = "LEYLINE_TEST_VIRTIOFS_INIT_MOUNTS";

/// A guest that mounts nothing: it reads `hello.txt` from its working
/// directory, which must be the share init mounted, and must not see the
/// mount list in its environment.
const GUEST_MOUNTED_BY_INIT: &str = r#"
fn main() {
    let cwd = std::env::current_dir().unwrap();
    println!("guest cwd: {}", cwd.display());
    if std::env::var_os("KRUN_VIRTIOFS_MOUNTS").is_some() {
        println!("guest saw KRUN_VIRTIOFS_MOUNTS");
        std::process::exit(4);
    }
    let read = match std::fs::read_to_string("hello.txt") {
        Ok(read) => read,
        Err(error) => {
            println!("guest read failed: {error}");
            std::process::exit(3);
        }
    };
    println!("guest read: {}", read.trim_end());
    let expected = format!("{}\n", std::env::var("EXPECT").unwrap());
    std::process::exit(if read == expected && cwd.as_os_str() == "/llo" { 0 } else { 1 });
}
"#;

/// Set in the child to the extra environment entries (`KEY=value`, one per
/// line) to hand the guest init, for a boot init must refuse.
const INIT_ENV: &str = "LEYLINE_TEST_VIRTIOFS_INIT_ENV";

/// Prints one line if it runs at all; init must stop before it does.
const GUEST_THAT_MUST_NOT_RUN: &str = r#"
fn main() {
    println!("guest ran");
}
"#;

/// The exit status init reports when it stops a guest before the workload
/// runs (LLO libkrun patch 0015, bead `ley-line-open-7164ec`).
const STOP_EXIT_CODE: i32 = 125;

/// What a refused boot's rootfs holds beyond `bin/guest`.
enum Plant {
    Nothing,
    /// A symbolic link at the first path, pointing at the second.
    Link(&'static str, &'static str),
    /// A regular file at this path.
    File(&'static str),
}

/// One boot init must refuse: what the rootfs holds, the environment init
/// is given, and the reason it must state where the host reads it (`None`
/// when it stops before the krun console ports exist, so only the exit
/// status reaches the host).
struct Refusal {
    plant: Plant,
    init_env: &'static [&'static str],
    says: Option<&'static str>,
}

/// A guest that stops without recording an exit status does not read as a
/// success (LLO libkrun patch 0016, bead `ley-line-open-7164ec`). With
/// KRUN_INIT_PID1 the workload runs as PID 1: it prints and exits 0, the
/// kernel panics, and nothing records a status. Upstream libkrun then took
/// the vcpu's "OK" and exited 0, as it did after any init crash or kernel
/// panic. The VMM must exit 125 and say why on its stderr.
#[test]
fn a_guest_that_records_no_exit_status_ends_as_a_failure() {
    if let Some(rootfs) = std::env::var_os(VMM_CHILD) {
        enter_guest(PathBuf::from(rootfs));
    }
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let rootfs = fixture.path().join("rootfs");
    fs::create_dir_all(rootfs.join("bin")).expect("rootfs bin");
    let source = fixture.path().join("guest.rs");
    fs::write(&source, GUEST_THAT_MUST_NOT_RUN).expect("guest source");
    let guest = rootfs.join("bin/guest");
    hypervisor::compile_guest(&source, &guest);
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).expect("guest mode");
    let vmm = fixture.path().join("vmm");
    hypervisor::entitled_copy(&std::env::current_exe().expect("test binary"), &vmm);
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    let mut child = Command::new(&vmm)
        .args([
            "a_guest_that_records_no_exit_status_ends_as_a_failure",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(VMM_CHILD, &rootfs)
        .env(TOKEN, "unused")
        .env(INIT_ENV, "KRUN_INIT_PID1=1")
        .env(library_var, library_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the VMM");
    let status = child
        .wait_timeout(Duration::from_secs(30))
        .expect("wait for the VMM");
    if status.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().expect("VMM output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        status.is_some(),
        "the guest did not stop within 30 seconds\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("guest ran"),
        "the workload ran as PID 1, so its exit is what stops the guest:\n{stdout}\n{stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(STOP_EXIT_CODE),
        "no recorded status is not a success: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("the guest stopped without recording an exit status"),
        "the VMM says why it exits 125:\n{stderr}"
    );
}

/// Init mounts only over a real directory (LLO libkrun patch 0014, bead
/// `ley-line-open-2594af`; defence in depth for `ley-line-open-10b516`).
/// mount(2) follows a link, so a rootfs holding `work -> bin` with a
/// workspace attached at /work would get the share over /bin, covering the
/// executable the run verified. For a top-level share target, a nested one
/// whose parent is a link, and one of init's own kernel mounts, init must
/// refuse and stop the guest as a failure before the workload runs, and
/// create nothing through the link.
#[test]
fn the_guest_init_refuses_a_mount_target_that_is_a_link() {
    if let Some(rootfs) = std::env::var_os(VMM_CHILD) {
        enter_guest(PathBuf::from(rootfs));
    }
    for refusal in [
        Refusal {
            plant: Plant::Link("work", "bin"),
            init_env: &["KRUN_VIRTIOFS_MOUNTS=llo:/work"],
            says: Some("/work is not a directory, refusing to mount over it"),
        },
        Refusal {
            plant: Plant::Link("srv", "bin"),
            init_env: &["KRUN_VIRTIOFS_MOUNTS=llo:/srv/work"],
            says: Some("/srv/work is not a directory, refusing to mount over it"),
        },
        // Refused before /sys and /dev are mounted, so before the krun
        // console ports exist: the host sees the exit status alone.
        Refusal {
            plant: Plant::Link("dev", "bin"),
            init_env: &[],
            says: None,
        },
    ] {
        if boot_refused(
            "the_guest_init_refuses_a_mount_target_that_is_a_link",
            refusal,
        )
        .is_none()
        {
            return;
        }
    }
}

/// A guest init that refuses to start the workload stops the guest as a
/// failure the host sees, with its reason on the console the host reads
/// (LLO libkrun patch 0015, bead `ley-line-open-7164ec`). Upstream init
/// called exit() from PID 1, the kernel panicked, and libkrun reported exit
/// status 0 with an empty console: a refused boot read as a success. Each
/// of LLO's own pre-workload refusals: a /tmp that is not a directory
/// (patch 0012), a missing exec config (0013), and a channel list init
/// cannot set up (0010).
#[test]
fn a_refused_boot_ends_as_a_failure_with_its_reason() {
    if let Some(rootfs) = std::env::var_os(VMM_CHILD) {
        enter_guest(PathBuf::from(rootfs));
    }
    for refusal in [
        Refusal {
            plant: Plant::File("tmp"),
            init_env: &[],
            says: Some("/tmp is not a directory, refusing to mount over it"),
        },
        Refusal {
            plant: Plant::Nothing,
            init_env: &["KRUN_CONFIG=/no-such-exec.json", "KRUN_CONFIG_ARGV=1"],
            says: Some("KRUN_CONFIG_ARGV is set and /no-such-exec.json is not a valid exec config"),
        },
        Refusal {
            plant: Plant::Nothing,
            init_env: &["KRUN_LOOPBACK_CHANNELS=sideways:1:2"],
            says: Some("Couldn't set up KRUN_LOOPBACK_CHANNELS, bailing out"),
        },
    ] {
        if boot_refused("a_refused_boot_ends_as_a_failure_with_its_reason", refusal).is_none() {
            return;
        }
    }
}

/// Boot a rootfs holding `bin/guest` and `refusal.plant`, with init given
/// `refusal.init_env`, through the entitled re-exec of test `name`. Asserts
/// that the VMM exits with STOP_EXIT_CODE, that the workload never ran,
/// that the console states the reason, and that nothing was created through
/// a planted link. `None` when there is no hypervisor here.
fn boot_refused(name: &str, refusal: Refusal) -> Option<()> {
    let _serial = common::serial();
    let libkrunfw = hypervisor::libkrunfw_or_skip()?;
    let fixture = TempDir::new().expect("fixture");
    let fixture = fixture.path();

    let rootfs = fixture.join("rootfs");
    fs::create_dir_all(rootfs.join("bin")).expect("rootfs bin");
    let source = fixture.join("guest.rs");
    fs::write(&source, GUEST_THAT_MUST_NOT_RUN).expect("guest source");
    let guest = rootfs.join("bin/guest");
    hypervisor::compile_guest(&source, &guest);
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).expect("guest mode");
    match refusal.plant {
        Plant::Nothing => {}
        Plant::Link(at, to) => {
            std::os::unix::fs::symlink(to, rootfs.join(at)).expect("the planted link");
        }
        Plant::File(at) => {
            fs::write(rootfs.join(at), b"not a directory").expect("the planted file")
        }
    }

    let vmm = fixture.join("vmm");
    hypervisor::entitled_copy(&std::env::current_exe().expect("test binary"), &vmm);
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    let mut child = Command::new(&vmm)
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(VMM_CHILD, &rootfs)
        .env(TOKEN, "unused")
        .env(INIT_ENV, refusal.init_env.join("\n"))
        .env(library_var, library_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the VMM");
    let status = child
        .wait_timeout(Duration::from_secs(30))
        .expect("wait for the VMM");
    if status.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().expect("VMM output");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        status.is_some(),
        "the guest did not stop within 30 seconds\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(STOP_EXIT_CODE),
        "init stops the guest as a failure, not a panic that reads as exit 0: {:?}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    if let Some(reason) = refusal.says {
        assert!(
            stdout.contains(reason),
            "init states why ({reason}) where the host reads it:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    assert!(
        !stdout.contains("guest ran"),
        "the workload never runs:\n{stdout}"
    );
    // A share target is walked before init creates it: nothing appears
    // through a planted link.
    for created in ["bin/work", "bin/llo"] {
        assert!(
            !rootfs.join(created).exists(),
            "init created {created} through a link before refusing"
        );
    }
    Some(())
}

#[test]
fn a_guest_reads_a_file_the_linked_vmm_serves_from_memory() {
    if let Some(rootfs) = std::env::var_os(VMM_CHILD) {
        enter_guest(PathBuf::from(rootfs));
    }
    boot(
        "a_guest_reads_a_file_the_linked_vmm_serves_from_memory",
        GUEST,
        false,
    );
}

#[test]
fn the_guest_init_mounts_the_share_the_vmm_lists_before_the_workload_runs() {
    if let Some(rootfs) = std::env::var_os(VMM_CHILD) {
        enter_guest(PathBuf::from(rootfs));
    }
    boot(
        "the_guest_init_mounts_the_share_the_vmm_lists_before_the_workload_runs",
        GUEST_MOUNTED_BY_INIT,
        true,
    );
}

/// Boot `guest` through the entitled re-exec of test `name` and check what
/// it printed.
fn boot(name: &str, guest: &str, init_mounts: bool) {
    let _serial = common::serial();
    let Some(libkrunfw) = hypervisor::libkrunfw_or_skip() else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");

    let rootfs = fixture.path().join("rootfs");
    fs::create_dir_all(rootfs.join("bin")).expect("rootfs bin");
    if !init_mounts {
        fs::create_dir(rootfs.join("llo")).expect("mount point");
    }
    let source = fixture.path().join("guest.rs");
    fs::write(&source, guest).expect("guest source");
    let guest = rootfs.join("bin/guest");
    hypervisor::compile_guest(&source, &guest);
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).expect("guest mode");

    let vmm = fixture.path().join("vmm");
    hypervisor::entitled_copy(&std::env::current_exe().expect("test binary"), &vmm);

    let token = format!(
        "served-from-vmm-memory pid={} t={}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let (library_var, library_dir) = hypervisor::library_path_env(&libkrunfw);
    let mut command = Command::new(&vmm);
    if init_mounts {
        command.env(INIT_MOUNTS, "1");
    }
    let mut child = command
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(VMM_CHILD, &rootfs)
        .env(TOKEN, &token)
        .env(library_var, library_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the VMM");

    let status = child
        .wait_timeout(Duration::from_secs(30))
        .expect("wait for the VMM");
    if status.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().expect("VMM output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        status.is_some(),
        "the guest did not exit within 30 seconds\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("guest read: {token}")),
        "the guest must print the bytes the backend served\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "the VMM exits with the guest's status, and the guest exits 0 only on \
         an exact match: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    eprintln!("VMM stdout (guest console):\n{stdout}");
}

/// The child: build the VM through the runtime's API and enter it. Returns
/// only by panicking; on success libkrun exits the process with the guest's
/// status.
fn enter_guest(rootfs: PathBuf) -> ! {
    let token = std::env::var(TOKEN).expect("token from the parent");
    let init_mounts = std::env::var_os(INIT_MOUNTS).is_some();
    let mut environment = vec![c(&format!("EXPECT={token}"))];
    if init_mounts {
        environment.push(c("KRUN_VIRTIOFS_MOUNTS=llo:/llo"));
    }
    if let Ok(entries) = std::env::var(INIT_ENV) {
        environment.extend(entries.lines().filter(|e| !e.is_empty()).map(c));
    }
    let config = KrunConfig {
        run_id: "virtiofs-backend".into(),
        rootfs: ResolvedRootfs {
            digest: DigestRef {
                algorithm: "blake3-256".into(),
                value: "0".repeat(64),
            },
            canonical_path: rootfs.canonicalize().expect("canonical rootfs"),
        },
        executable: c("/bin/guest"),
        arguments: vec![c("guest")],
        environment,
        workdir: c(if init_mounts { "/llo" } else { "/" }),
        vcpus: 1,
        ram_mib: 256,
        wall_time_ms: 30_000,
        tsi_features: 0,
        port_map: Vec::new(),
        vsock_unix_map: Vec::new(),
    };
    let backend = memfs::MemFs::new([("hello.txt".to_owned(), format!("{token}\n").into_bytes())]);

    let vm = prepare_vm(&LinkedKrunApi, &config).expect("prepare the VM");
    vm.add_virtiofs_backend("llo", 0, Arc::new(backend))
        .expect("attach the in-memory share");
    let error = vm.enter().expect_err("krun_start_enter returned");
    panic!("the VMM did not start: {error}");
}

fn c(value: &str) -> CString {
    CString::new(value).expect("no NUL")
}
