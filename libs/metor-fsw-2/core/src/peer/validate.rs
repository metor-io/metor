use std::collections::HashSet;

use metor_proto::types::ComponentId;
use metor_proto::vtable::{Op, OpRef};

use super::{
    CONTRACT_VERSION, ContractError, DynamicBounds, EndpointContract, FrameChannel, MAX_CHANNELS,
};

impl EndpointContract {
    /// Check the offline ICD structure. Incoming record bytes require separate
    /// validation against the locally pinned schema before entering any ring.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.format_version != CONTRACT_VERSION {
            return Err(ContractError::invalid(
                "format_version",
                format!(
                    "expected {CONTRACT_VERSION}, received {}",
                    self.format_version,
                ),
            ));
        }
        for (path, value) in [
            ("name", &self.name),
            ("version", &self.version),
            ("clock_domain", &self.clock_domain),
        ] {
            if value.trim().is_empty() {
                return Err(ContractError::invalid(path, "must not be empty"));
            }
        }
        if self.channels.is_empty() || self.channels.len() > MAX_CHANNELS {
            return Err(ContractError::invalid(
                "channels",
                format!("expected 1..={MAX_CHANNELS} channels"),
            ));
        }
        let mut keys = HashSet::new();
        for channel in &self.channels {
            if !keys.insert(&channel.key) {
                return Err(ContractError::invalid(
                    format!("channels.{}", channel.key),
                    "duplicate channel key",
                ));
            }
            channel.validate()?;
        }
        Ok(())
    }
}

