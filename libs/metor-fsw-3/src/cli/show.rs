//! `metor show` prints a resolved deployment as tables.
//!
//! With no name it shows every target; `fsw` one target; `fsw.nav`, or
//! `nav` in a deployment of one, one system with its params and grants.

use anstyle::Style;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::completions;
use super::run::{RunError, prepare, print};
use super::ui::event::Event;
use super::ui::table::{self, Align};
use super::ui::theme::Theme;
use super::ui::{Format, Ui};
use crate::cli::ui::glyph;
use crate::coordinator::{Clock, InputConfig, SystemConfig};
use crate::deploy::{ResolvedDeployment, TargetConfig};

/// The arguments to `metor show`.
#[derive(clap::Args, Debug, Default)]
pub struct Show {
    /// A target file, then a name: `fsw` for a target, `fsw.nav` for one of
    /// its systems. Without a file, the nearest `target.py` is shown.
    #[arg(num_args = 0..=2, value_name = "FILE|NAME", add = completions::names(&[completions::TARGET, completions::SYSTEM], true))]
    pub args: Vec<String>,
}

/// What `show` prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    All,
    Target(usize),
    System(usize, usize),
}

/// Prints the deployment, or the part of it `args` names, to stdout.
pub fn show(args: Show, ui: &Ui) -> Result<(), RunError> {
    let (file, name) = split_args(&args.args);
    let config = prepare(file.as_deref(), ui)?;
    let selection = select(&config, name.as_deref())?;
    if ui.format == Format::Json {
        let value = selected_json(&config, selection)?;
        ui.emit(Event::Data("config", &value));
        return Ok(());
    }
    print(ui, |out, theme, width| {
        render(out, &config, selection, theme, width)
    })
}

/// `fsw`, or `target` for a deployment's one unnamed target.
pub(crate) fn target_name<S, R>(target: &TargetConfig<S, R>) -> &str {
    Some(target.namespace())
        .filter(|ns| !ns.is_empty())
        .unwrap_or("target")
}

/// `1 target` or `3 targets`.
pub(crate) fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// Splits `args` into a target file and a name. A lone argument is the
/// file when it looks like a path: it has a separator, or ends in `.py` or
/// `.json`.
pub(crate) fn split_args(args: &[String]) -> (Option<PathBuf>, Option<String>) {
    match args {
        [file, name] => (Some(file.into()), Some(name.clone())),
        [only] if is_path(only) => (Some(only.into()), None),
        [only] => (None, Some(only.clone())),
        _ => (None, None),
    }
}

fn is_path(arg: &str) -> bool {
    let path = Path::new(arg);
    arg.contains(std::path::MAIN_SEPARATOR)
        || path
            .extension()
            .is_some_and(|ext| ext == "py" || ext == "json")
}

/// Finds what `name` names in `config`.
pub fn select(config: &ResolvedDeployment, name: Option<&str>) -> Result<Selection, RunError> {
    let Some(name) = name else {
        return Ok(Selection::All);
    };
    let namespaces = config.namespaces();
    if let Some(target) = namespaces.iter().position(|ns| *ns == name) {
        return Ok(Selection::Target(target));
    }
    let qualified = name.split_once('.').and_then(|(ns, system)| {
        let target = namespaces.iter().position(|n| *n == ns)?;
        Some((target, system))
    });
    let (target, system) = match qualified {
        Some((target, system)) => (Some(target), system),
        None if config.targets.len() == 1 => (Some(0), name),
        None => (None, name),
    };
    let found = target.and_then(|target| {
        let systems = &config.targets[target].coordinator.systems;
        let index = systems.iter().position(|s| s.id == system)?;
        Some(Selection::System(target, index))
    });
    found.ok_or_else(|| RunError::NoSuchName {
        name: name.to_string(),
        known: names(config),
    })
}

/// Every name `select` accepts.
pub(crate) fn names(config: &ResolvedDeployment) -> Vec<String> {
    let several = config.targets.len() > 1;
    let mut names: Vec<String> = Vec::new();
    for (target, ns) in config.targets.iter().zip(config.namespaces()) {
        if several {
            names.push(ns.to_string());
        }
        for system in &target.coordinator.systems {
            names.push(if several {
                format!("{ns}.{}", system.id)
            } else {
                system.id.clone()
            });
        }
    }
    names
}

