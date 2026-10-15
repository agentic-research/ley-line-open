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

use leyline_runtime::DigestRef;
use leyline_runtime::backends::libkrun::api::{LinkedKrunApi, prepare_vm};
use leyline_runtime::backends::libkrun::plan::{KrunConfig, ResolvedRootfs};
use tempfile::TempDir;
use wait_timeout::ChildExt;

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
