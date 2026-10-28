//! Value types, frame layouts, and compile-time constants.

use mc_rt::desc::{FrameDesc, LeafDesc};

pub use mc_rt::desc::Prim;

use crate::diag::Span;

/// The type of a value or a place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Prim(Prim),
    /// A fixed array, which only a frame field can hold.
    Array(Box<Ty>, u32),
    /// A frame, by index into the program's frames.
    Frame(usize),
}

impl Ty {
    pub fn prim(&self) -> Option<Prim> {
        match self {
            Ty::Prim(prim) => Some(*prim),
            _ => None,
        }
    }

    /// The size and alignment of a field of this type.
    pub fn layout(&self, frames: &[FrameTy]) -> (u32, u32) {
        match self {
            Ty::Prim(prim) => (prim.size(), prim.size()),
            Ty::Array(elem, len) => {
                let (size, align) = elem.layout(frames);
                (size * len, align)
            }
            Ty::Frame(index) => frames.get(*index).map_or((0, 1), |f| (f.size, f.align)),
        }
    }
}

/// Names a scalar type as the source spells it.
pub fn prim_named(name: &str) -> Option<Prim> {
    Some(match name {
        "bool" => Prim::Bool,
        "u8" => Prim::U8,
        "u16" => Prim::U16,
        "u32" => Prim::U32,
        "u64" => Prim::U64,
        "i8" => Prim::I8,
        "i16" => Prim::I16,
        "i32" => Prim::I32,
        "i64" => Prim::I64,
        "f32" => Prim::F32,
        "f64" => Prim::F64,
        _ => return None,
    })
}

pub fn prim_name(prim: Prim) -> &'static str {
    match prim {
        Prim::Bool => "bool",
        Prim::U8 => "u8",
        Prim::U16 => "u16",
        Prim::U32 => "u32",
        Prim::U64 => "u64",
        Prim::I8 => "i8",
        Prim::I16 => "i16",
        Prim::I32 => "i32",
        Prim::I64 => "i64",
        Prim::F32 => "f32",
        Prim::F64 => "f64",
    }
}

/// A `#[repr(C)]` frame: an `i64` timestamp at offset 0, then its fields.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameTy {
    /// The name the source uses, such as `HeaterCmd`.
    pub name: String,
    /// The record name, such as `heater_cmd`.
    pub record: String,
    pub size: u32,
    pub align: u32,
    pub timestamp: u32,
    pub fields: Vec<FieldTy>,
    /// Where the frame was declared, or imported.
    pub span: Span,
    /// The pack it came from, for an imported frame.
    pub pack: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldTy {
    pub name: String,
    pub ty: Ty,
    pub offset: u32,
}

impl FrameTy {
    /// Lays out a declared frame's fields after its timestamp.
    pub fn declared(name: &str, fields: Vec<(String, Ty)>, frames: &[FrameTy], span: Span) -> FrameTy {
        let mut offset = 8u32;
        let mut align = 8u32;
        let fields = fields
            .into_iter()
            .map(|(name, ty)| {
                let (size, field_align) = ty.layout(frames);
                offset = offset.next_multiple_of(field_align);
                align = align.max(field_align);
                let field = FieldTy { name, ty, offset };
                offset += size;
                field
            })
            .collect();
        FrameTy {
            name: name.to_string(),
            record: snake_case(name),
            size: offset.next_multiple_of(align),
            align,
            timestamp: 0,
            fields,
            span,
            pack: None,
        }
    }

    pub fn field(&self, name: &str) -> Option<&FieldTy> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The runtime's view of this frame: every scalar leaf, with arrays
    /// flattened and `_`-prefixed padding left out, as `#[derive(Frame)]`
    /// does.
    pub fn desc(&self) -> FrameDesc {
        let mut leaves = Vec::new();
        for field in self.fields.iter().filter(|f| !f.name.starts_with('_')) {
            push_leaves(&mut leaves, &field.name, &field.ty, field.offset);
        }
        FrameDesc {
            name: self.record.clone(),
            size: self.size,
            align: self.align,
            timestamp: self.timestamp,
            leaves,
        }
    }

