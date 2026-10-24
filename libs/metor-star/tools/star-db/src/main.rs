//! `star-db build` writes the onboard DB; `star-db check` prints its counts
//! and how often a cone of the field's size holds fewer than four stars.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use metor_star::catalog::Db;
use metor_star::catalog::synthetic::uniform;
use metor_star::starcat::StarCat;
use star_db::{Opts, build_db};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Builds the DB from a render catalog.
    Build {
        #[arg(long, default_value = "libs/metor-star/data/starcat-g8.bin")]
        starcat: PathBuf,
        #[arg(long, default_value = "libs/metor-star/data/stars-u2000.stdb")]
        out: PathBuf,
        #[arg(long, default_value_t = Opts::default().n_stars)]
        stars: usize,
        /// The largest pair separation (deg).
        #[arg(long, default_value_t = Opts::default().fov_deg)]
        fov: f64,
        #[arg(long, default_value_t = Opts::default().face_n)]
        face_n: u32,
    },
    /// Prints a DB's counts and its sparse-sky fraction.
    Check {
        db: PathBuf,
        /// The cone radius (deg) the sparse-sky test uses.
        #[arg(long, default_value_t = 7.0)]
        radius: f64,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse().cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("star-db: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Build {
            starcat,
            out,
            stars,
            fov,
            face_n,
        } => {
            let bytes =
                std::fs::read(&starcat).map_err(|e| format!("{}: {e}", starcat.display()))?;
            let cat = StarCat::parse(&bytes).map_err(|e| format!("starcat: {e:?}"))?;
            let opts = Opts {
                n_stars: stars,
                fov_deg: fov,
                face_n,
            };
            let db = build_db(&cat, &opts).map_err(|e| format!("build: {e:?}"))?;
            std::fs::write(&out, &db).map_err(|e| format!("{}: {e}", out.display()))?;
            println!("wrote {} ({} B)", out.display(), db.len());
            Ok(())
        }
        Cmd::Check { db, radius } => {
            let bytes = std::fs::read(&db).map_err(|e| format!("{}: {e}", db.display()))?;
            let db = Db::parse(&bytes).map_err(|e| format!("db: {e:?}"))?;
            let h = db.header;
            println!(
                "{} B, {} stars, {} pairs, mag ≤ {:.2}, pairs < {:.2}°, epoch {}",
                bytes.len(),
                db.n_stars(),
                db.pairs().len(),
                h.mag_limit.get(),
                f64::from(h.fov_max_rad.get()).to_degrees(),
                h.epoch_jy.get(),
            );
            let axes = uniform(100_000, 1);
            let mut hits = Vec::new();
            let sparse = axes
                .iter()
                .filter(|axis| {
                    hits.clear();
                    db.cone(**axis, radius.to_radians(), &mut hits);
                    hits.len() < 4
                })
                .count();
            println!(
                "P(< 4 stars within {radius}°) = {:.4}% over {} boresights",
                100.0 * sparse as f64 / axes.len() as f64,
                axes.len()
            );
            Ok(())
        }
    }
}
