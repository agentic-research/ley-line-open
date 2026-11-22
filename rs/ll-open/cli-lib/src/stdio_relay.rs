//! Standard output and error that no write can fail on with EPIPE (bead
//! `ley-line-open-f6b525`).
//!
//! A daemon started with `--lifeline-fd` outlives the launcher that may be
//! the only reader of its standard output and error (mache sets
//! `cmd.Stderr` to a pipe it reads). Once that reader is gone, every write
//! to the pipe fails with EPIPE (Rust ignores SIGPIPE), and `eprintln!`
//! panics: on whichever thread wrote, holding whatever it held. A panic that
//! poisons a lock the shutdown needs costs the final publish.
//!
//! [`install`] makes that failure unreachable rather than racing it: each
//! descriptor is replaced (`dup2`, atomic for every thread) by the write end
//! of a pipe whose reader is a relay thread in this process, so a write
//! never meets a pipe without a reader. The relay forwards every byte to the
//! original descriptor and, once a forward fails, discards the rest. It is
//! transparent to the launcher while the launcher reads.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// The relays [`install`] started; [`StdioRelay::finish`] hands each
/// descriptor back.
#[must_use = "finish() flushes what the relay holds before the process exits"]
pub struct StdioRelay {
    streams: Vec<Relayed>,
    /// Opened at install, so handing a descriptor to `/dev/null` at finish
    /// has no failure path that falls back to a reader-less original.
    null: File,
}

struct Relayed {
    fd: RawFd,
    original: OwnedFd,
    /// Set once forwarding to `original` failed: its reader is gone.
    dead: Arc<AtomicBool>,
    /// Sent when the relay thread has forwarded everything and seen
    /// end-of-file.
    drained: mpsc::Receiver<()>,
}

/// Relay each of `fds` (standard output and error, for the daemon) through
/// a pipe this process reads. Call it before anything writes to them that
/// could outlive their reader.
pub fn install(fds: &[RawFd]) -> io::Result<StdioRelay> {
    let null = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .map_err(|error| io::Error::new(error.kind(), format!("open /dev/null: {error}")))?;
    let mut streams = Vec::with_capacity(fds.len());
    for &fd in fds {
        // A close-on-exec copy (F_DUPFD_CLOEXEC): no child gets the original
        // through it. A number that is not open fails here.
        // SAFETY: borrowed for the one duplication; a closed number answers
        // EBADF.
        let original = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
        let target = File::from(original.try_clone()?);
        let (mut reader, writer) = io::pipe()?;
        // SAFETY: dup2 onto `fd` replaces it atomically for every thread.
        checked(unsafe { libc::dup2(writer.as_raw_fd(), fd) })?;
        drop(writer);
        let dead = Arc::new(AtomicBool::new(false));
        let (done, drained) = mpsc::channel();
        let relay_dead = Arc::clone(&dead);
        std::thread::Builder::new()
            .name("stdio-relay".into())
            .spawn(move || relay(&mut reader, target, &relay_dead, &done))?;
        streams.push(Relayed {
            fd,
            original,
            dead,
            drained,
        });
    }
    Ok(StdioRelay { streams, null })
}

