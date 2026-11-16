//! Every daemon spawn hands a child exactly what it names (ADR-0046 Slice 1
//! S7; bead `ley-line-open-528525`).
//!
//! N children are spawned concurrently, each handed the write end of a pipe
//! of its own. Afterwards each write end is held only by this process and
//! its own child: no sibling spawned on another thread inherited it. A
//! descriptor this process holds without close-on-exec, and does not name,
//! is absent in every child.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Barrier};

use leyline_runtime::backends::spawn::{ChildIo, Spawn, Spawned};

mod common;

const CHILDREN: usize = 16;
/// The number each child holds its write end at.
const CHILD_FD: RawFd = 3;

/// An identifier for the pipe `fd` is one end of, the same in every
/// process that holds it: macOS `lsof`'s pipe address, Linux's inode.
fn pipe_id(pid: u32, fd: RawFd) -> String {
    pipe_id_of(pid, fd).unwrap_or_else(|| panic!("no descriptor {fd} in {pid}"))
}

/// [`pipe_id`], or `None` when `pid` holds no descriptor `fd` (it may close
/// one between two listings).
fn pipe_id_of(pid: u32, fd: RawFd) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .ok()
            .map(|link| link.display().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args([
                "-a",
                "-n",
                "-P",
                "-p",
                &pid.to_string(),
                "-d",
                &fd.to_string(),
                "-F",
                "fd",
            ])
            .output()
            .expect("run lsof");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix('d').map(str::to_owned))
    }
}

/// Every (pid, fd) among `pids` holding the pipe `id`.
fn holders(id: &str, pids: &[u32]) -> Vec<(u32, RawFd)> {
    descriptors(pids)
        .into_iter()
        .filter(|(_, fd, pipe)| *fd > 2 && pipe.as_deref() == Some(id))
        .map(|(pid, fd, _)| (pid, fd))
        .collect()
}

/// Every numbered descriptor of `pids`, with its pipe identifier when it is
/// a pipe, from one listing.
fn descriptors(pids: &[u32]) -> Vec<(u32, RawFd, Option<String>)> {
    #[cfg(target_os = "linux")]
    {
        let mut all = Vec::new();
        for &pid in pids {
            for fd in open_fds(pid) {
                let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
                    .ok()
                    .map(|link| link.display().to_string())
                    .filter(|link| link.starts_with("pipe:"));
                all.push((pid, fd, link));
            }
        }
        all
    }
    #[cfg(target_os = "macos")]
    {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-n", "-P", "-p", &list, "-F", "pftd"])
            .output()
            .expect("run lsof");
        let mut all = Vec::new();
        let mut pid = 0u32;
        let mut current: Option<(RawFd, bool, Option<String>)> = None;
        let flush = |pid: u32, current: Option<(RawFd, bool, Option<String>)>, all: &mut Vec<_>| {
            if let Some((fd, pipe, device)) = current {
                all.push((pid, fd, device.filter(|_| pipe)));
            }
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (tag, value) = line.split_at(1);
            match tag {
                "p" => {
                    flush(pid, current.take(), &mut all);
                    pid = value.parse().unwrap();
                }
                "f" => {
                    flush(pid, current.take(), &mut all);
                    current = value.parse().ok().map(|fd| (fd, false, None));
                }
                "t" => {
                    if let Some(entry) = current.as_mut() {
                        entry.1 = value == "PIPE";
                    }
                }
                "d" => {
                    if let Some(entry) = current.as_mut() {
                        entry.2 = Some(value.to_owned());
                    }
                }
                _ => {}
            }
        }
        flush(pid, current, &mut all);
        all
    }
}

/// The descriptor numbers open in `pid`.
fn open_fds(pid: u32) -> Vec<RawFd> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .expect("list the process's descriptors")
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-n", "-P", "-p", &pid.to_string(), "-F", "f"])
            .output()
            .expect("run lsof");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('f')?.parse().ok())
            .collect()
    }
}

#[test]
fn concurrent_spawns_each_inherit_only_their_own_write_end() {
    let _serial = common::serial();
    // A descriptor this process holds WITHOUT close-on-exec and names to no
    // child: no child may hold it.
    // SAFETY: dup of /dev/null's descriptor; owned below.
    let stray = {
        let null = std::fs::File::open("/dev/null").unwrap();
        let raw = unsafe { libc::dup(null.as_raw_fd()) };
        assert!(raw >= 0);
        unsafe { OwnedFd::from_raw_fd(raw) }
    };
    let barrier = Arc::new(Barrier::new(CHILDREN));
    let spawned: Vec<(Spawned, OwnedFd, OwnedFd)> = (0..CHILDREN)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let (reader, writer) = std::io::pipe().unwrap();
                let (reader, writer) = (OwnedFd::from(reader), OwnedFd::from(writer));
                barrier.wait();
                let mut spawn = Spawn::new("/bin/cat");
                spawn
                    .stdin(ChildIo::Piped)
                    .stdout(ChildIo::Null)
                    .inherit(writer.as_raw_fd(), CHILD_FD)
                    .process_group();
                let child = spawn.spawn().expect("spawn");
                (child, reader, writer)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    let me = std::process::id();
    let mut pids: Vec<u32> = spawned.iter().map(|(child, _, _)| child.id()).collect();
    pids.push(me);
    for (child, _, writer) in &spawned {
        let id = pipe_id(me, writer.as_raw_fd());
        let mut held = holders(&id, &pids);
        held.sort();
        let mut expected = vec![(me, writer.as_raw_fd()), (child.id(), CHILD_FD)];
        expected.sort();
        assert_eq!(
            held,
            expected,
            "write end {id} must be held only by this process and child {}",
            child.id()
        );
        let in_child = open_fds(child.id());
        assert!(
            !in_child.contains(&stray.as_raw_fd()) || stray.as_raw_fd() == CHILD_FD,
            "child {} holds descriptor {}, which nothing named: {in_child:?}",
            child.id(),
            stray.as_raw_fd()
        );
    }
    for (mut child, _, _) in spawned {
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}

#[test]
fn an_inherited_descriptor_on_a_standard_stream_or_named_twice_is_refused() {
    for inherit in [&[(5, 1)][..], &[(5, 4), (6, 4)]] {
        let mut spawn = Spawn::new("/bin/true");
        for &(from, to) in inherit {
            spawn.inherit(from, to);
        }
        let error = spawn.spawn().expect_err("refused");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "{inherit:?}"
        );
    }
}
