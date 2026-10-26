use anyhow::{Result, ensure};
use brick_runtime::Mode;
use clap::{Parser, Subcommand, ValueEnum};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "brick",
    version,
    about = "Standalone interfaces, runnable shadows, explicit evidence"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate a standalone definition and all declared examples.
    Check { definition: PathBuf },
    /// Generate a self-contained descriptor/bindings bundle. Never overwrites files.
    Generate {
        definition: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Run a generated endpoint. Loopback only; Ctrl-C stops it.
    Serve {
        bundle: PathBuf,
        #[arg(long, default_value = "127.0.0.1:0")]
        address: SocketAddr,
        #[arg(long, value_enum, default_value = "shadow")]
        mode: Provider,
    },
    /// Verify declared examples over a private gRPC server; print a JSON receipt.
    Verify {
        bundle: PathBuf,
        #[arg(long, value_enum, default_value = "shadow")]
        mode: Provider,
    },
    /// Invoke one method on an existing local endpoint using protobuf JSON.
    Call {
        bundle: PathBuf,
        address: SocketAddr,
        method: String,
        request_json: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Provider {
    Shadow,
    InterfaceCheck,
}
impl From<Provider> for Mode {
    fn from(value: Provider) -> Self {
        match value {
            Provider::Shadow => Self::Shadow,
            Provider::InterfaceCheck => Self::InterfaceCheck,
        }
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    if let Err(error) = run().await {
        eprintln!("brick: {error:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

async fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Check { definition } => {
            let bundle = brick_contract::build(&std::fs::read_to_string(definition)?)?;
            println!(
                "{}",
                serde_json::json!({"valid": true, "digest": bundle.digest})
            );
        }
        Commands::Generate { definition, output } => {
            let bundle = brick_contract::build(&std::fs::read_to_string(definition)?)?;
            let output = output.unwrap_or_else(|| brick_contract::default_output(&bundle));
            brick_contract::publish(&bundle, &output)?;
            println!("{}", output.display());
        }
        Commands::Serve {
            bundle,
            address,
            mode,
        } => {
            let bundle = brick_contract::load(&bundle)?;
            let mut server = brick_runtime::start(&bundle, mode.into(), address).await?;
            println!(
                "{}",
                serde_json::json!({"address": server.address.to_string(), "mode": Mode::from(mode), "digest": bundle.digest})
            );
            tokio::signal::ctrl_c().await?;
            server.shutdown().await?;
        }
        Commands::Verify { bundle, mode } => {
            let bundle = brick_contract::load(&bundle)?;
            let receipt = tokio::select! {
                result = brick_runtime::verify(&bundle, mode.into()) => result?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("verification cancelled"),
            };
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            ensure!(receipt.passed, "verification failed");
        }
        Commands::Call {
            bundle,
            address,
            method,
            request_json,
        } => {
            ensure!(
                address.ip().is_loopback(),
                "bootstrap calls are loopback only"
            );
            let response = brick_runtime::call(
                &brick_contract::load(&bundle)?,
                address,
                &method,
                &request_json,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
    }
    Ok(())
}
