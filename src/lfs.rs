//! A Git LFS *clean* filter for the evidence service's isolated Git (issue #150).
//!
//! The isolated runner disables system and global config, which is where `git lfs install` puts
//! `filter.lfs.*`. Without it, Git reads a hydrated LFS file in the working tree as its raw payload
//! rather than as the pointer the commit holds, so a clean checkout of a committed LFS change diffs
//! as a binary rewrite and the converged-hook binding fails with `head_does_not_match_review`.
//!
//! Forwarding the user's own `filter.lfs.process` would run whatever program that config names, and
//! `git-lfs clean` also writes the payload into `.git/lfs/objects` -- a write from a read-only
//! service. So when the user's Git *would* apply an LFS filter, the runner registers this binary as
//! the filter instead: it computes the pointer `git-lfs clean` would produce (SHA-256 and size) and
//! writes nothing. A file that is already a pointer -- a pointer-only checkout -- passes through
//! unchanged, as `git-lfs clean` passes it. Only `clean` is implemented: the service never checks
//! anything out.
//!
//! It is the *long-running* (`process`) protocol rather than a per-file `clean` command because an
//! empty `filter.lfs.process=` does not disable a repository-level `process` entry -- Git then fails
//! the filter instead of falling back to `clean` (verified with Git 2.55) -- and only a `process` of
//! our own reliably overrides one. It also costs one spawn per Git command rather than one per file.

use std::io::{self, BufRead, BufReader, Write};

/// Hidden argv flag selecting filter mode; handled in `main` before any configuration is read.
pub const FILTER_FLAG: &str = "--lfs-filter-process";

/// What `git-lfs` writes as the first pointer line.
const POINTER_VERSION: &str = "version https://git-lfs.github.com/spec/v1";

/// `git-lfs` only treats content shorter than this as a possible pointer.
const POINTER_MAX: usize = 1024;

/// The largest pkt-line payload (`LARGE_PACKET_DATA_MAX`).
const PKT_MAX: usize = 65516;

/// The `filter.lfs.process` value naming this executable in filter mode, or `None` when its path
/// cannot be quoted for the shell Git runs filters through (a `'` in it). `None` leaves the
/// runner as it was before issue #150: noisier for a hydrated LFS file, never thinner.
pub fn filter_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str()?.replace('\\', "/");
    (!exe.contains('\'')).then(|| format!("'{exe}' {FILTER_FLAG}"))
}

/// Filter mode's entry point: serve Git's filter protocol on stdin/stdout. Returns the exit code.
pub fn serve_stdio() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    match serve(&mut BufReader::new(stdin.lock()), &mut stdout.lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cross-review: LFS filter failed: {e}");
            1
        }
    }
}

/// One pkt-line: a data packet, or a flush (`0000`).
enum Pkt {
    Data(Vec<u8>),
    Flush,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Read one pkt-line, or `None` at a clean end of input between packets.
fn read_pkt(r: &mut impl BufRead) -> io::Result<Option<Pkt>> {
    let mut len = [0u8; 4];
    if r.fill_buf()?.is_empty() {
        return Ok(None);
    }
    r.read_exact(&mut len)?;
    let len = std::str::from_utf8(&len)
        .ok()
        .and_then(|s| usize::from_str_radix(s, 16).ok())
        .ok_or_else(|| bad("malformed pkt-line length"))?;
    match len {
        0 => Ok(Some(Pkt::Flush)),
        1..=4 => Err(bad(format!("unexpected pkt-line length {len}"))),
        _ => {
            let mut data = vec![0u8; len - 4];
            r.read_exact(&mut data)?;
            Ok(Some(Pkt::Data(data)))
        }
    }
}

fn write_pkt(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write!(w, "{:04x}", data.len() + 4)?;
    w.write_all(data)
}

fn write_text(w: &mut impl Write, line: &str) -> io::Result<()> {
    write_pkt(w, format!("{line}\n").as_bytes())
}

fn write_flush(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"0000")
}

