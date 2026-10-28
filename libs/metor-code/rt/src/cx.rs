//! The context every compiled function takes, and the words it returns.

/// The memory one compiled call reads and writes.
///
/// The runtime lays out every block at `create`. The compiled code only
/// reads and writes through these pointers, at offsets the descriptor
/// records.
#[repr(C)]
#[derive(Debug)]
pub struct McCx {
    /// The newest frame of each input.
    pub inputs: *const u8,
    /// One byte per input: 1 once the input has had a record.
    pub present: *const u8,
    /// The current value of each output.
    pub outputs: *mut u8,
    /// One byte per output: set to 1 when the call writes the output.
    pub written: *mut u8,
    pub state: *mut u8,
    pub params: *const u8,
    /// This cycle's time, in microseconds.
    pub now: i64,
    /// The time since the previous cycle, in seconds.
    pub dt: f64,
    /// The faults the call raised, as [`Fault`] bits.
    pub faults: u32,
    pub reserved: u32,
}

/// Field offsets of [`McCx`], which the compiler reads through.
pub mod offsets {
    use super::McCx;

    pub const INPUTS: i32 = core::mem::offset_of!(McCx, inputs) as i32;
    pub const PRESENT: i32 = core::mem::offset_of!(McCx, present) as i32;
    pub const OUTPUTS: i32 = core::mem::offset_of!(McCx, outputs) as i32;
    pub const WRITTEN: i32 = core::mem::offset_of!(McCx, written) as i32;
    pub const STATE: i32 = core::mem::offset_of!(McCx, state) as i32;
    pub const PARAMS: i32 = core::mem::offset_of!(McCx, params) as i32;
    pub const NOW: i32 = core::mem::offset_of!(McCx, now) as i32;
    pub const DT: i32 = core::mem::offset_of!(McCx, dt) as i32;
    pub const FAULTS: i32 = core::mem::offset_of!(McCx, faults) as i32;
}

/// A compiled entry point. Every entry has this signature.
///
/// A `step` returns 0; a `resume` returns a [`ResumeWord`]; a redline
/// returns 1 when it trips.
pub type EntryFn = unsafe extern "C" fn(*mut McCx) -> u32;

/// The faults compiled code can raise, one bit each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Fault {
    /// An integer division or remainder by zero, which yields 0.
    DivByZero = 1,
    /// A dynamic index out of range, which clamps.
    IndexClamped = 2,
    /// A NaN written to a field, which keeps the field's last value.
    NanWrite = 4,
}

impl Fault {
    pub const ALL: [Fault; 3] = [Fault::DivByZero, Fault::IndexClamped, Fault::NanWrite];

    /// The fault's kind on the system's `log`.
    pub const fn kind(self) -> &'static str {
        match self {
            Fault::DivByZero => "div_by_zero",
            Fault::IndexClamped => "index_clamped",
            Fault::NanWrite => "nan_write",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Fault::DivByZero => "integer division by zero yielded 0",
            Fault::IndexClamped => "an index out of range was clamped",
            Fault::NanWrite => "a NaN write kept the field's last value",
        }
    }

    /// Iterates over the faults set in `word`.
    pub fn of(word: u32) -> impl Iterator<Item = Fault> {
        Fault::ALL
            .into_iter()
            .filter(move |fault| word & *fault as u32 != 0)
    }
}

/// The words a sequence's `resume` returns, which match fsw-3's `Status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ResumeWord {
    Running = 0,
    Completed = 2,
    Failed = 4,
}

impl ResumeWord {
    /// Reads a word, treating an unknown one as a failure.
    pub fn from_raw(word: u32) -> ResumeWord {
        match word {
            0 => ResumeWord::Running,
            2 => ResumeWord::Completed,
            _ => ResumeWord::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fault_bits_decode() {
        let word = Fault::DivByZero as u32 | Fault::NanWrite as u32;
        let faults: Vec<_> = Fault::of(word).collect();
        assert_eq!(faults, [Fault::DivByZero, Fault::NanWrite]);
        assert_eq!(Fault::of(0).count(), 0);
        assert_eq!(Fault::of(u32::MAX).count(), 3);
    }

    #[test]
    fn test_resume_words() {
        assert_eq!(ResumeWord::from_raw(0), ResumeWord::Running);
        assert_eq!(ResumeWord::from_raw(2), ResumeWord::Completed);
        assert_eq!(ResumeWord::from_raw(4), ResumeWord::Failed);
        assert_eq!(ResumeWord::from_raw(99), ResumeWord::Failed);
    }

    #[test]
    fn test_cx_layout_is_pointer_first() {
        assert_eq!(offsets::INPUTS, 0);
        assert_eq!(offsets::NOW, 6 * size_of::<usize>() as i32);
        const { assert!(offsets::FAULTS > offsets::DT) };
    }
}
