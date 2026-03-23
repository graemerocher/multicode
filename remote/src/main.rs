use clap::Parser;
use multicode_remote::{CliArgs, RemoteCliDependencies, RemoteCliOptions, run_remote_cli};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();
    run_remote_cli(
        args,
        RemoteCliOptions::default(),
        RemoteCliDependencies::default(),
    )
    .await?;
    Ok(())
}