/// Read text packets up to a flush, each with its trailing newline removed.
fn read_list(r: &mut impl BufRead) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
        match read_pkt(r)? {
            Some(Pkt::Data(d)) => {
                let s = String::from_utf8(d).map_err(|_| bad("non-UTF-8 protocol line"))?;
                lines.push(s.strip_suffix('\n').unwrap_or(&s).to_string());
            }
            Some(Pkt::Flush) => return Ok(lines),
            None => return Err(bad("input ended inside a packet list")),
        }
    }
}

/// The filter protocol, version 2 (`gitattributes(5)`, "Long Running Filter Process").
fn serve(r: &mut impl BufRead, w: &mut impl Write) -> io::Result<()> {
    let hello = read_list(r)?;
    if hello.first().map(String::as_str) != Some("git-filter-client")
        || !hello.iter().any(|l| l == "version=2")
    {
        return Err(bad("not a version-2 Git filter client"));
    }
    write_text(w, "git-filter-server")?;
    write_text(w, "version=2")?;
    write_flush(w)?;
    w.flush()?;

    let caps = read_list(r)?;
    if !caps.iter().any(|l| l == "capability=clean") {
        return Err(bad("Git did not offer the clean capability"));
    }
    write_text(w, "capability=clean")?;
    write_flush(w)?;
    w.flush()?;

    loop {
        // End of input between requests is Git closing the pipe: the normal way a session ends.
        if r.fill_buf()?.is_empty() {
            return Ok(());
        }
        let request = read_list(r)?;
        let command = request.iter().find_map(|l| l.strip_prefix("command="));
        let mut cleaner = Cleaner::new().ok_or_else(|| bad("SHA-256 is unavailable"))?;
        loop {
            match read_pkt(r)? {
                Some(Pkt::Data(d)) => cleaner.update(&d),
                Some(Pkt::Flush) => break,
                None => return Err(bad("input ended inside file content")),
            }
        }
        // Only `clean` was negotiated; anything else is answered as an error, which Git reports
        // per file rather than tearing down the filter.
        let output = match command {
            Some("clean") => cleaner.finish(),
            _ => None,
        };
        match output {
            Some(bytes) => {
                write_text(w, "status=success")?;
                write_flush(w)?;
                for chunk in bytes.chunks(PKT_MAX) {
                    write_pkt(w, chunk)?;
                }
                write_flush(w)?;
                // An empty trailing list: the status above stands.
                write_flush(w)?;
            }
            None => {
                write_text(w, "status=error")?;
                write_flush(w)?;
            }
        }
        w.flush()?;
    }
}

/// Streams one file's content and produces what `git-lfs clean` would store for it.
struct Cleaner {
    head: Vec<u8>,
    len: u64,
    hash: crate::digest::Sha256Stream,
}

impl Cleaner {
    fn new() -> Option<Self> {
        Some(Self {
            head: Vec::new(),
            len: 0,
            hash: crate::digest::Sha256Stream::new()?,
        })
    }

