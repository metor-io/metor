//! Standalone storage-mode arguments; paths remain OS strings.
#[derive(Debug, PartialEq)]
pub(crate) enum Startup {
    Choose,
    Temporary,
    RecordTo(std::path::PathBuf),
    Open(std::path::PathBuf),
    /// A temporary session that mirrors the db at `host:port` at once.
    Connect(String),
    Help,
}

/// The startup mode plus `--preset FILE`, a layout loaded into the first
/// window in place of any saved one.
#[derive(Debug, PartialEq)]
pub(crate) struct Args {
    pub mode: Startup,
    pub preset: Option<std::path::PathBuf>,
}

pub(crate) const USAGE: &str = "Usage: metor-panel [--temporary | --record-to PATH | --open PATH | --connect HOST:PORT] [--preset LAYOUT.json]";

pub(crate) fn startup_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Args, String> {
    let mut args = args.into_iter();
    let mut mode = None;
    let mut preset = None;
    while let Some(arg) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .filter(|v| !v.is_empty() && !v.to_string_lossy().starts_with("--"))
                .ok_or(format!("{} requires {what}", arg.to_string_lossy()))
        };
        let next = match arg.to_str() {
            Some("--temporary") => Startup::Temporary,
            Some("--record-to") => Startup::RecordTo(value("a new recording path")?.into()),
            Some("--open") => Startup::Open(value("a recording path")?.into()),
            Some("--connect") => Startup::Connect(
                value("a HOST:PORT address")?
                    .into_string()
                    .map_err(|_| "--connect address is not UTF-8")?,
            ),
            Some("--preset") => {
                if preset.replace(value("a layout file")?.into()).is_some() {
                    return Err("--preset given twice".into());
                }
                continue;
            }
            Some("--help" | "-h") => {
                return Ok(Args {
                    mode: Startup::Help,
                    preset: None,
                });
            }
            _ => return Err(format!("Unknown option: {}", arg.to_string_lossy())),
        };
        if mode.replace(next).is_some() {
            return Err("Choose one startup mode; unexpected extra arguments".into());
        }
    }
    Ok(Args {
        mode: mode.unwrap_or(Startup::Choose),
        preset,
    })
}

/// Resolve `host:port`, preferring IPv4: a db bound to `0.0.0.0` is not
/// reachable on the `::1` that `localhost` often resolves to first.
pub(crate) fn resolve(addr: &str) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| format!("--connect {addr}: {e}"))?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or(format!("--connect {addr}: no addresses"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        startup_args(args.iter().map(std::ffi::OsString::from))
    }

    fn mode(args: &[&str]) -> Result<Startup, String> {
        parse(args).map(|a| a.mode)
    }

    #[test]
    fn startup_modes_are_exclusive_and_preserve_path_arguments() {
        assert_eq!(mode(&[]).unwrap(), Startup::Choose);
        assert_eq!(mode(&["--temporary"]).unwrap(), Startup::Temporary);
        assert_eq!(
            mode(&["--record-to", "/tmp/a recording.metor"]).unwrap(),
            Startup::RecordTo("/tmp/a recording.metor".into())
        );
        assert!(parse(&["--record-to"]).is_err());
        assert!(parse(&["--record-to", ""]).is_err());
        assert!(parse(&["--record-to", "--temporary"]).is_err());
        assert!(parse(&["--temporary", "--record-to", "x"]).is_err());
        assert!(parse(&["--record-to", "x", "--open", "y"]).is_err());
    }

    #[test]
    fn connect_and_preset_combine_in_either_order() {
        let want = Args {
            mode: Startup::Connect("127.0.0.1:2250".into()),
            preset: Some("/p/star tracker.json".into()),
        };
        let a = [
            "--connect",
            "127.0.0.1:2250",
            "--preset",
            "/p/star tracker.json",
        ];
        assert_eq!(parse(&a).unwrap(), want);
        let b = [
            "--preset",
            "/p/star tracker.json",
            "--connect",
            "127.0.0.1:2250",
        ];
        assert_eq!(parse(&b).unwrap(), want);
        assert_eq!(parse(&["--preset", "x"]).unwrap().mode, Startup::Choose);
        assert!(parse(&["--connect"]).is_err());
        assert!(parse(&["--connect", "a:1", "--temporary"]).is_err());
        assert!(parse(&["--preset", "a", "--preset", "b"]).is_err());
        assert!(parse(&["--preset", "--connect", "a:1"]).is_err());
    }

    #[test]
    fn resolve_prefers_ipv4() {
        assert_eq!(
            resolve("127.0.0.1:2250").unwrap().to_string(),
            "127.0.0.1:2250"
        );
        assert!(resolve("localhost:2250").unwrap().is_ipv4());
        assert!(resolve("no-port").is_err());
    }
}
