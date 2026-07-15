use metor_fsw_2::cli;

#[stellarator::main]
pub async fn main() -> miette::Result<()> {
    cli::run().await
}