/// Whether `fd` is the write end of a pipe or socket whose reader is gone:
/// `poll` reports POLLHUP for it on macOS and POLLERR on Linux. A terminal,
/// a file or a live pipe reports neither.
fn reader_hung_up(fd: RawFd) -> bool {
    let mut entry = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one pollfd this frame owns, a zero timeout; poll writes only
    // its revents.
    let ready = unsafe { libc::poll(&mut entry, 1, 0) };
    ready > 0 && entry.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// A libc return value: `-1` is the error in `errno`, anything else success.
fn checked(rc: libc::c_int) -> io::Result<libc::c_int> {
    match rc {
        -1 => Err(io::Error::last_os_error()),
        rc => Ok(rc),
    }
}

/// Forward everything `reader` yields to `target` until end-of-file; once a
/// forward fails, read and discard instead. Never writes anywhere else.
fn relay(
    reader: &mut impl Read,
    mut target: impl Write,
    dead: &AtomicBool,
    done: &mpsc::Sender<()>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if !dead.load(Ordering::SeqCst) && target.write_all(&buffer[..read]).is_err() {
                    dead.store(true, Ordering::SeqCst);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = done.send(());
}

impl StdioRelay {
    /// [`finish`](Self::finish) after `leyline daemon` returns: the
    /// launcher is gone when its lifeline ended in this process
    /// ([`crate::lifecycle::lifeline_ended`]).
    pub fn finish_after_daemon(self, bound: Duration) {
        self.finish(bound, crate::lifecycle::lifeline_ended());
    }

    /// Hand each descriptor back and flush what its relay holds, so no
    /// later write can fail either. A descriptor goes to `/dev/null` when
    /// its original's reader is or may be gone: `launcher_gone` (the
    /// launcher's lifeline ended, and it may have been the only reader,
    /// written to since or not), a forward already failed, or the original
    /// reports its reader hung up. Otherwise it goes back to the original.
    /// A reader that dies after this check is not seen. Waits for each relay
    /// to forward what was written before, at most `bound` (a child that
    /// inherited the relay's pipe keeps it open past that).
    pub fn finish(self, bound: Duration, launcher_gone: bool) {
        for stream in &self.streams {
            let reader_gone = launcher_gone
                || stream.dead.load(Ordering::SeqCst)
                || reader_hung_up(stream.original.as_raw_fd());
            let target = if reader_gone {
                self.null.as_raw_fd()
            } else {
                stream.original.as_raw_fd()
            };
            // SAFETY: dup2 onto the relayed descriptor; this drops this
            // process's last copy of the relay's write end.
            unsafe { libc::dup2(target, stream.fd) };
        }
        for stream in self.streams {
            let _ = stream.drained.recv_timeout(bound);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd, IntoRawFd};

    const BOUND: Duration = Duration::from_secs(10);

    fn write(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: a borrowed descriptor this test owns; ManuallyDrop keeps
        // the File from closing it.
        let mut file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        file.write_all(bytes)
    }

    /// Bytes reach the original reader while it reads; once it is gone no
    /// write fails; after finish, writes go to `/dev/null`.
    #[test]
    fn a_relayed_descriptor_never_fails_once_its_reader_is_gone() {
        let (mut reader, writer) = io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        let relay = install(&[fd]).unwrap();
        write(fd, b"hello\n").unwrap();
        let mut line = [0u8; 6];
        reader.read_exact(&mut line).unwrap();
        assert_eq!(&line, b"hello\n");
        drop(reader);
        for _ in 0..1000 {
            write(fd, b"no reader, and no EPIPE\n").expect("a relayed write never fails");
        }
        relay.finish(BOUND, false);
        // SAFETY: an out-parameter of the size fstat writes.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0);
        assert_eq!(
            stat.st_mode & libc::S_IFMT,
            libc::S_IFCHR,
            "finished onto /dev/null, the original's reader being gone"
        );
        write(fd, b"after finish\n").expect("finished onto /dev/null");
        // SAFETY: the test's own descriptor.
        unsafe { libc::close(fd) };
    }

    /// While the original reader lives, finish hands the descriptor back to
    /// it, after the relay has forwarded what was written before.
    #[test]
    fn finish_hands_a_live_descriptor_back_after_forwarding_everything() {
        let (mut reader, writer) = io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        let relay = install(&[fd]).unwrap();
        write(fd, b"before\n").unwrap();
        relay.finish(BOUND, false);
        write(fd, b"after\n").unwrap();
        // SAFETY: the test's own descriptor; closing it ends the pipe.
        unsafe { libc::close(fd) };
        let mut all = String::new();
        reader.read_to_string(&mut all).unwrap();
        assert_eq!(all, "before\nafter\n");
    }

    /// A reader that died without a write through the relay leaves it
    /// unaware: finish must not hand back the dead original after the
    /// launcher's lifeline ended, or a teardown write fails with EPIPE.
    #[test]
    fn after_the_lifeline_ends_finish_never_hands_back_a_reader_less_pipe() {
        let (reader, writer) = io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        let relay = install(&[fd]).unwrap();
        // The launcher's reader dies, and nothing is written meanwhile.
        drop(reader);
        relay.finish(BOUND, true);
        write(fd, b"teardown\n").expect("a teardown write after the launcher died");
        // SAFETY: the test's own descriptor.
        unsafe { libc::close(fd) };
    }

    /// A reader that died silently is seen at finish even when the stop was
    /// not a lifeline end (a SIGTERM whose launcher died too): the original
    /// reports its reader hung up, so the descriptor goes to `/dev/null`.
    #[test]
    fn finish_never_hands_back_a_pipe_whose_reader_hung_up() {
        let (reader, writer) = io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        let relay = install(&[fd]).unwrap();
        drop(reader);
        relay.finish(BOUND, false);
        write(fd, b"teardown\n").expect("a reader that hung up is never handed back");
        // SAFETY: the test's own descriptor.
        unsafe { libc::close(fd) };
    }

    /// What `leyline daemon` calls: after a lifeline end, a descriptor whose
    /// reader is still alive at finish but dies afterwards (the probe cannot
    /// see it yet) still goes to `/dev/null`, so the later write succeeds.
    #[test]
    fn after_a_lifeline_end_the_daemon_finish_hands_no_descriptor_back() {
        let _flag = crate::lifecycle::LIFELINE_FLAG_TESTS.blocking_lock();
        crate::lifecycle::set_lifeline_ended_for_test(true);
        let (reader, writer) = io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        let relay = install(&[fd]).unwrap();
        relay.finish_after_daemon(BOUND);
        crate::lifecycle::set_lifeline_ended_for_test(false);
        drop(reader);
        write(fd, b"teardown\n").expect("finished onto /dev/null after the lifeline ended");
        // SAFETY: the test's own descriptor.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn a_descriptor_that_is_not_open_is_refused_and_nothing_is_relayed() {
        assert!(install(&[1 << 20]).is_err());
    }

    #[test]
    fn checked_is_an_error_only_for_minus_one() {
        assert!(checked(-1).is_err());
        assert_eq!(checked(0).unwrap(), 0);
        assert_eq!(checked(7).unwrap(), 7);
    }

    /// An interrupted read is retried; any other read error ends the relay.
    #[test]
    fn the_relay_retries_an_interrupted_read_and_ends_on_any_other_error() {
        struct Script(Vec<io::Result<&'static [u8]>>);
        impl Read for Script {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                match self.0.remove(0) {
                    Ok(chunk) => {
                        buffer[..chunk.len()].copy_from_slice(chunk);
                        Ok(chunk.len())
                    }
                    Err(error) => Err(error),
                }
            }
        }
        let dead = AtomicBool::new(false);
        let (done, drained) = mpsc::channel();
        let mut forwarded = Vec::new();
        relay(
            &mut Script(vec![
                Ok(b"a"),
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(b"b"),
                Err(io::Error::from(io::ErrorKind::Other)),
                Ok(b"never read"),
            ]),
            &mut forwarded,
            &dead,
            &done,
        );
        assert_eq!(forwarded, b"ab");
        assert!(!dead.load(Ordering::SeqCst));
        drained.try_recv().expect("the relay reports it ended");
    }

    #[test]
    fn a_failed_forward_marks_the_relay_dead_and_discards_the_rest() {
        struct Broken(usize);
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let dead = AtomicBool::new(false);
        let (done, drained) = mpsc::channel();
        // Three reads, so a relay that kept forwarding would try three times.
        struct Chunks(Vec<&'static [u8]>);
        impl Read for Chunks {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let chunk = self.0.remove(0);
                buffer[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }
        let mut target = Broken(0);
        relay(
            &mut Chunks(vec![b"one ", b"two ", b"three"]),
            &mut target,
            &dead,
            &done,
        );
        assert!(dead.load(Ordering::SeqCst));
        assert_eq!(target.0, 1, "one failed forward, then discarded");
        drained.try_recv().expect("the relay reports it drained");
    }
}
