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
                        atime: Duration::new(1_760_000_000, 1),
                        mtime: Duration::new(1_760_000_000, 2),
                        ctime: Duration::new(1_760_000_000, 3),
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
        Request::Symlink {
            parent: 1,
            name: b"link.go".to_vec(),
            target: b"a.go".to_vec(),
        },
        Request::Readlink { inode: 7 },
        Request::Getattr {
            inode: 2,
            handle: None,
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
            atime: Duration::new(1_760_000_000, 999_999_999),
            mtime: Duration::new(1_760_000_001, 0),
            ctime: Duration::new(1_760_000_002, 1),
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
            entries: vec![(dirent.clone(), entry)],
        },
        // Listings of more than one entry, and none: every entry survives.
        Reply::Dir {
            entries: (0..3)
                .map(|i| DirEntry {
                    inode: 10 + i,
                    offset: i + 1,
                    kind: DT_REG,
                    name: format!("f{i}.go").into_bytes(),
                })
                .collect(),
        },
        Reply::DirPlus {
            entries: vec![
                (dirent.clone(), entry),
                (
                    DirEntry {
                        name: b"lib".to_vec(),
                        offset: 2,
                        ..dirent
                    },
                    entry,
                ),
            ],
        },
        Reply::Dir {
            entries: Vec::new(),
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
    assert!(header_len < 100, "the bytes stay out of the header");
    assert_eq!(wire.len(), 8 + header_len + payload_len);
    assert_eq!(&wire[8 + header_len..], &[0xAB; 1000][..]);

    let mut wire = Vec::new();
    write_reply(&mut wire, &Reply::Data { data: vec![7; 64] }).unwrap();
    assert_eq!(u32::from_le_bytes(wire[4..8].try_into().unwrap()), 64);
}

/// The header and payload lengths of the one frame `wire` holds.
fn lengths(wire: &[u8]) -> (usize, usize) {
    (
        u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize,
        u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize,
    )
}

/// One frame with `header` and `payload`, as the peer would write it.
fn frame(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    wire.extend_from_slice(&(header.len() as u32).to_le_bytes());
    wire.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    wire.extend_from_slice(header);
    wire.extend_from_slice(payload);
    wire
}

#[test]
fn the_header_is_one_capnp_share_message() {
    let mut wire = Vec::new();
    write_request(
        &mut wire,
        &Request::Lookup {
            parent: 1,
            name: b"a.go".to_vec(),
        },
    )
    .unwrap();
    let (header_len, _) = lengths(&wire);
    let mut header = &wire[8..8 + header_len];
    let message = capnp::serialize::read_message_from_flat_slice(
        &mut header,
        capnp::message::ReaderOptions::new(),
    )
    .expect("the header is a Cap'n Proto message");
    assert!(header.is_empty(), "the message is the whole header");
    let root = message
        .get_root::<leyline_public_schema::share_capnp::request::Reader<'_>>()
        .unwrap();
    match root.which().unwrap() {
        leyline_public_schema::share_capnp::request::Which::Lookup(lookup) => {
            let lookup = lookup.unwrap();
            assert_eq!(lookup.get_parent(), 1);
            assert_eq!(lookup.get_name().unwrap(), b"a.go");
        }
        _ => panic!("the header names a lookup"),
    }
}