    /// Rebuilds a frame from a pack's description of it.
    ///
    /// Returns `None` if a leaf is neither a scalar field nor an element of
    /// an array field.
    pub fn imported(desc: &FrameDesc, pack: &str, span: Span) -> Option<FrameTy> {
        let mut fields: Vec<FieldTy> = Vec::new();
        for leaf in &desc.leaves {
            let (name, index) = match leaf.path.split_once('.') {
                Some((name, index)) => (name, Some(index.parse::<u32>().ok()?)),
                None => (leaf.path.as_str(), None),
            };
            match (index, fields.last_mut()) {
                (None, _) => fields.push(FieldTy {
                    name: name.to_string(),
                    ty: Ty::Prim(leaf.prim),
                    offset: leaf.offset,
                }),
                (Some(0), _) => fields.push(FieldTy {
                    name: name.to_string(),
                    ty: Ty::Array(Box::new(Ty::Prim(leaf.prim)), 1),
                    offset: leaf.offset,
                }),
                (Some(i), Some(last)) if last.name == name => match &mut last.ty {
                    Ty::Array(elem, len) if *len == i && **elem == Ty::Prim(leaf.prim) => *len += 1,
                    _ => return None,
                },
                _ => return None,
            }
        }
        let frame = FrameTy {
            name: pascal_case(&desc.name),
            record: desc.name.clone(),
            size: desc.size,
            align: desc.align,
            timestamp: desc.timestamp,
            fields,
            span,
            pack: Some(pack.to_string()),
        };
        (frame.desc() == *desc).then_some(frame)
    }
}

/// Adds the leaves of a field of type `ty` at `offset`.
fn push_leaves(leaves: &mut Vec<LeafDesc>, path: &str, ty: &Ty, offset: u32) {
    match ty {
        Ty::Prim(prim) => leaves.push(LeafDesc {
            path: path.to_string(),
            offset,
            prim: *prim,
        }),
        Ty::Array(elem, len) => {
            let (size, _) = elem.layout(&[]);
            for i in 0..*len {
                push_leaves(leaves, &format!("{path}.{i}"), elem, offset + i * size);
            }
        }
        Ty::Frame(_) => {}
    }
}

/// `HeaterCmd` to `heater_cmd`, as `#[derive(Frame)]` names a record.
pub fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1);
        let boundary = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase() || p.is_ascii_digit() || (p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
            });
        if boundary {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// `valve_cmd` to `ValveCmd`.
pub fn pascal_case(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect()
}

/// A value known at compile time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConstVal {
    Int(i128),
    Float(f64),
    Bool(bool),
}

impl ConstVal {
    pub fn as_f64(self) -> f64 {
        match self {
            ConstVal::Int(v) => v as f64,
            ConstVal::Float(v) => v,
            ConstVal::Bool(v) => f64::from(u8::from(v)),
        }
    }

    pub fn is_zero(self) -> bool {
        match self {
            ConstVal::Int(v) => v == 0,
            ConstVal::Float(v) => v == 0.0,
            ConstVal::Bool(v) => !v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_pads_like_repr_c() {
        let fields = vec![
            ("on".to_string(), Ty::Prim(Prim::Bool)),
            ("duty".to_string(), Ty::Prim(Prim::F64)),
            ("bits".to_string(), Ty::Array(Box::new(Ty::Prim(Prim::U16)), 3)),
        ];
        let frame = FrameTy::declared("HeaterCmd", fields, &[], Span::default());
        let offsets: Vec<u32> = frame.fields.iter().map(|f| f.offset).collect();
        assert_eq!(offsets, [8, 16, 24]);
        assert_eq!((frame.size, frame.align, frame.record.as_str()), (32, 8, "heater_cmd"));
        assert_eq!(frame.desc().leaves.len(), 5);
    }

    #[test]
    fn test_import_round_trips() {
        let fields = vec![
            ("psi".to_string(), Ty::Prim(Prim::F64)),
            ("t".to_string(), Ty::Array(Box::new(Ty::Prim(Prim::F32)), 2)),
        ];
        let frame = FrameTy::declared("Chamber", fields, &[], Span::default());
        let back = FrameTy::imported(&frame.desc(), "stand_io", Span::default()).expect("rebuilds");
        assert_eq!(back.fields, frame.fields);
        assert_eq!(back.name, "Chamber");
        let mut nested = frame.desc();
        nested.leaves[0].path = "a.b".into();
        assert!(FrameTy::imported(&nested, "p", Span::default()).is_none());
    }

    #[test]
    fn test_case_conversions() {
        assert_eq!(snake_case("HeaterCmd"), "heater_cmd");
        assert_eq!(snake_case("IMUData"), "imu_data");
        assert_eq!(snake_case("chamber"), "chamber");
        assert_eq!(pascal_case("valve_cmd"), "ValveCmd");
    }
}
