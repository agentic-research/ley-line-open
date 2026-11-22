//! Every daemon spawn hands a child exactly what it names (ADR-0046 Slice 1
//! S7; bead `ley-line-open-528525`).
//!
//! N children are spawned concurrently, each handed the write end of a pipe
//! of its own. Every write end is held WITHOUT close-on-exec while the
//! children are spawned, the worst case of the macOS window between `pipe`
//! and `fcntl(FD_CLOEXEC)`: a spawn that let anything unnamed cross the exec
//! would hand every sibling's write end to every child. Once every child has
//! exec'd, each write end is held only by this process and its own child,
//! and a descriptor this process holds without close-on-exec, and names to
//! no child, is absent in every child.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Barrier};

use leyline_runtime::backends::spawn::{ChildIo, Spawn, Spawned};

mod common;

const CHILDREN: usize = 16;
/// The number each child holds its write end at.
const CHILD_FD: RawFd = 3;
/// The lowest number the unnamed descriptor is placed at: far from
/// [`CHILD_FD`], so a child's `dup2` onto [`CHILD_FD`] never replaces it and
/// its absence in a child is the spawn's doing.
const STRAY_MIN_FD: RawFd = 64;

/// An identifier for one END of the pipe `fd` is, the same in every process
/// that holds that end. macOS: `lsof`'s address for the end, which differs
/// between the two ends of one pipe. Linux: `/proc/<pid>/fd/<fd>` reads
/// `pipe:[inode]` for BOTH ends, so the access mode from `fdinfo` is added:
/// without it, this process's read end of a pipe counts as a holder of its
/// write end.
fn pipe_id(pid: u32, fd: RawFd) -> String {
    pipe_id_of(pid, fd).unwrap_or_else(|| panic!("no pipe at descriptor {fd} in {pid}"))
}

/// [`pipe_id`], or `None` when `pid` holds no pipe at `fd`.
fn pipe_id_of(pid: u32, fd: RawFd) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let link = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
        let link = link.display().to_string();
        if !link.starts_with("pipe:") {
            return None;
        }
        let info = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| i32::from_str_radix(flags.trim(), 8).ok())?;
        Some(format!("{link} accmode={}", flags & libc::O_ACCMODE))
    }
    #[cfg(target_os = "macos")]
    {
        descriptors(&[pid])
            .into_iter()
            .find(|&(_, at, _)| at == fd)
            .and_then(|(_, _, pipe)| pipe)
    }
}

/// Every (pid, fd) among `pids` holding the pipe end `id`.
fn holders(id: &str, pids: &[u32]) -> Vec<(u32, RawFd)> {
    descriptors(pids)
        .into_iter()
        .filter(|(_, fd, pipe)| *fd > 2 && pipe.as_deref() == Some(id))
        .map(|(pid, fd, _)| (pid, fd))
        .collect()
}

/// Every numbered descriptor of `pids`, with its pipe-end identifier
/// ([`pipe_id`]) when it is one end of a pipe.
fn descriptors(pids: &[u32]) -> Vec<(u32, RawFd, Option<String>)> {
    #[cfg(target_os = "linux")]
    {
        let mut all = Vec::new();
        for &pid in pids {
            for fd in open_fds(pid) {
                all.push((pid, fd, pipe_id_of(pid, fd)));
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

/// Clear `fd`'s close-on-exec flag.
fn clear_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a descriptor the caller owns; no memory is passed.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0, "F_GETFD {fd}");
        assert!(libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0);
    }
}

/// Wait until `child` (a `cat` on piped stdin and stdout) has exec'd: it
/// echoes a line only once it runs `cat`, after its exec. Every descriptor
/// it holds from then on crossed that exec.
fn await_exec(child: &mut Spawned) {
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"exec'd\n")
        .unwrap();
    let mut echo = [0u8; 7];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut echo)
        .unwrap();
    assert_eq!(&echo, b"exec'd\n", "child {}", child.id());
}

#[test]
fn concurrent_spawns_each_inherit_only_their_own_write_end() {
    let _serial = common::serial();
    // A descriptor this process holds WITHOUT close-on-exec and names to no
    // child: no child may hold it.
    let stray = {
        let null = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: F_DUPFD duplicates a live descriptor; the result is owned
        // below. F_DUPFD (not F_DUPFD_CLOEXEC) leaves close-on-exec clear.
        let raw = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_DUPFD, STRAY_MIN_FD) };
        assert!(raw >= STRAY_MIN_FD, "F_DUPFD");
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        unsafe { OwnedFd::from_raw_fd(raw) }
    };
    let barrier = Arc::new(Barrier::new(CHILDREN));
    let mut spawned: Vec<(Spawned, OwnedFd, OwnedFd)> = (0..CHILDREN)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let (reader, writer) = std::io::pipe().unwrap();
                let (reader, writer) = (OwnedFd::from(reader), OwnedFd::from(writer));
                // Every write end crosses an exec unless the spawn itself
                // keeps it from doing so.
                clear_cloexec(writer.as_raw_fd());
                barrier.wait();
                let mut spawn = Spawn::new("/bin/cat");
                spawn
                    .stdin(ChildIo::Piped)
                    .stdout(ChildIo::Piped)
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

    // Sample only once every child has exec'd. Until a child execs, it is a
    // fork of this process and holds a copy of every descriptor this process
    // held at the fork, its siblings' write ends among them (on Linux, where
    // the spawn forks; see `backends::spawn`). That copy closes at the exec.
    for (child, _, _) in &mut spawned {
        await_exec(child);
    }

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
            !in_child.contains(&stray.as_raw_fd()),
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
