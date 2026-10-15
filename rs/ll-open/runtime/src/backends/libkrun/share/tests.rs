use std::io::Cursor;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use super::*;

/// Answers every request with a reply derived from it, and records what it
/// received.
#[derive(Default)]
struct Echo {
    seen: Mutex<Vec<Request>>,
}

impl FsService for Echo {
    fn call(&self, request: Request) -> Reply {
        self.seen.lock().push(request.clone());
        match request {
            Request::Read { size, .. } => Reply::Data {
                data: (0..size).map(|i| i as u8).collect(),
            },
            Request::Write { data, .. } => Reply::Written {
                size: data.len() as u32,
            },
            Request::Lookup { parent, .. } => Reply::Entry {
                entry: Entry {
                    inode: parent + 1,
                    attr: Attr {
                        inode: parent + 1,
                        mode: 0o100644,
                        nlink: 1,
                        size: 3,
                        blocks: 1,
                        blksize: 4096,
                    },
                    entry_ttl: Duration::from_millis(1500),
                    attr_ttl: Duration::from_millis(250),
                },
            },
            _ => Reply::Error { errno: 116 },
        }
    }
}

fn every_request() -> Vec<Request> {
    vec![
        Request::Init,
        Request::Lookup {
            parent: 1,
            name: b"a.go".to_vec(),
        },
        Request::Getattr {
            inode: 2,
            handle: Some(3),
        },
        Request::Setattr {
            inode: 2,
            handle: None,
            size: Some(0),
            mode: Some(0o600),
            uid: Some(5),
            gid: Some(6),
        },
        Request::Mkdir {
            parent: 1,
            name: b"d".to_vec(),
        },
        Request::Unlink {
            parent: 1,
            name: b"x".to_vec(),
        },
        Request::Rmdir {
            parent: 1,
            name: b"d".to_vec(),
        },
        Request::Rename {
            olddir: 1,
            oldname: b"a".to_vec(),
            newdir: 4,
            newname: b"b".to_vec(),
            flags: 1,
        },
        Request::Open { inode: 2, flags: 2 },
        Request::Create {
            parent: 1,
            name: b"n.go".to_vec(),
            flags: 0o102,
        },
        Request::Read {
            handle: 3,
            offset: 7,
            size: 9,
        },
        Request::Write {
            handle: 3,
            offset: 7,
            data: b"payload\0bytes".to_vec(),
        },
        Request::Flush { handle: 3 },
        Request::Fsync { handle: 3 },
        Request::Release { handle: 3 },
        Request::Opendir { inode: 1 },
        Request::Readdir {
            inode: 1,
            offset: 2,
        },
        Request::Readdirplus {
            inode: 1,
            offset: 2,
        },
    ]
}

fn every_reply() -> Vec<Reply> {
    let entry = Entry {
        inode: 9,
        attr: Attr {
            inode: 9,
            mode: 0o040755,
            nlink: 2,
            size: 0,
            blocks: 0,
            blksize: 4096,
        },
        entry_ttl: Duration::from_secs(60),
        attr_ttl: Duration::from_nanos(1),
    };
    let dirent = DirEntry {
        inode: 9,
        offset: 1,
        kind: DT_DIR,
        name: b"pkg".to_vec(),
    };
    vec![
        Reply::Error { errno: 116 },
        Reply::Init {
            want: FUSE_DO_READDIRPLUS,
        },
        Reply::Entry { entry },
        Reply::Attr {
            attr: entry.attr,
            ttl: Duration::from_secs(1),
        },
        Reply::Opened {
            handle: 4,
            flags: FOPEN_KEEP_CACHE,
        },
        Reply::Created {
            entry,
            handle: 5,
            flags: FOPEN_DIRECT_IO,
        },
        Reply::Data {
            data: b"bytes\0and\xffmore".to_vec(),
        },
        Reply::Written { size: 12 },
        Reply::Done,
        Reply::Dir {
            entries: vec![dirent.clone()],
        },
        Reply::DirPlus {
            entries: vec![(dirent, entry)],
        },
    ]
}

#[test]
fn every_request_and_reply_survives_a_frame_unchanged() {
    let mut wire = Vec::new();
    for request in every_request() {
        write_request(&mut wire, &request).unwrap();
    }
    for reply in every_reply() {
        write_reply(&mut wire, &reply).unwrap();
    }
    let mut reader = Cursor::new(wire);
    for request in every_request() {
        assert_eq!(read_request(&mut reader).unwrap(), Some(request));
    }
    for reply in every_reply() {
        assert_eq!(read_reply(&mut reader).unwrap(), reply);
    }
    assert_eq!(read_request(&mut reader).unwrap(), None);
}

#[test]
fn write_and_read_bytes_travel_as_the_payload_not_the_header() {
    let mut wire = Vec::new();
    write_request(
        &mut wire,
        &Request::Write {
            handle: 1,
            offset: 0,
            data: vec![0xAB; 1000],
        },
    )
    .unwrap();
    let header_len = u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize;
    let payload_len = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
    assert_eq!(payload_len, 1000);
    assert!(header_len < 100, "the bytes stay out of the JSON header");
    assert_eq!(wire.len(), 8 + header_len + payload_len);
    assert_eq!(&wire[8 + header_len..], &[0xAB; 1000][..]);

    let mut wire = Vec::new();
    write_reply(&mut wire, &Reply::Data { data: vec![7; 64] }).unwrap();
    assert_eq!(u32::from_le_bytes(wire[4..8].try_into().unwrap()), 64);
}

