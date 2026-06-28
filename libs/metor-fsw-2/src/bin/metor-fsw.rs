//! The `metor-fsw` runner binary — a one-line delegate to the reusable CLI
//! (`cli-runner.md`). All logic lives in [`metor_fsw_2::cli`]; a mission host's own
//! `main` is the identical line, so `metor-fsw run …` and `cargo run -p <mission> -- run …`
//! behave the same.

fn main() {
    metor_fsw_2::cli::main()
}
