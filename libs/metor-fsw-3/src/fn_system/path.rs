//! [`ParamPath`], the dotted name of a parameter inside a system fn.

use std::borrow::Cow;

/// Where a parameter sits among a system fn's parameters.
///
/// A path is a chain of segments borrowed from the callers that built it. It
/// allocates only when [`to_name`](Self::to_name) joins two or more segments.
#[derive(Clone, Copy, Debug)]
pub enum ParamPath<'a> {
    /// No segment.
    Root,
    /// A fn's argument names. Adds no segment; the tuple of the fn's
    /// parameters reads element `i`'s name from it.
    Args(&'static [&'static str]),
    /// A named field or argument under its parent.
    Name(&'a ParamPath<'a>, &'static str),
    /// An array element under its parent.
    Index(&'a ParamPath<'a>, usize),
}

static ROOT: ParamPath<'static> = ParamPath::Root;

impl<'a> ParamPath<'a> {
    /// The path of the field `name` under this one.
    pub fn child(&'a self, name: &'static str) -> Self {
        Self::Name(self, name)
    }

    /// The path of element `i` under this one.
    pub fn index(&'a self, i: usize) -> Self {
        Self::Index(self, i)
    }

    /// The path of the fn's argument `i`, or the root when it is flattened.
    ///
    /// On a path that holds no argument names, every argument shares this
    /// path.
    pub fn arg(&self, i: usize, flatten: bool) -> Self {
        match self {
            Self::Args(_) if flatten => Self::Root,
            Self::Args(names) => names.get(i).map_or(Self::Root, |name| ROOT.child(name)),
            other => *other,
        }
    }

    /// Joins the segments with dots.
    ///
    /// A single name stays borrowed.
    pub fn to_name(&self) -> Cow<'static, str> {
        match self {
            Self::Name(parent, name) if parent.is_empty() => Cow::Borrowed(name),
            _ => {
                let mut name = String::new();
                self.write(&mut name);
                Cow::Owned(name)
            }
        }
    }

    /// Whether this path has no segment.
    fn is_empty(&self) -> bool {
        matches!(self, Self::Root | Self::Args(_))
    }

    fn write(&self, out: &mut String) {
        let parent = match self {
            Self::Root | Self::Args(_) => return,
            Self::Name(parent, _) | Self::Index(parent, _) => parent,
        };
        parent.write(out);
        if !out.is_empty() {
            out.push('.');
        }
        match self {
            Self::Name(_, name) => out.push_str(name),
            Self::Index(_, i) => push_index(out, *i),
            Self::Root | Self::Args(_) => {}
        }
    }
}

/// Appends `i` in decimal.
fn push_index(out: &mut String, i: usize) {
    if i >= 10 {
        push_index(out, i / 10);
    }
    out.push(char::from_digit((i % 10) as u32, 10).unwrap_or('?'));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_one_name_stays_borrowed() {
        let name = ROOT.child("imu").to_name();
        assert!(matches!(name, Cow::Borrowed("imu")));
    }

    #[test]
    fn test_segments_join_with_dots() {
        let wheels = ROOT.child("wheels");
        let two = wheels.index(2);
        assert_eq!(two.child("cmd").to_name(), "wheels.2.cmd");
        assert_eq!(ROOT.index(0).index(1).to_name(), "0.1");
        assert_eq!(ROOT.child("x").index(1207).to_name(), "x.1207");
    }

    #[test]
    fn test_args_name_each_argument() {
        let args = ParamPath::Args(&["imu", "est"]);
        assert_eq!(args.arg(1, false).to_name(), "est");
        assert_eq!(args.arg(1, true).child("fss").to_name(), "fss");
    }

    #[test]
    fn test_root_and_missing_args_are_empty() {
        assert_eq!(ROOT.to_name(), "");
        assert_eq!(ParamPath::Args(&[]).arg(3, false).to_name(), "");
        let fss = ROOT.child("fss");
        assert_eq!(fss.arg(0, false).to_name(), "fss");
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    const NAMES: [&str; 3] = ["a", "bc", "d"];

    /// A name or a small index, and its text.
    fn segment<'a>(parent: &'a ParamPath<'a>) -> (ParamPath<'a>, String) {
        let pick: u8 = kani::any();
        kani::assume(pick < 6);
        match pick {
            0..3 => {
                let name = NAMES[pick as usize];
                (parent.child(name), name.to_string())
            }
            _ => {
                let i = pick - 3;
                (
                    parent.index(usize::from(i)),
                    char::from(b'0' + i).to_string(),
                )
            }
        }
    }

    /// `to_name` is the dotted join of the segments, up to depth 3.
    #[kani::proof]
    #[kani::unwind(8)]
    fn to_name_joins_segments() {
        let (one, a) = segment(&ROOT);
        let (two, b) = segment(&one);
        let (three, c) = segment(&two);
        assert_eq!(one.to_name(), a);
        assert_eq!(two.to_name(), [a.as_str(), &b].join("."));
        assert_eq!(three.to_name(), [a.as_str(), &b, &c].join("."));
    }
}