#[test]
fn a_stream_cut_inside_a_frame_is_an_error_and_a_clean_end_is_none() {
    let mut wire = Vec::new();
    write_request(&mut wire, &Request::Flush { handle: 1 }).unwrap();
    for cut in 1..wire.len() {
        let mut reader = Cursor::new(wire[..cut].to_vec());
        assert!(
            read_request(&mut reader).is_err(),
            "a frame cut at byte {cut} must not parse"
        );
    }
    assert_eq!(read_request(&mut Cursor::new(Vec::new())).unwrap(), None);
    assert!(read_reply(&mut Cursor::new(Vec::new())).is_err());
}

#[test]
fn a_length_above_the_frame_limit_is_refused_before_allocating() {
    let mut wire = Vec::new();
    wire.extend_from_slice(&(MAX_FRAME + 1).to_le_bytes());
    wire.extend_from_slice(&0u32.to_le_bytes());
    let error = read_request(&mut Cursor::new(wire)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let mut wire = Vec::new();
    wire.extend_from_slice(&2u32.to_le_bytes());
    wire.extend_from_slice(&(MAX_FRAME + 1).to_le_bytes());
    wire.extend_from_slice(b"{}");
    let error = read_request(&mut Cursor::new(wire)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    // At the limit is allowed: the bound is inclusive.
    assert_eq!(frame_len(MAX_FRAME as usize).unwrap(), MAX_FRAME);
    assert!(frame_len(MAX_FRAME as usize + 1).is_err());
}

#[test]
fn a_reader_interrupted_mid_lengths_resumes() {
    /// Yields one byte per read and reports `Interrupted` between them.
    struct Trickle {
        bytes: Vec<u8>,
        at: usize,
        interrupt: bool,
    }
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.at == self.bytes.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.bytes[self.at];
            self.at += 1;
            Ok(1)
        }
    }
    let mut wire = Vec::new();
    write_request(&mut wire, &Request::Opendir { inode: 77 }).unwrap();
    let mut reader = Trickle {
        bytes: wire,
        at: 0,
        interrupt: false,
    };
    assert_eq!(
        read_request(&mut reader).unwrap(),
        Some(Request::Opendir { inode: 77 })
    );
}

#[test]
fn serve_answers_each_request_in_order_until_the_peer_closes() {
    let mut wire = Vec::new();
    write_request(
        &mut wire,
        &Request::Read {
            handle: 1,
            offset: 0,
            size: 4,
        },
    )
    .unwrap();
    write_request(
        &mut wire,
        &Request::Write {
            handle: 1,
            offset: 0,
            data: b"hello".to_vec(),
        },
    )
    .unwrap();
    let echo = Echo::default();
    let mut replies = Vec::new();
    serve(&mut Cursor::new(wire), &mut replies, &echo).unwrap();
    let mut replies = Cursor::new(replies);
    assert_eq!(
        read_reply(&mut replies).unwrap(),
        Reply::Data {
            data: vec![0, 1, 2, 3]
        }
    );
    assert_eq!(
        read_reply(&mut replies).unwrap(),
        Reply::Written { size: 5 }
    );
    assert!(
        read_reply(&mut replies).is_err(),
        "two requests, two replies"
    );
    assert_eq!(echo.seen.lock().len(), 2);
}

#[test]
fn a_remote_service_answers_across_a_socket_as_the_served_one_does() {
    let (near, far) = UnixStream::pair().unwrap();
    let echo = Arc::new(Echo::default());
    let served = Arc::clone(&echo);
    let server = std::thread::spawn(move || {
        let mut reader = &far;
        let mut writer = &far;
        serve(&mut reader, &mut writer, served.as_ref())
    });
    let remote = RemoteFs::new(near);
    for request in every_request() {
        assert_eq!(remote.call(request.clone()), echo.call(request));
    }
    drop(remote);
    server.join().unwrap().unwrap();
    // Each request reached the served service once, and the test's direct
    // call once.
    assert_eq!(echo.seen.lock().len(), every_request().len() * 2);
}

#[test]
fn a_remote_service_whose_peer_is_gone_answers_eio_from_then_on() {
    let (near, far) = UnixStream::pair().unwrap();
    drop(far);
    let remote = RemoteFs::new(near);
    assert_eq!(
        remote.call(Request::Flush { handle: 1 }),
        Reply::Error { errno: LINUX_EIO }
    );
    assert!(remote.stream.lock().is_none(), "a failed stream is dropped");
    assert_eq!(
        remote.call(Request::Init),
        Reply::Error { errno: LINUX_EIO }
    );
}

#[test]
fn fuse_constants_are_the_linux_uapi_values() {
    assert_eq!(FUSE_DO_READDIRPLUS, 8192);
    assert_eq!(FUSE_WRITEBACK_CACHE, 65536);
    assert_eq!(
        (FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE, FOPEN_CACHE_DIR),
        (1, 2, 8)
    );
    assert_eq!((DT_DIR, DT_REG, LINUX_EIO), (4, 8, 5));
}
