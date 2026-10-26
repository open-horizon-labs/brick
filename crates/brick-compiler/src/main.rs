use anyhow::{Result, ensure};
use clap::Parser;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    contract: PathBuf,
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
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
async fn run() -> Result<()> {
    let args = Args::parse();
    let bundle = brick_contract::load(&args.contract)?;
    ensure!(
        brick_contract::contract_id(&bundle.source)?
            == brick_contract::contract_id(brick_compiler::provider::SCHEMA)?,
        "compiler contract mismatch"
    );
    let cancel = Cancel(Arc::new(AtomicBool::new(false)));
    let token = cancel.0.clone();
    let compiler = tokio::select! {result=tokio::task::spawn_blocking(move ||brick_compiler::Compiler::discover_with_cancel(&token))=>result??,_=tokio::signal::ctrl_c()=>anyhow::bail!("startup cancelled")};
    let mut server = brick_rpc::start(
        &bundle,
        Arc::new(brick_compiler_contract::Adapter::new(
            brick_compiler::provider::Provider(compiler),
        )),
        args.address,
    )
    .await?;
    println!(
        "{}",
        serde_json::json!({"address":server.address.to_string(),"contract_id":brick_contract::contract_id(&bundle.source)?})
    );
    tokio::signal::ctrl_c().await?;
    server.shutdown().await
}