    fn update(&mut self, bytes: &[u8]) {
        if self.head.len() < POINTER_MAX {
            let take = bytes.len().min(POINTER_MAX - self.head.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        self.len += bytes.len() as u64;
        self.hash.update(bytes);
    }

    /// The cleaned bytes, or `None` if the digest failed.
    fn finish(self) -> Option<Vec<u8>> {
        // `git-lfs` stores an empty file as an empty blob, and an existing pointer as itself.
        if self.len == 0 {
            return Some(Vec::new());
        }
        if (self.len as usize) < POINTER_MAX && is_pointer(&self.head) {
            return Some(self.head);
        }
        let oid = crate::digest::to_hex(&self.hash.finish()?);
        Some(pointer(&oid, self.len).into_bytes())
    }
}

/// The canonical pointer text `git-lfs` writes for content with this SHA-256 and size.
fn pointer(oid: &str, size: u64) -> String {
    format!("{POINTER_VERSION}\noid sha256:{oid}\nsize {size}\n")
}

/// Whether `bytes` is exactly a canonical pointer, so passing it through is what `git-lfs` would
/// store. Anything less exact is hashed as content, which can only make the diff noisier.
fn is_pointer(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut lines = text.split_terminator('\n');
    let (Some(version), Some(oid), Some(size), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return false;
    };
    let oid_ok = oid.strip_prefix("oid sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    let size_ok = size
        .strip_prefix("size ")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    version == POINTER_VERSION && oid_ok && size_ok && text.ends_with('\n')
}

/// In the unit-test binary, run filter mode before the test harness starts when Git spawns this
/// executable as the filter, so the binding tests exercise the real filter end to end. This uses
/// the MSVC CRT initializer table, as the `ctor` crate does; it is compiled into tests only.
#[cfg(test)]
#[used]
#[link_section = ".CRT$XCU"]
static TEST_FILTER_ENTRY: extern "C" fn() = {
    extern "C" fn entry() {
        if std::env::args().nth(1).as_deref() == Some(FILTER_FLAG) {
            std::process::exit(serve_stdio());
        }
    }
    entry
};

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(content: &[u8]) -> Vec<u8> {
        let mut c = Cleaner::new().unwrap();
        for chunk in content.chunks(3) {
            c.update(chunk);
        }
        c.finish().unwrap()
    }

    /// Pinned against `git-lfs/3.7.1 clean` on the same 5-byte payload.
    #[test]
    fn content_becomes_the_pointer_git_lfs_writes() {
        let expected = pointer(
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            5,
        );
        assert_eq!(clean(b"hello"), expected.as_bytes());
    }

    #[test]
    fn a_pointer_and_an_empty_file_pass_through() {
        let p = pointer(&"a".repeat(64), 12345);
        assert_eq!(clean(p.as_bytes()), p.as_bytes());
        assert!(clean(b"").is_empty());
        // Not exactly canonical: hashed as content, not passed through.
        let loose = p.replace("size", "size  ");
        assert_ne!(clean(loose.as_bytes()), loose.as_bytes());
        assert!(!is_pointer(p.trim_end().as_bytes()));
    }

    fn pkt(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        write_pkt(&mut v, s.as_bytes()).unwrap();
        v
    }

    /// One whole session as Git drives it: handshake, capabilities, a clean, an unsupported
    /// command, then end of input.
    #[test]
    fn the_protocol_round_trip_cleans_and_refuses_what_it_did_not_negotiate() {
        let mut input = Vec::new();
        for l in ["git-filter-client\n", "version=2\n"] {
            input.extend(pkt(l));
        }
        input.extend(b"0000");
        for l in ["capability=clean\n", "capability=smudge\n"] {
            input.extend(pkt(l));
        }
        input.extend(b"0000");
        input.extend(pkt("command=clean\n"));
        input.extend(pkt("pathname=x.bin\n"));
        input.extend(b"0000");
        input.extend(pkt("hel"));
        input.extend(pkt("lo"));
        input.extend(b"0000");
        input.extend(pkt("command=smudge\n"));
        input.extend(b"0000");
        input.extend(b"0000");

        let mut out = Vec::new();
        serve(&mut &input[..], &mut out).unwrap();

        let mut r = &out[..];
        assert_eq!(
            read_list(&mut r).unwrap(),
            ["git-filter-server", "version=2"]
        );
        assert_eq!(read_list(&mut r).unwrap(), ["capability=clean"]);
        assert_eq!(read_list(&mut r).unwrap(), ["status=success"]);
        let Some(Pkt::Data(body)) = read_pkt(&mut r).unwrap() else {
            panic!("expected content");
        };
        assert_eq!(body, clean(b"hello"));
        assert!(matches!(read_pkt(&mut r).unwrap(), Some(Pkt::Flush)));
        assert!(read_list(&mut r).unwrap().is_empty());
        assert_eq!(read_list(&mut r).unwrap(), ["status=error"]);
        assert!(r.is_empty());
    }

    #[test]
    fn a_client_that_is_not_git_is_refused() {
        let mut input = pkt("hello\n");
        input.extend(b"0000");
        assert!(serve(&mut &input[..], &mut Vec::new()).is_err());
    }
}
