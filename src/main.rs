use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

mod client;
mod config;
mod mcp;
mod memory;
mod protocol;
mod provider;
mod server;
mod storage;
mod swarm;
mod tools;

#[derive(Parser)]
#[command(name = "jancode")]
#[command(about = "Lightweight AI coding agent daemon + CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Serve,
    Run {
        prompt: String,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(short, long)]
        tools: bool,
    },
    Connect,
    /// Swarm (multi-agent) management commands.
    #[command(subcommand)]
    Swarm(SwarmCommands),
}

#[derive(Subcommand)]
pub enum SwarmCommands {
    /// Spawn a new headless agent in the daemon.
    Spawn {
        prompt: String,
        #[arg(short, long)]
        label: Option<String>,
        #[arg(short, long)]
        parent: Option<String>,
        #[arg(short, long)]
        model: Option<String>,
    },
    /// List all swarm members.
    List,
    /// Show status of a session (or all sessions).
    Status {
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Send a direct message to another session.
    Dm {
        #[arg(short, long)]
        to: String,
        message: String,
    },
    /// Stop a session.
    Stop {
        #[arg(short, long)]
        force: bool,
        session: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();
    match cli.command {
        Commands::Serve => server::run().await,
        Commands::Run { prompt, model, tools } => client::run_prompt(&prompt, model, tools).await,
        Commands::Connect => client::connect().await,
        Commands::Swarm(sub) => client::handle_swarm(sub).await,
    }
}
