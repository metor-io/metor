use super::*;
use metor_db::{
    manifest::{ComponentManifest, SpanState},
    seal::{SealRecord, SealRecordExt},
    time_series::TimeSeriesNode,
};
use metor_proto::schema::Schema;
use metor_proto_wkt::{ComponentMetadata, DbConfig, MsgMetadata};

pub(super) fn database(path: &Path, cancel: &AtomicBool) -> io::Result<()> {
    if !cfg!(target_endian = "little") {
        return Err(invalid("Recording import requires a little-endian host"));
    }
    let _: DbConfig = decode(&path.join("db_state"))?;
    let mut wal_budget = 0u64;
    for entry in fs::read_dir(path)? {
        cancelled(cancel)?;
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("Invalid component name"))?;
        if name == "msgs" {
            for log in fs::read_dir(entry.path())? {
                let log = log?;
                if !number::<u16>(&log.file_name().to_string_lossy()) {
                    return Err(invalid("Invalid message id"));
                }
                wal_budget += 1024 * 1024;
                check_wal_budget(wal_budget)?;
                if log.path().join("metadata").exists() {
                    let _: MsgMetadata = decode(&log.path().join("metadata"))?;
                }
                for node in fs::read_dir(log.path())? {
                    let node = node?;
                    if node.file_type()?.is_dir() {
                        messages(&node.path(), cancel)?;
                    }
                }
            }
            continue;
        }
        let id: u64 = name.parse().map_err(invalid)?;
        let component = entry.path();
        // Validate the wire schema before ComponentSchema converts its shape
        // using native integer multiplication and allocates a WAL on open.
        let schema: Schema<Vec<u64>> = decode(&component.join("schema"))?;
        if schema.dim().len() > 16 {
            return Err(invalid("Too many schema dimensions"));
        }
        let element_size = schema
            .dim()
            .iter()
            .try_fold(schema.prim_type().size() as u64, |n, d| n.checked_mul(*d))
            .ok_or_else(|| invalid("Schema size overflows"))?;
        if element_size == 0 || element_size > 64 * 1024 {
            return Err(invalid(
                "Unsupported recording element size (maximum 64 KiB)",
            ));
        }
        wal_budget += element_size * 256;
        check_wal_budget(wal_budget)?;
        let metadata: ComponentMetadata = decode(&component.join("metadata"))?;
        if metadata.component_id.0 != id {
            return Err(invalid("Component metadata id mismatch"));
        }
        let manifest = ComponentManifest::read_from(&component)
            .map_err(invalid)?
            .unwrap_or_default();
        for span in &manifest.spans {
            if span.seal.count == 0
                || span.seal.start_ts.0 > span.cover_end.0
                || span.cover_end.0 > span.seal.end_ts.0
                || span.seal.element_size != element_size
                || span.seal.count.checked_mul(8) != Some(span.seal.index_len)
                || span.seal.count.checked_mul(element_size) != Some(span.seal.data_len)
            {
                return Err(invalid("Invalid component coverage metadata"));
            }
            if span.state == SpanState::Resident
                && !component
                    .join(span.seal.start_ts.0.to_string())
                    .join("seal")
                    .is_file()
            {
                return Err(invalid("Recording is missing a resident chunk"));
            }
        }
        for node in fs::read_dir(&component)? {
            cancelled(cancel)?;
            let node = node?;
            if !node.file_type()?.is_dir() {
                continue;
            }
            let start = node_start(&node.path())?;
            let (index_len, extra) = log_header(&node.path().join("index"), 24)?;
            let (data_len, data_extra) = log_header(&node.path().join("data"), 24)?;
            if index_len % 8 != 0
                || (index_len / 8).checked_mul(element_size) != Some(data_len)
                || data_extra != element_size
                || extra as i64 != start
            {
                return Err(invalid("Mismatched sample index and data lengths"));
            }
            let (_, end) =
                timestamps(&node.path().join("index"), 24, index_len / 8, start, cancel)?;
            let seal_path = node.path().join("seal");
            if seal_path.exists() {
                let seal: SealRecord = decode(&seal_path)?;
                if seal.start_ts.0 != start
                    || seal.end_ts.0 != end
                    || seal.count != index_len / 8
                    || seal.index_len != index_len
                    || seal.data_len != data_len
                    || seal.element_size != element_size
                {
                    return Err(invalid("Chunk seal does not match its records"));
                }
                // Mapping is safe now: header and payload bounds are checked.
                if !seal.verify(&TimeSeriesNode::open(node.path()).map_err(invalid)?) {
                    return Err(invalid("Chunk seal checksum mismatch"));
                }
            }
        }
    }
    Ok(())
}
fn check_wal_budget(bytes: u64) -> io::Result<()> {
    if bytes > 512 * 1024 * 1024 {
        return Err(invalid(
            "Recording exceeds the 512 MiB working-memory limit",
        ));
    }
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    postcard::from_bytes(&read_metadata(path)?).map_err(invalid)
}
fn node_start(path: &Path) -> io::Result<i64> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("Missing chunk timestamp"))?
        .parse()
        .map_err(invalid)
}
fn log_header(path: &Path, size: u64) -> io::Result<(u64, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut header = [0; 24];
    file.read_exact(&mut header[..size as usize])?;
    let committed = u64::from_le_bytes(header[..8].try_into().unwrap());
    if committed != len || committed < size {
        return Err(invalid("Invalid compact log header"));
    }
    Ok((
        committed - size,
        u64::from_le_bytes(header[16..24].try_into().unwrap()),
    ))
}
fn timestamps(
    path: &Path,
    header: u64,
    count: u64,
    start: i64,
    cancel: &AtomicBool,
) -> io::Result<(i64, i64)> {
    let mut input = BufReader::new(File::open(path)?);
    input.seek(SeekFrom::Start(header))?;
    let mut previous = i64::MIN;
    for index in 0..count {
        if index % 8192 == 0 {
            cancelled(cancel)?;
        }
        let mut bytes = [0; 8];
        input.read_exact(&mut bytes)?;
        let timestamp = i64::from_le_bytes(bytes);
        if timestamp < previous || (index == 0 && timestamp != start) {
            return Err(invalid("Unordered or mismatched chunk timestamps"));
        }
        previous = timestamp;
    }
    Ok((start, previous))
}
fn messages(path: &Path, cancel: &AtomicBool) -> io::Result<()> {
    let (timestamp_len, _) = log_header(&path.join("timestamps"), 16)?;
    let (offset_len, _) = log_header(&path.join("offsets"), 16)?;
    let (data_len, _) = log_header(&path.join("data_log"), 16)?;
    if timestamp_len % 8 != 0 || timestamp_len.checked_mul(2) != Some(offset_len) {
        return Err(invalid("Mismatched message timestamps and offsets"));
    }
    let count = timestamp_len / 8;
    timestamps(
        &path.join("timestamps"),
        16,
        count,
        node_start(path)?,
        cancel,
    )?;
    let mut offsets = BufReader::new(File::open(path.join("offsets"))?);
    offsets.seek(SeekFrom::Start(16))?;
    let mut payload_end = 0;
    for index in 0..count {
        if index % 8192 == 0 {
            cancelled(cancel)?;
        }
        let mut buf = [0; 16];
        offsets.read_exact(&mut buf)?;
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as u64;
        if len > 12 {
            let offset = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as u64;
            if offset != payload_end || offset + len > data_len {
                return Err(invalid("Message payload lies outside the captured data"));
            }
            payload_end = offset + len;
        }
    }
    if payload_end != data_len {
        return Err(invalid("Unexpected trailing message payload"));
    }
    Ok(())
}
