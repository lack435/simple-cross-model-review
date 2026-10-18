//! A collision-resistant content digest, for fingerprinting captured evidence.
//!
//! The Perforce resume delta decides whether a file is byte-identical to what the reviewer
//! was shown last turn by comparing a stored fingerprint of the evidence. That evidence is
//! attacker-influenced content from a repository this tool does not trust, so the fingerprint
//! is a **security boundary**: a non-cryptographic hash (FNV and the like) can be *deliberately*
//! collided by an author who controls both revisions, rendering a changed file as "unchanged"
//! and slipping it past the merge gate. The digest therefore has to be collision-resistant.
//!
//! SHA-256 via **Windows CNG** (`BCryptHash`) gives that without a third-party crate on a
//! Windows-only tool: a reviewed OS implementation rather than a hand-rolled one. The design
//! (see `docs/perforce-resume-delta.md`) is **fail-closed** -- if the digest cannot be
//! produced, [`sha256`] returns `None` and the caller disables elision rather than trusting a
//! weaker comparison. `std::collections::hash_map::DefaultHasher` is separately unusable here:
//! it is not stable across Rust releases and the fingerprint is persisted to disk.
//!
//! The persisted form is [`Fingerprint`]: the digest plus the input length, so a false
//! "unchanged" would require a SHA-256 collision *at a fixed length*.

use std::ffi::c_void;

use serde::{Deserialize, Serialize};

/// The pseudo-handle for the SHA-256 algorithm provider, usable directly with
/// [`BCryptHash`] without opening or closing a provider. Defined by `bcrypt.h` as
/// `BCRYPT_SHA256_ALG_HANDLE`.
const BCRYPT_SHA256_ALG_HANDLE: *mut c_void = 0x0000_0041 as *mut c_void;

/// `STATUS_SUCCESS`. `BCryptHash` returns an `NTSTATUS`, which is zero on success and a
/// negative code on failure.
const STATUS_SUCCESS: i32 = 0;

/// `BCryptGenRandom` flag: use the system-preferred RNG without opening a provider, so the
/// algorithm handle may be null. Defined by `bcrypt.h` as `BCRYPT_USE_SYSTEM_PREFERRED_RNG`.
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

#[link(name = "bcrypt")]
extern "system" {
    /// One-shot hash (Windows 10+). Passing the algorithm pseudo-handle avoids the
    /// open/create/finish/destroy dance. `pbInput` is typed mutable by the API but is not
    /// written, so passing a pointer derived from a shared slice is sound (see [`sha256`]).
    fn BCryptHash(
        hAlgorithm: *mut c_void,
        pbSecret: *mut u8,
        cbSecret: u32,
        pbInput: *mut u8,
        cbInput: u32,
        pbOutput: *mut u8,
        cbOutput: u32,
    ) -> i32;
    /// Fill a buffer with cryptographically-random bytes from the OS CSPRNG (Windows CNG). With a
    /// null algorithm handle and `BCRYPT_USE_SYSTEM_PREFERRED_RNG` it needs no provider handle.
    fn BCryptGenRandom(
        hAlgorithm: *mut c_void,
        pbBuffer: *mut u8,
        cbBuffer: u32,
        dwFlags: u32,
    ) -> i32;
    /// Create a reusable hash object for [`Sha256Stream`]. With a null `pbHashObject` CNG allocates
    /// the object itself (Windows 7+), so no size query is needed.
    fn BCryptCreateHash(
        hAlgorithm: *mut c_void,
        phHash: *mut *mut c_void,
        pbHashObject: *mut u8,
        cbHashObject: u32,
        pbSecret: *mut u8,
        cbSecret: u32,
        dwFlags: u32,
    ) -> i32;
    /// Feed bytes to a hash object. `pbInput` is not written (as with [`BCryptHash`]).
    fn BCryptHashData(hHash: *mut c_void, pbInput: *mut u8, cbInput: u32, dwFlags: u32) -> i32;
    fn BCryptFinishHash(hHash: *mut c_void, pbOutput: *mut u8, cbOutput: u32, dwFlags: u32) -> i32;
    fn BCryptDestroyHash(hHash: *mut c_void) -> i32;
}

/// A lowercase-hex token of `n` cryptographically-random bytes (`2n` chars), or `None` if the OS
/// CSPRNG could not be read (fail-closed: an unguessable token must never be replaced by a
/// predictable fallback). Used for the setup approval page's one-time capability token.
#[allow(dead_code)] // production caller lands with the setup tool (Phase 3 task #15).
pub fn random_hex_token(n: usize) -> Option<String> {
    let len = u32::try_from(n).ok()?;
    let mut buf = vec![0u8; n];
    // SAFETY: `BCryptGenRandom` writes exactly `cbBuffer` bytes into `pbBuffer`; the buffer is `n`
    // bytes and the length matches. A null algorithm handle is valid with the system-preferred flag.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    (status == STATUS_SUCCESS).then(|| to_hex(&buf))
}

