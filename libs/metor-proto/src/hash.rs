//! Incremental dotted-path hashing.
//!
//! [`PathHasher`] is a rolling fnv1a-64 hasher that builds the same
//! [`ComponentId`](crate::types::ComponentId) as
//! [`ComponentId::new`](crate::types::ComponentId::new) over a dotted name, but
//! lets the name be assembled segment-by-segment without ever materializing the
//! full `String`.
//!
//! This is the mechanism behind dynamic-component identity (see the VTable
//! `List`/`Map`/`PathComponent` ops): a name like `processes.htop.pid` is hashed
//! by pushing `"processes"`, then the runtime key `"htop"`, then the member
//! `"pid"`. Because fnv1a is a pure left-fold, feeding those segments separated by
//! `.` yields the exact bytes — and therefore the exact hash — of the literal
//! `"processes.htop.pid"`.

use crate::types::ComponentId;

// fnv1a-64 constants, matching `const_fnv1a_hash` (and thus `ComponentId::new`).
const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x0000_0100_0000_01B3;

/// A rolling fnv1a-64 hasher over a dotted component path.
///
/// Segments are separated by `.`; empty segments contribute nothing and emit no
/// separator, matching the `ComponentPath`/`ChainPath` join rules. The final
/// [`finish`](PathHasher::finish) masks the top bit exactly like
/// [`ComponentId::new`](crate::types::ComponentId::new) (so the id fits in an
/// `i64` and stays Lua-compatible).
#[derive(Clone, Copy, Debug)]
pub struct PathHasher {
    hash: u64,
    has_content: bool,
}

impl Default for PathHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl PathHasher {
    /// Creates an empty hasher seeded with the fnv1a-64 offset basis.
    pub const fn new() -> Self {
        Self {
            hash: FNV_OFFSET_BASIS_64,
            has_content: false,
        }
    }

    #[inline]
    fn feed(&mut self, byte: u8) {
        self.hash ^= byte as u64;
        self.hash = self.hash.wrapping_mul(FNV_PRIME_64);
    }

    /// Appends one path segment's raw bytes, inserting a `.` separator first if a
    /// previous non-empty segment was already pushed. Empty segments are no-ops.
    #[inline]
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.has_content {
            self.feed(b'.');
        }
        for &b in bytes {
            self.feed(b);
        }
        self.has_content = true;
    }

    /// Appends one path segment.
    #[inline]
    pub fn push(&mut self, segment: &str) {
        self.push_bytes(segment.as_bytes());
    }

    /// Appends a list index segment (its decimal representation), e.g. `3` -> `"3"`.
    ///
    /// Formats into a stack buffer, so no allocation occurs.
    pub fn push_index(&mut self, idx: u32) {
        let mut buf = [0u8; 10]; // u32::MAX is 10 digits
        let mut n = idx;
        let mut i = buf.len();
        if n == 0 {
            i -= 1;
            buf[i] = b'0';
        } else {
            while n > 0 {
                i -= 1;
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        self.push_bytes(&buf[i..]);
    }

    /// Finalizes the hash into a [`ComponentId`], masking the top bit.
    #[inline]
    pub fn finish(self) -> ComponentId {
        ComponentId(self.hash & !(1u64 << 63))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn via_hasher(segments: &[&str]) -> ComponentId {
        let mut h = PathHasher::new();
        for s in segments {
            h.push(s);
        }
        h.finish()
    }

    #[test]
    fn matches_component_id_new() {
        assert_eq!(via_hasher(&["processes", "htop", "pid"]), ComponentId::new("processes.htop.pid"));
        assert_eq!(via_hasher(&["a"]), ComponentId::new("a"));
        assert_eq!(via_hasher(&[]), ComponentId::new(""));
        // empty segments are skipped, matching ChainPath join rules
        assert_eq!(via_hasher(&["", "a", "", "b"]), ComponentId::new("a.b"));
    }

    #[test]
    fn index_segments() {
        let mut h = PathHasher::new();
        h.push("processes");
        h.push_index(0);
        h.push("pid");
        assert_eq!(h.finish(), ComponentId::new("processes.0.pid"));

        let mut h = PathHasher::new();
        h.push("a");
        h.push_index(4294967295);
        assert_eq!(h.finish(), ComponentId::new("a.4294967295"));
    }

    /// Property test: for random dotted paths, pushing the segments one at a time must
    /// equal `ComponentId::new` of the joined string.
    #[test]
    fn property_matches_component_id_new() {
        // Small deterministic LCG so the test needs no rng dependency.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state >> 33
        };
        const ALPHABET: &[u8] = b"abcdefABCDEF0123_";
        for _ in 0..2000 {
            let n_segments = (next() % 5) + 1;
            let mut segments: std::vec::Vec<std::string::String> = std::vec::Vec::new();
            for _ in 0..n_segments {
                let len = next() % 6; // allow empty segments
                let mut s = std::string::String::new();
                for _ in 0..len {
                    let c = ALPHABET[(next() as usize) % ALPHABET.len()];
                    s.push(c as char);
                }
                segments.push(s);
            }
            let mut h = PathHasher::new();
            for s in &segments {
                h.push(s);
            }
            // Join non-empty segments with '.', matching the empty-segment rule.
            let joined = segments
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<std::vec::Vec<_>>()
                .join(".");
            assert_eq!(
                h.finish(),
                ComponentId::new(&joined),
                "mismatch for segments {segments:?} (joined {joined:?})"
            );
        }
    }
}