fn selected_json(
    config: &ResolvedDeployment,
    selection: Selection,
) -> Result<serde_json::Value, RunError> {
    let value = match selection {
        Selection::All => serde_json::to_value(config),
        Selection::Target(target) => serde_json::to_value(&config.targets[target]),
        Selection::System(target, system) => {
            serde_json::to_value(&config.targets[target].coordinator.systems[system])
        }
    };
    value.map_err(|e| crate::deploy::ConfigError::Decode(e).into())
}

/// Writes `selection` of `config` as tables, each line within `width`.
pub fn render(
    out: &mut impl Write,
    config: &ResolvedDeployment,
    selection: Selection,
    theme: &Theme,
    width: Option<usize>,
) -> io::Result<()> {
    match selection {
        Selection::All => {
            write_overview(out, config, theme)?;
            for index in 0..config.targets.len() {
                writeln!(out)?;
                write_target(out, config, index, theme, width)?;
            }
            Ok(())
        }
        Selection::Target(index) => write_target(out, config, index, theme, width),
        Selection::System(target, system) => write_system(out, config, target, system, theme),
    }
}

/// `3 targets · 4 packs`.
fn write_overview(
    out: &mut impl Write,
    config: &ResolvedDeployment,
    theme: &Theme,
) -> io::Result<()> {
    let muted = theme.muted;
    let dot = glyph::DOT;
    let targets = config.targets.len();
    let mut packs: Vec<&str> = config
        .targets
        .iter()
        .flat_map(|target| target.packs.iter().map(|pack| pack.id.as_str()))
        .collect();
    packs.sort_unstable();
    packs.dedup();
    writeln!(
        out,
        "  {muted}{} {dot} {}{muted:#}",
        plural(targets, "target"),
        plural(packs.len(), "pack")
    )
}

/// A target's title line, then its systems.
fn write_target(
    out: &mut impl Write,
    config: &ResolvedDeployment,
    index: usize,
    theme: &Theme,
    width: Option<usize>,
) -> io::Result<()> {
    let target = &config.targets[index];
    write_title(out, target, index, theme)?;
    let systems = target.coordinator.systems.iter();
    let rows = systems.map(|system| vec![system.id.clone(), kind(system), inputs(system)]);
    table::write(out, &SYSTEM_COLUMNS, rows, theme, width, "    ")
}

/// `fsw  100 Hz wall`, the name in the target's hue.
pub(crate) fn write_title(
    out: &mut impl Write,
    target: &TargetConfig,
    index: usize,
    theme: &Theme,
) -> io::Result<()> {
    let hue = theme.bold(theme.hue(index));
    let muted = theme.muted;
    let name = target_name(target);
    let clock = clock(&target.coordinator.clock);
    writeln!(out, "  {hue}{name}{hue:#}  {muted}{clock}{muted:#}")
}

/// One system: its kind and location, then params, inputs, and grants.
fn write_system(
    out: &mut impl Write,
    config: &ResolvedDeployment,
    target: usize,
    system: usize,
    theme: &Theme,
) -> io::Result<()> {
    let config_target = &config.targets[target];
    let system = &config_target.coordinator.systems[system];
    let hue = theme.bold(theme.hue(target));
    let muted = theme.muted;
    let ns = config_target.namespace();
    let prefix = if config.targets.len() > 1 {
        format!("{hue}{ns}{hue:#}.")
    } else {
        String::new()
    };
    let bold = theme.bold(Style::new());
    write!(
        out,
        "  {prefix}{bold}{}{bold:#}  {muted}{}",
        system.id, system.ty
    )?;
    if let Some(location) = &system.location {
        write!(out, "  {location}")?;
    }
    writeln!(out, "{muted:#}")?;
    write_section(out, &PARAM_COLUMNS, params(&system.params), theme)?;
    let inputs = system
        .inputs
        .iter()
        .map(|input| vec![input.port.clone(), sources(input)]);
    write_section(out, &INPUT_COLUMNS, inputs.collect(), theme)?;
    let grants = system
        .grants
        .iter()
        .map(|(param, resource)| vec![param.clone(), resource.clone()]);
    write_section(out, &GRANT_COLUMNS, grants.collect(), theme)
}

