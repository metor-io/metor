//! Fixed-capacity alarm ids and messages.

use core::fmt;

use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};

/// A full alarm id, `{ns}.{system}.{name}`.
pub type Id = heapless::String<ID_CAP>;

const ID_CAP: usize = 96;
const MSG_CAP: usize = 128;

/// An alarm message. Writes past its capacity are truncated.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Msg(heapless::String<MSG_CAP>);

impl fmt::Write for Msg {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        push_cut(&mut self.0, s, MSG_CAP)
            .then_some(())
            .ok_or(fmt::Error)
    }
}

impl Msg {
    /// Lets `write!(m, ..)` work without importing `fmt::Write`.
    pub fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        fmt::Write::write_fmt(self, args)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// Length of `_{hash:08x}`, the suffix `compose` appends to a cut id.
const HASH_SUFFIX_LEN: usize = 9;

/// The infix of a redline's id.
pub(crate) const REDLINE: &str = ".redline.";

/// Writes the concatenation of `parts` into `id`. If it does not fit, cuts
/// it and appends `_` and an FNV-1a hash of the whole.
pub(crate) fn compose(id: &mut Id, parts: &[&str]) {
    id.clear();
    let fits = parts.iter().map(|p| p.len()).sum::<usize>() <= ID_CAP;
    let cap = if fits {
        ID_CAP
    } else {
        ID_CAP - HASH_SUFFIX_LEN
    };
    for p in parts {
        push_cut(id, p, cap);
    }
    if !fits {
        let hash = fnv1a_32(parts.iter().flat_map(|p| p.bytes()));
        let _ = fmt::Write::write_fmt(id, format_args!("_{hash:08x}"));
    }
}

/// True if `prefix` leaves room for `.redline.`, one byte, and a hash suffix.
pub(crate) fn prefix_fits(prefix: &str) -> bool {
    prefix.len() + REDLINE.len() + 1 + HASH_SUFFIX_LEN <= ID_CAP
}

/// `fnv1a_64(def_id) ^ raised_at`, the occurrence id the panel keys on.
pub(crate) fn occurrence(def_id: &str, raised_at: Timestamp) -> u64 {
    const_fnv1a_hash::fnv1a_hash_str_64(def_id) ^ raised_at.0 as u64
}

/// Pushes the longest prefix of `s` that keeps `out` within `cap` bytes, cut
/// on a char boundary. Returns false if any of `s` was cut.
fn push_cut<const N: usize>(out: &mut heapless::String<N>, s: &str, cap: usize) -> bool {
    let room = cap.min(N).saturating_sub(out.len());
    let keep = s.floor_char_boundary(room);
    let pushed = out.push_str(&s[..keep]);
    debug_assert!(pushed.is_ok());
    keep == s.len()
}

fn fnv1a_32(bytes: impl Iterator<Item = u8>) -> u32 {
    bytes.fold(0x811c_9dc5, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composed(parts: &[&str]) -> Id {
        let mut id = Id::new();
        compose(&mut id, parts);
        id
    }

    fn ends_in_hash(id: &str) -> bool {
        let (_, tail) = id.split_at(id.len() - HASH_SUFFIX_LEN);
        tail.starts_with('_') && tail[1..].bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[test]
    fn hash_matches_fnv1a_32() {
        let text = "imu.get() > 1";
        assert_eq!(
            fnv1a_32(text.bytes()),
            const_fnv1a_hash::fnv1a_hash_str_32(text)
        );
    }

    #[test]
    fn msg_write_short() {
        let mut m = Msg::default();
        assert!(write!(m, "batt {:.1} C", 61.25).is_ok());
        assert_eq!(m.as_str(), "batt 61.2 C");
    }

    #[test]
    fn msg_overflow_truncates() {
        let mut m = Msg::default();
        let long = "x".repeat(200);
        assert!(write!(m, "{long}").is_err());
        assert_eq!(m.as_str(), &long[..MSG_CAP]);
    }

    #[test]
    fn msg_drops_a_char_cut_at_the_end() {
        let mut m = Msg::default();
        let text = format!("{}é", "x".repeat(MSG_CAP - 1));
        assert!(write!(m, "{text}").is_err());
        assert_eq!(m.as_str().len(), MSG_CAP - 1);
    }

    #[test]
    fn compose_short_has_no_suffix() {
        let id = composed(&["ns.power", ".", "batt_hot"]);
        assert_eq!(id.as_str(), "ns.power.batt_hot");
    }

    #[test]
    fn compose_at_capacity() {
        let exact = "n".repeat(ID_CAP - 2);
        assert_eq!(composed(&["p", ".", &exact]).len(), ID_CAP);
        assert!(!ends_in_hash(&composed(&["p", ".", &exact])));
        let over = composed(&["p", ".", &exact, "n"]);
        assert_eq!(over.len(), ID_CAP);
        assert!(ends_in_hash(&over));
    }

    #[test]
    fn long_ids_with_a_shared_prefix_differ() {
        let base = "a".repeat(ID_CAP);
        let (a, b) = (composed(&[&base, "1"]), composed(&[&base, "2"]));
        assert!(ends_in_hash(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn prefix_fits_leaves_room() {
        assert!(prefix_fits(&"p".repeat(77)));
        assert!(!prefix_fits(&"p".repeat(78)));
    }

    #[test]
    fn occurrence_differs_by_id_and_time() {
        let t = Timestamp(42);
        assert_ne!(occurrence("a.b", t), occurrence("a.c", t));
        assert_ne!(occurrence("a.b", t), occurrence("a.b", Timestamp(43)));
    }
}
