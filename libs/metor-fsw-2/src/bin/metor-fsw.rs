//! The `metor-fsw` binary. Everything happens in [`metor_fsw_2::cli::main`];
//! this shim only forwards to it, so a mission crate whose `main` is the same
//! one line gets an identical command-line interface.

fn main() {
    metor_fsw_2::cli::main()
}