/// SHA-256 of `bytes`, or `None` if the OS digest could not be produced.
///
/// `None` is the fail-closed signal: the caller must treat the unit as non-elidable rather
/// than fall back to a weaker comparison. Inputs longer than `u32::MAX` also yield `None` --
/// captured evidence is capped far below that, so it never happens in practice, but a digest
/// that silently hashed only a prefix would be worse than none.
pub fn sha256(bytes: &[u8]) -> Option<[u8; 32]> {
    let len = u32::try_from(bytes.len()).ok()?;
    let mut out = [0u8; 32];
    // SAFETY: `BCryptHash` reads `cbInput` bytes from `pbInput` and writes exactly
    // `cbOutput` (32) bytes to `pbOutput`. It does not modify the input despite the mutable
    // pointer type, so casting away the shared slice's constness is sound; we never form a
    // `&mut` to the input. The secret pointer is null with length zero for an unkeyed hash.
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null_mut(),
            0,
            bytes.as_ptr() as *mut u8,
            len,
            out.as_mut_ptr(),
            out.len() as u32,
        )
    };
    (status == STATUS_SUCCESS).then_some(out)
}

/// Incremental SHA-256 over input too large to hold at once: an LFS-tracked working-tree file can be
/// gigabytes (issue #150). Same CNG provider as [`sha256`]; any failed call poisons the stream, so
/// [`Sha256Stream::finish`] returns `None` rather than a digest of part of the input.
pub struct Sha256Stream {
    handle: *mut c_void,
    ok: bool,
}

impl Sha256Stream {
    pub fn new() -> Option<Self> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: `phHash` points at a local the call fills in; the object buffer is null with length
        // zero, which asks CNG to allocate it; the secret is null with length zero (unkeyed).
        let status = unsafe {
            BCryptCreateHash(
                BCRYPT_SHA256_ALG_HANDLE,
                &mut handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
                0,
            )
        };
        (status == STATUS_SUCCESS && !handle.is_null()).then_some(Self { handle, ok: true })
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(u32::MAX as usize) {
            // SAFETY: the handle is live until `Drop`; CNG reads exactly `cbInput` bytes and does not
            // write through the input pointer (see `sha256`).
            let status = unsafe {
                BCryptHashData(
                    self.handle,
                    chunk.as_ptr() as *mut u8,
                    chunk.len() as u32,
                    0,
                )
            };
            self.ok &= status == STATUS_SUCCESS;
        }
    }

    pub fn finish(self) -> Option<[u8; 32]> {
        let mut out = [0u8; 32];
        // SAFETY: the handle is live; the output buffer is exactly the 32-byte SHA-256 length.
        let status =
            unsafe { BCryptFinishHash(self.handle, out.as_mut_ptr(), out.len() as u32, 0) };
        (self.ok && status == STATUS_SUCCESS).then_some(out)
    }
}

impl Drop for Sha256Stream {
    fn drop(&mut self) {
        // SAFETY: the handle came from `BCryptCreateHash` and is destroyed exactly once, here.
        unsafe {
            BCryptDestroyHash(self.handle);
        }
    }
}

/// A content fingerprint: the SHA-256 digest and the input length.
///
/// The length is stored alongside the digest so that a false "unchanged" would require a
/// collision at a fixed length, not merely any collision. Serialized with the digest as
/// lowercase hex, which is compact and diff-friendly in the session store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub len: u64,
    pub sha256: String,
}

impl Fingerprint {
    /// Fingerprint `bytes`, or `None` if the digest was unavailable (fail-closed).
    pub fn of(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            len: bytes.len() as u64,
            sha256: to_hex(&sha256(bytes)?),
        })
    }
}

/// Lowercase hex encoding, so a digest is a stable, human-readable string in the store.
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer vectors from FIPS 180-4 / the SHA-256 test suite. These pin the FFI to
    /// the standard algorithm, so a wrong pseudo-handle or a truncated read would be caught.
    #[test]
    fn sha256_matches_known_answers() {
        assert_eq!(
            to_hex(&sha256(b"").expect("digest of empty")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc").expect("digest of abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// The stream agrees with the one-shot digest however the input is split, and on empty input.
    #[test]
    fn sha256_stream_matches_the_one_shot_digest() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut stream = Sha256Stream::new().expect("stream");
        for chunk in data.chunks(7_919) {
            stream.update(chunk);
        }
        assert_eq!(stream.finish(), sha256(&data));
        assert_eq!(Sha256Stream::new().unwrap().finish(), sha256(b""));
    }

    #[test]
    fn sha256_is_length_and_content_sensitive() {
        // A one-byte change and a length change both move the digest -- the two properties the
        // fingerprint relies on to refuse a false "unchanged".
        assert_ne!(sha256(b"abc"), sha256(b"abd"));
        assert_ne!(sha256(b"abc"), sha256(b"abc "));
    }

    #[test]
    fn fingerprint_carries_length_and_digest() {
        let fp = Fingerprint::of(b"abc").expect("fingerprint");
        assert_eq!(fp.len, 3);
        assert_eq!(
            fp.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Equal inputs fingerprint equal; different inputs do not.
        assert_eq!(Fingerprint::of(b"abc"), Fingerprint::of(b"abc"));
        assert_ne!(Fingerprint::of(b"abc"), Fingerprint::of(b"abd"));
    }

    #[test]
    fn a_larger_input_hashes_without_error() {
        // 400 KB is the diff cap; make sure a realistically large unit still digests.
        let big = vec![b'x'; 400_000];
        assert!(sha256(&big).is_some());
    }
}
