use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
#[derive(Parser)]
struct Args {
    bundle: PathBuf,
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
    let bundle = brick_contract::load(&args.bundle)?;
    let mut server = brick_rpc::start(
        &bundle,
        Arc::new(brick_rpc::Shadow::new(&bundle)?),
        args.address,
    )
    .await?;
    println!(
        "{}",
        serde_json::json!({"address":server.address.to_string(),"contract_id":brick_contract::contract_id(&bundle.source)?})
    );
    tokio::signal::ctrl_c().await?;
    server.shutdown().await?;
    server.assert_complete()
}
