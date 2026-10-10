#![cfg(kani)]

use crate::io::region::Region;
use crate::io::{BufRef, BufTable};

/// A header, two entries, and 16 data bytes.
const LEN: usize = 12 + 2 * 16 + 16;

/// For any directory bytes and any `BufRef`, `checked` is `None` or a span
/// inside the region.
#[kani::proof]
#[kani::unwind(4)]
fn checked_bufref_in_region() {
    let bytes: [u8; LEN] = kani::any();
    let mut region = Region::mirror(Box::new(bytes));
    let start = region.base().as_ptr() as usize;
    let Ok(table) = BufTable::from_region(region) else {
        return;
    };
    let at = BufRef::new(kani::any::<u16>() % 3);
    if let Some(span) = table.checked(at) {
        let p = span.cast::<u8>().as_ptr() as usize;
        assert!(p >= start && p + span.len() <= start + LEN);
    }
}