/// A blank line and a table, unless it has no rows.
fn write_section(
    out: &mut impl Write,
    columns: &[(&str, Align)],
    rows: Vec<Vec<String>>,
    theme: &Theme,
) -> io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    table::write(out, columns, rows, theme, None, "    ")
}

const SYSTEM_COLUMNS: [(&str, Align); 3] = [
    ("SYSTEM", Align::Left),
    ("KIND", Align::Left),
    ("INPUTS", Align::Left),
];
const PARAM_COLUMNS: [(&str, Align); 2] = [("PARAM", Align::Left), ("VALUE", Align::Left)];
const INPUT_COLUMNS: [(&str, Align); 2] = [("INPUT", Align::Left), ("FROM", Align::Left)];
const GRANT_COLUMNS: [(&str, Align); 2] = [("GRANT", Align::Left), ("RESOURCE", Align::Left)];

/// `fsw.db (deleted on exit)` for a db whose database goes when the run
/// stops, otherwise the system's type.
fn kind(system: &SystemConfig) -> String {
    #[cfg(feature = "db")]
    if system.ty == crate::link::DB_TYPE
        && serde_json::from_value::<crate::link::DbParams>(system.params.clone())
            .is_ok_and(|params| params.on_exit() == crate::link::OnExit::Delete)
    {
        return format!("{} (deleted on exit)", system.ty);
    }
    system.ty.clone()
}

/// `sensors ← plant.sensors, plant.gps`: an input named after its source,
/// as a link's are, shows the source alone.
fn inputs(system: &SystemConfig) -> String {
    let from = glyph::FROM;
    let inputs: Vec<String> = system
        .inputs
        .iter()
        .map(|input| match sources(input) {
            source if source == input.port => source,
            source => format!("{} {from} {source}", input.port),
        })
        .collect();
    inputs.join(", ")
}

/// `plant.sensors`, several joined by `+`, or `unwired`.
fn sources(input: &InputConfig) -> String {
    if input.from.is_empty() {
        return "unwired".into();
    }
    let sources: Vec<String> = input
        .from
        .iter()
        .map(|from| format!("{}.{}", from.system, from.port))
        .collect();
    sources.join(" + ")
}

/// One row per param, each value as compact JSON.
fn params(params: &serde_json::Value) -> Vec<Vec<String>> {
    match params {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| vec![key.clone(), value.to_string()])
            .collect(),
        other => vec![vec!["(params)".into(), other.to_string()]],
    }
}

/// `100 Hz wall` or `10 ms sim`.
fn clock(clock: &Clock) -> String {
    match clock {
        Clock::Wall { rate } => format!("{} Hz wall", short(rate.get())),
        Clock::Simulated { dt, .. } => format!("{} ms sim", short(dt.as_secs_f64() * 1e3)),
    }
}

