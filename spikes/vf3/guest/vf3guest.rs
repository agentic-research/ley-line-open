//! V-F3 workloads. One source, two builds: a static aarch64-linux-musl guest
//! (`/vf3guest` in the libkrun rootfs, mounting the virtio-fs share itself)
//! and a macOS binary run directly against an APFS directory.
//!
//! Environment:
//! - `VF3_ROOT`       tree to work in (guest: `/work`, the mount point)
//! - `VF3_MODE`       `cold` (one recorded run) or `warm` (one unrecorded
//!                    warm-up, then `VF3_N` recorded runs)
//! - `VF3_N`          recorded runs in warm mode
//! - `VF3_WORKLOADS`  comma list of status, rg, find, edit, create, untar, correct
//! - `VF3_LABEL`      label echoed on every RESULT line
//! - `VF3_EDIT_FILE`  file (relative to the root) the edit loop rewrites
//! - `VF3_RG`         ripgrep pattern
//! - `VF3_TARBALL`    tarball the untar workload extracts
//! - `VF3_MOUNT_OPTS` (guest) virtio-fs mount data, e.g. `dax=inode`
//!
//! Output: `RESULT <label> <workload> <run> wall_us=<n>` per run, and for the
//! per-operation loops `OPS <label> <workload> <run> <us,us,...>`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::IntoRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

extern "C" {
    fn close(fd: i32) -> i32;
}

#[cfg(target_os = "linux")]
mod guest {
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

