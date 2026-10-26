use anyhow::{Result, ensure};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    contract: PathBuf,
    #[arg(long)]
    compiler_contract: PathBuf,
    #[arg(long)]
    compiler_id: String,
    #[arg(long)]
    compiler_address: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:0")]
    address: SocketAddr,
}
#[tokio::main(worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<()> {
    let args = Args::parse();
    let bundle = brick_contract::load(&args.contract)?;
    ensure!(
        brick_contract::contract_id(&bundle.source)?
            == brick_contract::contract_id(brick_generator::provider::SCHEMA)?,
        "generator contract mismatch"
    );
    let provider = brick_generator::provider::Generator::new(
        brick_contract::load(&args.compiler_contract)?,
        args.compiler_address,
        &args.compiler_id,
    )?;
    let mut server = brick_rpc::start(&bundle, Arc::new(provider), args.address).await?;
    println!(
        "{}",
        serde_json::json!({"address":server.address.to_string(),"contract_id":brick_contract::contract_id(&bundle.source)?})
    );
    tokio::signal::ctrl_c().await?;
    server.shutdown().await
}
