use clap::{Parser, Subcommand};
use rho_agent::AgentRuntime;
use rho_tui::TuiClient;

#[derive(Debug, Parser)]
#[command(name = "rho", about = "rho CLI scaffold")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        bind: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
    },
    Tui {
        #[arg(long)]
        url: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            bind,
            provider,
            model,
        } => {
            let runtime = AgentRuntime::new();
            println!(
                "serve scaffold: bind={bind} provider={provider} model={model} protocol_version={}",
                runtime.protocol_version()
            );
        }
        Command::Tui { url } => {
            let client = TuiClient::new(url);
            println!("tui scaffold: url={}", client.url());
        }
    }
}