    pub fn mount_fs(source: &str, target: &str, fstype: &str, data: &str) {
        let _ = std::fs::create_dir_all(target);
        let source = CString::new(source).unwrap();
        let target_c = CString::new(target).unwrap();
        let fstype = CString::new(fstype).unwrap();
        let data = CString::new(data).unwrap();
        // SAFETY: live NUL-terminated strings.
        let rc = unsafe {
            mount(
                source.as_ptr(),
                target_c.as_ptr(),
                fstype.as_ptr(),
                0,
                data.as_ptr() as *const c_void,
            )
        };
        if rc != 0 {
            println!("FATAL mount {target}: {}", std::io::Error::last_os_error());
            std::process::exit(2);
        }
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn errno_name(e: &io::Error) -> String {
    let n = e.raw_os_error().unwrap_or(-1);
    let name = if cfg!(target_os = "linux") {
        match n {
            1 => "EPERM",
            2 => "ENOENT",
            5 => "EIO",
            9 => "EBADF",
            13 => "EACCES",
            17 => "EEXIST",
            22 => "EINVAL",
            116 => "ESTALE",
            _ => "?",
        }
    } else {
        "?"
    };
    format!("{name}({n})")
}

fn outcome<T>(r: &io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => errno_name(e),
    }
}

fn explicit_close(file: File) -> io::Result<()> {
    let fd = file.into_raw_fd();
    // SAFETY: we own fd; it is closed exactly once here.
    if unsafe { close(fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct Ctx {
    root: PathBuf,
    label: String,
    ctl: Option<PathBuf>,
}

impl Ctx {
    fn ctl(&self, command: &str) {
        if let Some(ctl) = &self.ctl {
            let mut f = OpenOptions::new().write(true).open(ctl).expect("open ctl");
            f.write_all(format!("{command}\n").as_bytes()).expect("ctl write");
        }
    }
}

fn run_cmd(ctx: &Ctx, cmd: &mut Command, ok_codes: &[i32]) -> String {
    let out = cmd
        .current_dir(&ctx.root)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn workload command");
    let code = out.status.code().unwrap_or(-1);
    if !ok_codes.contains(&code) {
        println!(
            "FATAL {:?} exited {code}: {}",
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
        std::process::exit(3);
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn report_ops(ctx: &Ctx, workload: &str, run: usize, wall_us: u64, ops: &[u64]) {
    let mut sorted = ops.to_vec();
    sorted.sort_unstable();
    println!(
        "RESULT {} {workload} {run} wall_us={wall_us} p50_us={} p99_us={} ops={}",
        ctx.label,
        percentile(&sorted, 50.0),
        percentile(&sorted, 99.0),
        ops.len()
    );
    let joined: Vec<String> = ops.iter().map(|v| v.to_string()).collect();
    println!("OPS {} {workload} {run} {}", ctx.label, joined.join(","));
}

/// One recorded (or warm-up) execution of a workload.
fn execute(ctx: &Ctx, workload: &str, run: usize, record: bool) {
    let root = &ctx.root;
    let tag = format!("{}-{}-{run}-{}", workload, std::process::id(), if record { "r" } else { "w" });
    if record {
        ctx.ctl("reset");
    }
    let started = Instant::now();
    let mut ops: Vec<u64> = Vec::new();
    match workload {
        "status" => {
            let out = run_cmd(ctx, Command::new("git").args(["status", "--porcelain"]), &[0]);
            if run == 0 {
                let first: Vec<&str> = out.lines().take(3).collect();
                println!(
                    "INFO {} status porcelain_lines={} first={:?}",
                    ctx.label,
                    out.lines().count(),
                    first
                );
            }
        }
        "rg" => {
            let pattern = env("VF3_RG", "ESTALE");
            let out = run_cmd(
                ctx,
                Command::new("rg").args(["-j4", "-c", "--no-messages", &pattern]),
                &[0, 1],
            );
            if run == 0 {
                println!("INFO {} rg matching_files={}", ctx.label, out.lines().count());
            }
        }
        "find" => {
            let out = run_cmd(ctx, Command::new("sh").args(["-c", "find . -type f | wc -l"]), &[0]);
            if run == 0 {
                println!("INFO {} find files={}", ctx.label, out.trim());
            }
        }
        "edit" => {
            let path = root.join(env("VF3_EDIT_FILE", "llo/rs/ll-open/storefs/README.md"));
            for i in 0..1000u32 {
                let t = Instant::now();
                let mut f = OpenOptions::new().read(true).write(true).open(&path).expect("edit open");
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).expect("edit read");
                let at = (i as usize * 7) % buf.len().max(1);
                buf[at] = if buf[at] == b'#' { b'%' } else { b'#' };
                f.write_all_at(&buf, 0).expect("edit write");
                explicit_close(f).expect("edit close");
                ops.push(t.elapsed().as_micros() as u64);
            }
        }
        "create" => {
            let dir = root.join(format!("storm-{tag}"));
            fs::create_dir(&dir).expect("storm dir");
            let payload = [b'x'; 128];
            for i in 0..1000 {
                let t = Instant::now();
                let mut f = File::create_new(dir.join(format!("f{i:04}.txt"))).expect("storm create");
                f.write_all(&payload).expect("storm write");
                explicit_close(f).expect("storm close");
                ops.push(t.elapsed().as_micros() as u64);
            }
        }
        "untar" => {
            let dir = root.join(format!("untar-{tag}"));
            fs::create_dir(&dir).expect("untar dir");
            let tarball = env("VF3_TARBALL", "/tmp/tarball.tar");
            run_cmd(
                ctx,
                Command::new("tar").arg("-xf").arg(&tarball).arg("-C").arg(&dir),
                &[0],
            );
        }
        "correct" => correctness(ctx),
        other => panic!("unknown workload {other}"),
    }
    let wall_us = started.elapsed().as_micros() as u64;
    if record {
        ctx.ctl(&format!("stats {} {workload} {run}", ctx.label));
        if ops.is_empty() {
            println!("RESULT {} {workload} {run} wall_us={wall_us}", ctx.label);
        } else {
            report_ops(ctx, workload, run, wall_us, &ops);
        }
    }
}

/// V-F3 D: which syscall carries a refusal.
fn correctness(ctx: &Ctx) {
    let dir = ctx.root.join("dtest");
    let _ = fs::create_dir(&dir);
    for name in ["d1", "d2", "d3", "d4", "d5", "d6"] {
        fs::write(dir.join(name), b"line one\n").expect("seed d file");
    }
    let rel = |n: &str| format!("dtest/{n}");
    let p = |n: &str| dir.join(n);
    let store = |n: &str| fs::read(p(n)).map(|b| String::from_utf8_lossy(&b).replace('\n', "|"));

    // D1: another writer moves the file between the guest's open and write.
    let mut f = OpenOptions::new().read(true).write(true).open(p("d1")).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    ctx.ctl(&format!("bump {}", rel("d1")));
    let w = f.write_all_at(b"guest edit\n", 0);
    let s = f.sync_all();
    let c = explicit_close(f);
    println!(
        "D {} D1 open,read,FOREIGN,write,fsync,close: write={} fsync={} close={} store={:?}",
        ctx.label, outcome(&w), outcome(&s), outcome(&c), store("d1")
    );

    // D2: the guest wrote (staged) before the foreign change; then fsync, close.
    let f = OpenOptions::new().read(true).write(true).open(p("d2")).unwrap();
    let w = f.write_all_at(b"guest edit\n", 0);
    ctx.ctl(&format!("bump {}", rel("d2")));
    let s = f.sync_all();
    let c = explicit_close(f);
    println!(
        "D {} D2 open,write,FOREIGN,fsync,close: write={} fsync={} close={} store={:?}",
        ctx.label, outcome(&w), outcome(&s), outcome(&c), store("d2")
    );

    // D3: staged write, foreign change, close with no fsync.
    let f = OpenOptions::new().read(true).write(true).open(p("d3")).unwrap();
    let w = f.write_all_at(b"guest edit\n", 0);
    ctx.ctl(&format!("bump {}", rel("d3")));
    let c = explicit_close(f);
    println!(
        "D {} D3 open,write,FOREIGN,close: write={} close={} store={:?}",
        ctx.label, outcome(&w), outcome(&c), store("d3")
    );

    // D4: the backend refuses RENAME and UNLINK of d4.
    ctx.ctl(&format!("refuse {}", rel("d4")));
    let r = fs::rename(p("d4"), p("d4.renamed"));
    let after_rename = (p("d4").exists(), p("d4.renamed").exists());
    let u = fs::remove_file(p("d4"));
    let after_unlink = p("d4").exists();
    println!(
        "D {} D4 refused namespace ops: rename={} (d4 exists={}, d4.renamed exists={}) unlink={} (d4 exists={})",
        ctx.label, outcome(&r), after_rename.0, after_rename.1, outcome(&u), after_unlink
    );

    // D5: does the guest see a foreign change without invalidation?
    let before = fs::metadata(p("d5")).map(|m| m.len());
    let _ = fs::read(p("d5"));
    ctx.ctl(&format!("bump {}", rel("d5")));
    let after = fs::metadata(p("d5")).map(|m| m.len());
    let reread = fs::read(p("d5")).map(|b| b.len());
    println!(
        "D {} D5 foreign change visibility: size_before={:?} stat_size_after={:?} reread_len={:?} (store now 35 bytes)",
        ctx.label, before, after, reread
    );

    // D6: a foreign change BEFORE open: the open takes the new base.
    ctx.ctl(&format!("bump {}", rel("d6")));
    let f = OpenOptions::new().read(true).write(true).open(p("d6")).unwrap();
    let w = f.write_all_at(b"guest edit\n", 0);
    let s = f.sync_all();
    let c = explicit_close(f);
    println!(
        "D {} D6 FOREIGN,open,write,fsync,close: write={} fsync={} close={} store={:?}",
        ctx.label, outcome(&w), outcome(&s), outcome(&c), store("d6")
    );
}

fn main() {
    #[cfg(target_os = "linux")]
    {
        guest::mount_fs("vf3", "/work", "virtiofs", &env("VF3_MOUNT_OPTS", ""));
        guest::mount_fs("tmpfs", "/tmp", "tmpfs", "");
        if std::path::Path::new("/tarball.tar").exists() {
            fs::copy("/tarball.tar", "/tmp/tarball.tar").expect("stage tarball in tmpfs");
        }
    }
    let root = PathBuf::from(env("VF3_ROOT", "/work"));
    let ctl = root.join(".vf3ctl");
    let ctx = Ctx {
        label: env("VF3_LABEL", "unlabelled"),
        ctl: cfg!(target_os = "linux").then_some(ctl),
        root,
    };
    // Load the tools once outside the tree under test, so a cold run times the
    // tree and not the first read of the binaries (the guest's rootfs is a
    // separate passthrough share).
    for tool in [&["git", "--version"][..], &["rg", "--version"], &["sh", "-c", "find /dev | wc -l"], &["tar", "--help"]] {
        let _ = Command::new(tool[0])
            .args(&tool[1..])
            .current_dir("/")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mode = env("VF3_MODE", "warm");
    let n: usize = env("VF3_N", "5").parse().unwrap();
    for workload in env("VF3_WORKLOADS", "").split(',').filter(|w| !w.is_empty()) {
        if workload == "correct" {
            execute(&ctx, workload, 0, false);
            continue;
        }
        match mode.as_str() {
            "cold" => execute(&ctx, workload, 0, true),
            _ => {
                execute(&ctx, workload, 0, false);
                for run in 0..n {
                    execute(&ctx, workload, run, true);
                }
            }
        }
    }
    println!("DONE {}", ctx.label);
}
