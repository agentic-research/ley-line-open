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
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// The relays [`install`] started; [`StdioRelay::finish`] hands each
/// descriptor back.
#[must_use = "finish() flushes what the relay holds before the process exits"]
pub struct StdioRelay {
    streams: Vec<Relayed>,
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
    let mut streams = Vec::with_capacity(fds.len());
    for &fd in fds {
        // SAFETY: fcntl on a descriptor number; failure is reported. The
        // copy is close-on-exec: no child gets the original through it.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl just returned this descriptor, owned here.
        let original = unsafe { OwnedFd::from_raw_fd(copy) };
        let target = File::from(original.try_clone()?);
        let (mut reader, writer) = io::pipe()?;
        // SAFETY: dup2 onto `fd` replaces it atomically for every thread.
        if unsafe { libc::dup2(writer.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
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
    Ok(StdioRelay { streams })
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
    /// Hand each descriptor back and flush what its relay holds: the
    /// original when it still has a reader, `/dev/null` when it does not, so
    /// no later write can fail either. Waits for each relay to forward what
    /// was written before, at most `bound` (a child that inherited the
    /// relay's pipe keeps it open past that).
    pub fn finish(self, bound: Duration) {
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null");
        for stream in &self.streams {
            let target = match (&null, stream.dead.load(Ordering::SeqCst)) {
                (Ok(null), true) => null.as_raw_fd(),
                _ => stream.original.as_raw_fd(),
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
    use std::os::fd::IntoRawFd;

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
        relay.finish(BOUND);
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
        relay.finish(BOUND);
        write(fd, b"after\n").unwrap();
        // SAFETY: the test's own descriptor; closing it ends the pipe.
        unsafe { libc::close(fd) };
        let mut all = String::new();
        reader.read_to_string(&mut all).unwrap();
        assert_eq!(all, "before\nafter\n");
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