impl FrameChannel {
    pub fn validate(&self) -> Result<(), ContractError> {
        let path = format!("channels.{}", self.key);
        let invalid = |reason: &str| ContractError::invalid(&path, reason);
        let mut chars = self.key.chars();
        if !chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(invalid(
                "channel key must start with an ASCII letter and contain only letters, digits, or underscores",
            ));
        }
        let frame = &self.frame;
        if frame.name.is_empty() || frame.frame_id != ComponentId::new(&frame.name) {
            return Err(invalid("frame name and frame ID must agree"));
        }
        let layout = &frame.layout;
        if layout.fixed_size == 0
            || frame.max_record_size < layout.fixed_size
            || frame.max_record_size > u32::MAX as u64
        {
            return Err(invalid("invalid fixed or maximum record size"));
        }
        if !layout.alignment.is_power_of_two() || layout.fixed_size % layout.alignment != 0 {
            return Err(invalid("invalid frame alignment"));
        }
        let mut end = 0u64;
        let mut max_bytes = layout.fixed_size;
        for field in &layout.fields {
            if !field.alignment.is_power_of_two()
                || field.alignment > layout.alignment
                || field.offset % field.alignment != 0
                || field.offset != end
            {
                return Err(invalid(
                    "fields must cover the fixed region in order with explicit padding and valid alignment",
                ));
            }
            end = field
                .offset
                .checked_add(field.size)
                .ok_or_else(|| invalid("field size overflow"))?;
            if let Some(bounds) = &field.dynamic {
                if field.size != 8 {
                    return Err(invalid("dynamic field must occupy an 8-byte slot"));
                }
                let (max_count, stride, alignment, key_bytes) = match *bounds {
                    DynamicBounds::List {
                        max_count,
                        stride,
                        alignment,
                    } => (max_count, stride, alignment, 0),
                    DynamicBounds::Map {
                        max_count,
                        max_key_bytes,
                        stride,
                        alignment,
                        value_offset,
                        value_size,
                    } => {
                        if max_key_bytes == 0
                            || value_offset < 8
                            || value_size == 0
                            || value_offset
                                .checked_add(value_size)
                                .is_none_or(|end| end > stride)
                        {
                            return Err(invalid("invalid map value layout or key bound"));
                        }
                        (max_count, stride, alignment, max_key_bytes)
                    }
                };
                if stride == 0 || !alignment.is_power_of_two() || stride % alignment != 0 {
                    return Err(invalid("invalid dynamic stride or alignment"));
                }
                let budget = stride
                    .checked_add(key_bytes)
                    .and_then(|n| n.checked_mul(max_count))
                    .and_then(|n| n.checked_add(7))
                    .map(|n| n & !7)
                    .ok_or_else(|| invalid("dynamic bound overflow"))?;
                max_bytes = max_bytes
                    .checked_add(budget)
                    .ok_or_else(|| invalid("record size overflow"))?;
            }
        }
        if end != layout.fixed_size || max_bytes > frame.max_record_size {
            return Err(invalid(
                "layout and dynamic bounds exceed declared record size",
            ));
        }
        if let Some(offset) = layout.timestamp_offset {
            if !layout
                .fields
                .iter()
                .any(|field| field.offset == offset && field.size == 8 && field.dynamic.is_none())
            {
                return Err(invalid("timestamp must identify a fixed 8-byte field"));
            }
        }
        let mut ids = HashSet::new();
        for meta in &frame.metadata {
            if !ids.insert(meta.component_id) {
                return Err(invalid("duplicate component metadata ID"));
            }
        }
        self.validate_dynamic_schema(&path)
    }

    /// Check that every dynamic vtable terminal has a directly described bound.
    /// Existing nested frames do not export recursive trailer budgets, so reject
    /// those contracts explicitly rather than infer unsafe limits from MAX_SIZE.
    fn validate_dynamic_schema(&self, path: &str) -> Result<(), ContractError> {
        let vt = &self.frame.vtable;
        let invalid = |reason: &str| ContractError::invalid(path, reason);
        let mut members = HashSet::new();
        let mut dynamic_ops = HashSet::new();
        for (index, op) in vt.ops.iter().enumerate() {
            if let Op::List { members: range, .. } | Op::Map { members: range, .. } = op {
                let end = range
                    .start
                    .checked_add(range.count)
                    .ok_or_else(|| invalid("dynamic member range overflow"))?;
                if end as usize > vt.fields.len() {
                    return Err(invalid("dynamic member range outside vtable"));
                }
                members.extend(range.start as usize..end as usize);
                dynamic_ops.insert(index);
            }
        }
        // Hidden typed values must not disappear from the contract just because
        // their field was opted out of ground telemetry. Only explicitly skipped
        // byte arrays may occupy otherwise undescribed bytes.
        let mut covered = Vec::new();
        if let Some(offset) = self.frame.layout.timestamp_offset {
            covered.push((offset, offset + 8));
        }
        for (index, field) in vt.fields.iter().enumerate() {
            if members.contains(&index) {
                continue;
            }
            let start = field.offset.to_index() as u64;
            let end = start + field.len as u64;
            if end > self.frame.layout.fixed_size {
                return Err(invalid("vtable field lies outside the fixed region"));
            }
            covered.push((start, end));
        }
        covered.sort_unstable();
        for field in self.frame.layout.fields.iter().filter(|f| !f.padding) {
            let mut cursor = field.offset;
            for &(start, end) in &covered {
                if start > cursor {
                    break;
                }
                cursor = cursor.max(end);
            }
            if cursor < field.offset + field.size {
                return Err(invalid(
                    "fixed field contains bytes without a telemetry schema; only skipped u8 arrays may be padding",
                ));
            }
        }
        let mut described = HashSet::new();
        for field in self
            .frame
            .layout
            .fields
            .iter()
            .filter(|field| field.dynamic.is_some())
        {
            let root = vt
                .fields
                .iter()
                .enumerate()
                .find(|(index, f)| {
                    !members.contains(index)
                        && f.offset.to_index() as u64 == field.offset
                        && f.len == 8
                })
                .ok_or_else(|| {
                    invalid(
                        "dynamic field missing from vtable (hidden dynamic fields are unsupported)",
                    )
                })?
                .1;
            let terminal = terminal(vt.ops.as_slice(), root.arg)
                .ok_or_else(|| invalid("invalid or cyclic dynamic vtable op chain"))?;
            let agrees = match (field.dynamic.as_ref().unwrap(), &vt.ops[terminal]) {
                (DynamicBounds::List { stride, .. }, Op::List { stride: wire, .. }) => {
                    *stride == *wire as u64
                }
                (
                    DynamicBounds::Map {
                        stride,
                        value_offset,
                        ..
                    },
                    Op::Map {
                        stride: wire,
                        value_offset: offset,
                        ..
                    },
                ) => *stride == *wire as u64 && *value_offset == *offset as u64,
                _ => false,
            };
            if !agrees {
                return Err(invalid("dynamic bounds disagree with vtable layout"));
            }
            described.insert(terminal);
        }
        if described != dynamic_ops {
            return Err(invalid(
                "nested or unbounded dynamic fields are not supported in peer contracts yet",
            ));
        }
        for field in self
            .frame
            .layout
            .fields
            .iter()
            .filter(|f| f.dynamic.is_some())
        {
            let root = vt
                .fields
                .iter()
                .enumerate()
                .find(|(index, f)| {
                    !members.contains(index)
                        && f.offset.to_index() as u64 == field.offset
                        && f.len == 8
                })
                .unwrap()
                .1;
            let terminal = terminal(vt.ops.as_slice(), root.arg).unwrap();
            let (range, size) = match (&vt.ops[terminal], field.dynamic.as_ref().unwrap()) {
                (Op::List { members, .. }, DynamicBounds::List { stride, .. }) => {
                    (members, *stride)
                }
                (Op::Map { members, .. }, DynamicBounds::Map { value_size, .. }) => {
                    (members, *value_size)
                }
                _ => unreachable!("dynamic schema checked above"),
            };
            let mut ranges: Vec<_> = vt.fields
                [range.start as usize..(range.start + range.count) as usize]
                .iter()
                .map(|f| {
                    (
                        f.offset.to_index() as u64,
                        f.offset.to_index() as u64 + f.len as u64,
                    )
                })
                .collect();
            ranges.sort_unstable();
            let mut cursor = 0;
            for (start, end) in ranges {
                if start > cursor || end > size {
                    return Err(invalid(
                        "dynamic element contains undescribed bytes or fields outside its fixed value",
                    ));
                }
                cursor = cursor.max(end);
            }
            if cursor != size {
                return Err(invalid(
                    "dynamic element contains undescribed bytes; recursive element layout metadata is not exported yet",
                ));
            }
        }
        Ok(())
    }
}

fn terminal(ops: &[Op], mut reference: OpRef) -> Option<usize> {
    // Bounded iteration also rejects cycles without invoking vtable realization
    // on data loaded from a manifest.
    for _ in 0..ops.len() {
        match ops.get(reference.to_index())? {
            Op::Frame { arg, .. }
            | Op::Timestamp { arg, .. }
            | Op::Schema { arg, .. }
            | Op::Ext { arg, .. } => reference = *arg,
            Op::List { .. } | Op::Map { .. } => return Some(reference.to_index()),
            _ => return None,
        }
    }
    None
}
