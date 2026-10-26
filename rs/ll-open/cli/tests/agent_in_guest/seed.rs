// The agent-in-guest end-to-end test's source tree and the edits made to
// it. Compiled into the host test and, prepended as `mod seed`, into the
// guest program: both sides name the same bytes.
#![allow(dead_code)]

pub const A: &str = "package a\n\nfunc A() int { return 1 }\n";
/// (b): the guest's edit through open/write/close.
pub const A_EDIT: &str = "package a\n\nfunc A() int { return 2 }\n";
/// (e): the guest's save through a temp file and rename(2).
pub const A_RENAMED: &str =
    "package a\n\n// saved through a temp file and rename(2)\nfunc A() int { return 3 }\n";
pub const A_TEMP: &str = "a.go.tmp.4242";
pub const B: &str = "package a\n\nfunc B() int { return 1 }\n";
/// (c): the socket proposal that moves the file the guest holds open; it
/// changes the function, not the first line.
pub const B_HOST: &str = "package a\n\n// proposed over the socket\nfunc B() int { return 2 }\n";
/// (c): the guest's edit of the first line only, from the bytes it read.
pub const B_GUEST_TOP: &str = "package a // edited in the guest\n\nfunc B() int { return 1 }\n";
/// (c): the two, merged (ADR-0045 T5).
pub const B_MERGED: &str =
    "package a // edited in the guest\n\n// proposed over the socket\nfunc B() int { return 2 }\n";
/// (C): the host and the guest both change the merged function's return.
pub const B_HOST_AGAIN: &str =
    "package a // edited in the guest\n\n// proposed over the socket\nfunc B() int { return 4 }\n";
pub const B_GUEST_SAME: &str =
    "package a // edited in the guest\n\n// proposed over the socket\nfunc B() int { return 9 }\n";
pub const C: &str = "package a\n\nfunc C() int { return 1 }\n";
/// (d): a half-written edit; it lands, with a diagnostic.
pub const C_BROKEN: &str = "package a\n\nfunc C( {\n";
/// (p): files in no projected language, a mode, and a directory rename.
pub const README_EDIT: &str = "# agent in guest\n\nedited in the guest\n";
pub const NOTES: &str = "remember: the store holds every file\n";
pub const RUN_SH: &str = "#!/bin/sh\necho run\n";
pub const LIB_X: &str = "package pkg\n\nfunc X() int { return 1 }\n";
/// (e): deleted by the guest.
pub const D: &str = "package a\n\nfunc D() int { return 1 }\n";
/// (e): created by the guest.
pub const N: &str = "package a\n\nfunc N() int { return 7 }\n";
pub const G: &str = "package a\n\nfunc G() int { return 1 }\n";
/// (g): a host proposal the guest sees at its next open; longer than `G`.
pub const G_HOST: &str =
    "package a\n\n// a host proposal the guest sees at its next open\nfunc G() int { return 2 }\n";
/// (g): a file the guest only stats; the host grows it.
pub const T: &str = "package a\n\nfunc T() int { return 1 }\n";
pub const T_HOST: &str = "package a\n\n// grown by a host proposal\nfunc T() int { return 2 }\n";
/// (m): two successive same-size edits git status must each report
/// (bead `ley-line-open-b8100d`).
pub const M: &str = "package a\n\nfunc M() int { return 1 }\n";
pub const M_ONE: &str = "package a\n\nfunc M() int { return 2 }\n";
pub const M_TWO: &str = "package a\n\nfunc M() int { return 3 }\n";
pub const README: &str = "# agent in guest\n";
/// (o): build output the guest writes under target/ and node_modules/,
/// which reaches neither the store nor the checkout (ADR-0045 T9).
pub const OUTPUT_FILES: usize = 300;
pub const LEFT_PAD: &str = "module.exports = (s, n) => s.padStart(n);\n";

/// The bytes of (o)'s `i`th object file.
pub fn output(i: usize) -> Vec<u8> {
    format!("object {i}\n").repeat(16).into_bytes()
}

/// The host's go-ahead file and the guest's step file: equal-length tokens,
/// so a cached size never truncates a token.
pub const HOST_SYNC: &str = "sync/host.md";
pub const GUEST_SYNC: &str = "sync/guest.md";
pub const REPORT: &str = "report.md";

/// The share's settled entry and attribute timeout for this test
/// (`--workspace-ttl-ms`), and the bound (g) checks.
pub const SETTLED_MS: u64 = 3_000;
pub const CHANGED_MS: u64 = 500;

/// The files of the tree, as the daemon parses it.
pub const TREE: [(&str, &str); 10] = [
    ("a.go", A),
    ("b.go", B),
    ("c.go", C),
    ("d.go", D),
    ("g.go", G),
    ("t.go", T),
    ("m.go", M),
    ("README.md", README),
    (HOST_SYNC, "go-0\n"),
    (GUEST_SYNC, "st-0\n"),
];
