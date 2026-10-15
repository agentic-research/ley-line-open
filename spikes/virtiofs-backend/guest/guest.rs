// Guest program for the ADR-0044 virtio-fs spike. Built with:
//   rustc --edition 2021 --target aarch64-unknown-linux-musl -C linker=rust-lld \
//     -C target-feature=+crt-static -O guest.rs -o <rootfs>/guest
// Runs as the libkrun guest's exec target (root, after init.krun).

use std::ffi::{c_char, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::IntoRawFd;

extern "C" {
    fn mount(
        src: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: u64,
        data: *const c_char,
    ) -> i32;
    fn close(fd: i32) -> i32;
    fn fsync(fd: i32) -> i32;
    fn syscall(n: i64, ...) -> i64;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

const ESTALE: i32 = 116;
const EIO: i32 = 5;
const SYS_STATX: i64 = 291; // aarch64
const STATX_ATTR_DAX: u64 = 0x0020_0000;

static mut FAILURES: u32 = 0;

fn verdict(name: &str, ok: bool, detail: String) {
    println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        unsafe { FAILURES += 1 };
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn do_mount(tag: &str, target: &str, data: Option<&str>) -> Result<(), i32> {
    let _ = std::fs::create_dir_all(target);
    let (t, m, fs) = (
        CString::new(tag).unwrap(),
        CString::new(target).unwrap(),
        CString::new("virtiofs").unwrap(),
    );
    let d = data.map(|d| CString::new(d).unwrap());
    let dp = d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
    if unsafe { mount(t.as_ptr(), m.as_ptr(), fs.as_ptr(), 0, dp) } == 0 {
        Ok(())
    } else {
        Err(errno())
    }
}

fn read(path: &str) -> String {
    let mut s = String::new();
    File::open(path)
        .and_then(|mut f| f.read_to_string(&mut s))
        .map(|_| s)
        .unwrap_or_else(|e| format!("<{e}>"))
}

/// write -> optional fsync -> close; returns (fsync errno, close errno), 0 = success.
fn edit(path: &str, bytes: &[u8], do_fsync: bool) -> (i32, i32) {
    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for write");
    f.write_all(bytes).expect("write");
    let fd = f.into_raw_fd();
    let fs = if do_fsync {
        if unsafe { fsync(fd) } == 0 {
            0
        } else {
            errno()
        }
    } else {
        -1
    };
    let cl = if unsafe { close(fd) } == 0 {
        0
    } else {
        errno()
    };
    (fs, cl)
}

fn statx_attrs(path: &str) -> Option<u64> {
    let p = CString::new(path).unwrap();
    let mut buf = [0u8; 256];
    // AT_FDCWD = -100, flags 0, mask STATX_BASIC_STATS (0x7ff)
    let r = unsafe {
        syscall(
            SYS_STATX,
            -100i64,
            p.as_ptr(),
            0i64,
            0x7ffi64,
            buf.as_mut_ptr(),
        )
    };
    if r != 0 {
        return None;
    }
    Some(u64::from_le_bytes(buf[8..16].try_into().unwrap()))
}

/// Mount `tag` with dax=always, then read `file` with read(2) and with mmap(2).
fn dax_probe(tag: &str, dir: &str, file: &str, expect: &str) {
    let mounted = do_mount(tag, dir, Some("dax=always"));
    println!("mount {tag} dax=always: {mounted:?}");
    if mounted.is_err() {
        verdict(&format!("dax {tag}"), false, "mount refused".into());
        return;
    }
    let attrs = statx_attrs(file);
    let flagged = attrs.is_some_and(|a| a & STATX_ATTR_DAX != 0);
    let content = read(file);
    let f = File::open(file).unwrap();
    let fd = f.into_raw_fd();
    let p = unsafe { mmap(std::ptr::null_mut(), 4096, 1, 1, fd, 0) };
    let mapped = if p as isize == -1 {
        format!("<mmap errno {}>", errno())
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(p, expect.len()) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    verdict(
        &format!("dax {tag}"),
        flagged && content.trim_end() == expect && mapped == expect,
        format!(
            "STATX_ATTR_DAX={flagged} read={:?} mmap={mapped:?}",
            content.trim_end()
        ),
    );
}

fn main() {
    let expect = std::env::var("EXPECT").unwrap_or_default();
    if std::env::args().nth(1).as_deref() == Some("hostdax-probe") {
        dax_probe(
            "hostdax",
            "/dax",
            "/dax/probe.txt",
            "dax probe bytes from a host file",
        );
        std::process::exit(unsafe { FAILURES } as i32);
    }

    // 1. The llo share is served by the host process's memory.
    match do_mount("llo", "/llo", None) {
        Ok(()) => println!("mounted llo at /llo"),
        Err(e) => {
            verdict("mount llo", false, format!("errno {e}"));
            std::process::exit(1);
        }
    }
    let names: Vec<_> = std::fs::read_dir("/llo")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    println!("ls /llo: {names:?}");
    let hello = read("/llo/hello.txt");
    verdict(
        "read-from-host-memory",
        hello.trim_end() == expect && !expect.is_empty(),
        format!("guest read {:?}", hello.trim_end()),
    );

    // 2. An accepted edit: fsync and close succeed, re-read sees it.
    let (fs, cl) = edit("/llo/ok.txt", b"key=edited\n", true);
    let back = read("/llo/ok.txt");
    verdict(
        "accepted edit",
        fs == 0 && cl == 0 && back == "key=edited\n",
        format!("fsync={fs} close={cl} reread={back:?}"),
    );

    // 3. A stale edit: the backend's FSYNC and FLUSH refusals reach fsync and close.
    let (fs, cl) = edit("/llo/stale.txt", b"key=mine\n", true);
    verdict(
        "stale edit -> ESTALE",
        fs == ESTALE && cl == ESTALE,
        format!(
            "fsync errno={fs} close errno={cl} (ESTALE={ESTALE}); store holds {:?}",
            read("/llo/stale.txt")
        ),
    );

    // 4. An unparseable edit with no fsync: the FLUSH refusal reaches close.
    let (_, cl) = edit("/llo/syntax.txt", b"garbage{\n", false);
    verdict(
        "syntax edit -> EIO on close",
        cl == EIO,
        format!(
            "close errno={cl} (EIO={EIO}); store holds {:?}",
            read("/llo/syntax.txt")
        ),
    );

    // 5. DAX: the in-memory backend, then a passthrough share, each with a 64 MiB window.
    dax_probe("llodax", "/llodax", "/llodax/hello.txt", &expect);
    // libkrun's passthrough DAX can fault the reader; probe it in a child.
    let status = std::process::Command::new("/guest")
        .arg("hostdax-probe")
        .env("EXPECT", &expect)
        .status();
    println!("hostdax probe child: {status:?}");
    for line in read("/proc/mounts")
        .lines()
        .filter(|l| l.contains("virtiofs"))
    {
        println!("/proc/mounts: {line}");
    }

    let failures = unsafe { FAILURES };
    println!("guest done: {failures} failure(s)");
    std::process::exit(failures as i32);
}
