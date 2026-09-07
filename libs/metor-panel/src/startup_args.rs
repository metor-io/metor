//! Standalone storage-mode arguments; paths remain OS strings.
#[derive(Debug, PartialEq)]
pub(crate) enum Startup {
    Choose,
    Temporary,
    RecordTo(std::path::PathBuf),
    Open(std::path::PathBuf),
    Help,
}

pub(crate) fn startup_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Startup, String> {
    let mut args = args.into_iter();
    let Some(arg) = args.next() else {
        return Ok(Startup::Choose);
    };
    let startup = match arg.to_str() {
        Some("--temporary") => Startup::Temporary,
        Some("--record-to") => Startup::RecordTo(
            args.next()
                .filter(|path| !path.is_empty() && !path.to_string_lossy().starts_with("--"))
                .ok_or("--record-to requires a new recording path")?
                .into(),
        ),
        Some("--open") => Startup::Open(
            args.next()
                .filter(|path| !path.is_empty() && !path.to_string_lossy().starts_with("--"))
                .ok_or("--open requires a recording path")?
                .into(),
        ),
        Some("--help" | "-h") => Startup::Help,
        _ => return Err(format!("Unknown option: {}", arg.to_string_lossy())),
    };
    if args.next().is_some() {
        return Err("Choose one startup mode; unexpected extra arguments".into());
    }
    Ok(startup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_modes_are_exclusive_and_preserve_path_arguments() {
        let parse = |args: &[&str]| startup_args(args.iter().map(std::ffi::OsString::from));
        assert_eq!(parse(&[]).unwrap(), Startup::Choose);
        assert_eq!(parse(&["--temporary"]).unwrap(), Startup::Temporary);
        assert_eq!(
            parse(&["--record-to", "/tmp/a recording.metor"]).unwrap(),
            Startup::RecordTo("/tmp/a recording.metor".into())
        );
        assert!(parse(&["--record-to"]).is_err());
        assert!(parse(&["--record-to", ""]).is_err());
        assert!(parse(&["--record-to", "--temporary"]).is_err());
        assert!(parse(&["--temporary", "--record-to", "x"]).is_err());
        assert!(parse(&["--record-to", "x", "--open", "y"]).is_err());
    }
}