/// `x` to at most three decimals, without trailing zeros.
fn short(x: f64) -> String {
    let text = format!("{x:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::caps::Caps;

    fn deployment() -> ResolvedDeployment {
        let json = serde_json::json!({
            "config_version": 2,
            "targets": [
                {"packs": [], "coordinator": {
                    "clock": {"Wall": {"rate": 120.0}}, "ring_depth": 4, "namespace": "plant",
                    "systems": [{"id": "plant", "ty": "adcs.Plant", "params": {"seed": 42}}]}},
                {"packs": [], "coordinator": {
                    "clock": {"Simulated": {"dt": {"secs": 0, "nanos": 10000000}}}, "ring_depth": 4,
                    "namespace": "fsw",
                    "systems": [{"id": "nav", "ty": "adcs.Nav", "grants": {"uart": "gps_port"},
                        "inputs": [{"port": "sensors", "from": [{"system": "plant", "port": "sensors"}]}]}]}}
            ]
        });
        serde_json::from_value(json).expect("a deployment")
    }

    fn rendered(selection: Selection) -> String {
        let mut out = Vec::new();
        render(
            &mut out,
            &deployment(),
            selection,
            &Theme::new(&Caps::plain()),
            None,
        )
        .expect("writes");
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn test_render_everything() {
        insta::assert_snapshot!(rendered(Selection::All));
    }

    #[test]
    fn test_render_one_system() {
        insta::assert_snapshot!(rendered(Selection::System(1, 0)));
    }

    #[test]
    fn test_select_by_target_and_path() {
        let config = deployment();
        assert_eq!(select(&config, None).ok(), Some(Selection::All));
        assert_eq!(
            select(&config, Some("fsw")).ok(),
            Some(Selection::Target(1))
        );
        assert_eq!(
            select(&config, Some("fsw.nav")).ok(),
            Some(Selection::System(1, 0))
        );
        let Err(RunError::NoSuchName { known, .. }) = select(&config, Some("fsw.navv")) else {
            panic!("an unknown name is an error");
        };
        assert_eq!(known, ["plant", "plant.plant", "fsw", "fsw.nav"]);
        assert!(
            select(&config, Some("nav")).is_err(),
            "bare ids need one target"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn test_kind_notes_a_db_deleted_on_exit() {
        let db = |params: serde_json::Value| SystemConfig {
            params,
            ..SystemConfig::new("db", crate::link::DB_TYPE)
        };
        let listen = "127.0.0.1:2250";
        let temporary = db(serde_json::json!({"listen": listen, "path": null}));
        let at_path = db(serde_json::json!({"listen": listen, "path": "/data/db"}));
        let kept = db(serde_json::json!({"listen": listen, "on_exit": "keep"}));
        assert_eq!(kind(&temporary), "fsw.db (deleted on exit)");
        assert_eq!(kind(&at_path), "fsw.db");
        assert_eq!(kind(&kept), "fsw.db");
        assert_eq!(kind(&SystemConfig::new("nav", "adcs.Nav")), "adcs.Nav");
    }

    #[test]
    fn test_select_dotted_id() {
        let json = serde_json::json!({
            "config_version": 2,
            "targets": [{"packs": [], "coordinator": {
                "clock": {"Wall": {"rate": 120.0}}, "ring_depth": 4, "namespace": "fsw",
                "systems": [{"id": "gnc.nav", "ty": "adcs.Nav"}]}}]
        });
        let one: ResolvedDeployment = serde_json::from_value(json).expect("a deployment");
        let nav = Some(Selection::System(0, 0));
        assert_eq!(select(&one, Some("fsw.gnc.nav")).ok(), nav);
        assert_eq!(select(&one, Some("gnc.nav")).ok(), nav);
        assert!(select(&one, Some("gnc.navv")).is_err());
        assert!(
            select(&deployment(), Some("gnc.nav")).is_err(),
            "bare ids need one target"
        );
    }

    #[test]
    fn test_short() {
        assert_eq!(short(100.0), "100");
        assert_eq!(short(1000.0 / 30.0), "33.333");
        assert_eq!(short(0.5), "0.5");
    }

    #[test]
    fn test_split_args() {
        let file = "sub/t.py".to_string();
        assert_eq!(split_args(&[]), (None, None));
        assert_eq!(split_args(&["fsw".into()]), (None, Some("fsw".into())));
        assert_eq!(
            split_args(&["missing.py".into()]),
            (Some("missing.py".into()), None)
        );
        assert_eq!(
            split_args(&["deploy.json".into()]),
            (Some("deploy.json".into()), None)
        );
        assert_eq!(
            split_args(&["fsw.nav".into()]),
            (None, Some("fsw.nav".into()))
        );
        assert_eq!(
            split_args(std::slice::from_ref(&file)),
            (Some(file.clone().into()), None)
        );
        assert_eq!(
            split_args(&[file.clone(), "fsw".into()]),
            (Some(file.into()), Some("fsw".into()))
        );
    }
}