#[test]
fn a_header_that_is_not_a_share_message_is_refused() {
    for header in [&b"{\"op\":\"init\"}"[..], b"", &[0xff; 16][..]] {
        let error = read_request(&mut Cursor::new(frame(header, &[]))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{header:?}");
        let error = read_reply(&mut Cursor::new(frame(header, &[]))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{header:?}");
    }
}

#[test]
fn bytes_after_the_header_message_are_refused() {
    let mut wire = Vec::new();
    write_request(&mut wire, &Request::Init).unwrap();
    let (header_len, _) = lengths(&wire);
    let mut header = wire[8..8 + header_len].to_vec();
    header.extend_from_slice(&[0; 8]);
    let error = read_request(&mut Cursor::new(frame(&header, &[]))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

/// A one-segment message whose root struct has a one-word data section
/// holding `tag` at bit `at` and one null pointer: what a peer whose schema
/// has a union member this side lacks would send. `capnp compile -ocapnp
/// share.capnp` places Request's tag at bits [0, 16) and Reply's at [32, 48).
fn root_with_tag(tag: u16, at: u32) -> Vec<u8> {
    let mut message = Vec::new();
    message.extend_from_slice(&0u32.to_le_bytes()); // one segment
    message.extend_from_slice(&3u32.to_le_bytes()); // of three words
    let root: u64 = (1 << 32) | (1 << 48); // struct, offset 0, 1 data word, 1 pointer
    message.extend_from_slice(&root.to_le_bytes());
    message.extend_from_slice(&(u64::from(tag) << at).to_le_bytes());
    message.extend_from_slice(&0u64.to_le_bytes());
    message
}

const REQUEST_TAG_AT: u32 = 0;
const REPLY_TAG_AT: u32 = 32;

#[test]
fn a_request_or_reply_kind_this_side_does_not_know_is_refused() {
    let unknown = root_with_tag(999, REQUEST_TAG_AT);
    let error = read_request(&mut Cursor::new(frame(&unknown, &[]))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let unknown = root_with_tag(999, REPLY_TAG_AT);
    let error = read_reply(&mut Cursor::new(frame(&unknown, &[]))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    // The same bytes with a known kind parse: the refusal is the unknown
    // kind, not the hand-built message.
    let init = root_with_tag(0, REQUEST_TAG_AT);
    assert_eq!(
        read_request(&mut Cursor::new(frame(&init, &[]))).unwrap(),
        Some(Request::Init)
    );
    let done = root_with_tag(8, REPLY_TAG_AT);
    assert_eq!(
        read_reply(&mut Cursor::new(frame(&done, &[]))).unwrap(),
        Reply::Done
    );
}

#[test]
fn a_payload_on_a_kind_that_carries_none_is_refused() {
    let mut wire = Vec::new();
    write_request(&mut wire, &Request::Flush { handle: 1 }).unwrap();
    let (header_len, _) = lengths(&wire);
    let header = wire[8..8 + header_len].to_vec();
    let error = read_request(&mut Cursor::new(frame(&header, b"stray"))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let mut wire = Vec::new();
    write_reply(&mut wire, &Reply::Done).unwrap();
    let (header_len, _) = lengths(&wire);
    let header = wire[8..8 + header_len].to_vec();
    let error = read_reply(&mut Cursor::new(frame(&header, b"stray"))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn a_time_with_a_nanosecond_count_of_a_second_or_more_is_refused() {
    let mut message = capnp::message::Builder::new_default();
    {
        let root = message.init_root::<leyline_public_schema::share_capnp::reply::Builder<'_>>();
        let mut attr = root.init_attr();
        let mut ttl = attr.reborrow().init_ttl();
        ttl.set_secs(1);
        ttl.set_nanos(1_000_000_000);
        attr.init_attr();
    }
    let mut header = Vec::new();
    capnp::serialize::write_message(&mut header, &message).unwrap();
    let error = read_reply(&mut Cursor::new(frame(&header, &[]))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn a_remote_service_answering_a_malformed_reply_fails_eio_from_then_on() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let remote = RemoteFs::new(client);
    let answer = std::thread::spawn(move || {
        read_request(&mut server).unwrap().expect("a request");
        server
            .write_all(&frame(b"not a share message", &[]))
            .unwrap();
        server
    });
    assert_eq!(
        remote.call(Request::Init),
        Reply::Error { errno: LINUX_EIO }
    );
    let _server = answer.join().unwrap();
    assert_eq!(
        remote.call(Request::Init),
        Reply::Error { errno: LINUX_EIO },
        "a stream that returned a malformed reply stays failed"
    );
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
fn a_reader_error_other_than_an_interrupt_is_returned() {
    /// Fails every read the same way.
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::ConnectionReset.into())
        }
    }
    let error = read_request(&mut Broken).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
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
    // A broken exchange fails the test instead of hanging it.
    for end in [&near, &far] {
        end.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    }
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
