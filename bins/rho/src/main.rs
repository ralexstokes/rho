use clap::{Parser, Subcommand, ValueEnum};
use rho_agent::{AgentRuntime, AgentServer, build_provider};
use rho_core::providers::ProviderKind;
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
        #[arg(long, value_enum)]
        provider: ProviderArg,
        #[arg(long)]
        model: String,
    },
    Tui {
        #[arg(long)]
        url: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Openai,
    Anthropic,
}

impl ProviderArg {
    fn to_provider_kind(self) -> ProviderKind {
        match self {
            ProviderArg::Openai => ProviderKind::OpenAi,
            ProviderArg::Anthropic => ProviderKind::Anthropic,
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            bind,
            provider,
            model,
        } => {
            let runtime = AgentRuntime::new();
            let provider_kind = provider.to_provider_kind();
            let provider_impl = build_provider(provider_kind)?;
            let server = AgentServer::new(runtime.clone(), provider_impl, model);
            println!(
                "rho serve listening on {bind} with provider={provider_kind:?} protocol_version={}",
                runtime.protocol_version()
            );
            server.serve(bind).await?;
        }
        Command::Tui { url } => {
            let client = TuiClient::new(url);
            client.run().await?;
        }
    }

    Ok(())
}
