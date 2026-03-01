use clap::{Args, Parser, Subcommand, ValueEnum};
use rho_agent::{AgentRuntime, AgentServer, AgentServerError, build_provider};
use rho_core::{
    protocol::PROTOCOL_VERSION,
    providers::{ModelKind, ProviderKind},
};
use rho_tui::TuiClient;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND: &str = "127.0.0.1:0";
const DEFAULT_MODEL: ModelArg = ModelArg::ClaudeSonnet46;
const DEFAULT_PROVIDER: ProviderArg = ProviderArg::Anthropic;

#[derive(Debug, Parser)]
#[command(name = "rho", about = "rho, an agent harness")]
struct Cli {
    #[command(flatten)]
    local: ServeArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Args)]
struct ServeArgs {
    #[arg(long, default_value = DEFAULT_BIND)]
    bind: String,
    #[arg(long, value_enum, default_value_t = DEFAULT_PROVIDER)]
    provider: ProviderArg,
    #[arg(long, value_enum, default_value_t = DEFAULT_MODEL)]
    model: ModelArg,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve(ServeArgs),
    Tui {
        #[arg(long)]
        url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProviderArg {
    Openai,
    Anthropic,
}

impl From<ProviderArg> for ProviderKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Openai => ProviderKind::OpenAi,
            ProviderArg::Anthropic => ProviderKind::Anthropic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelArg {
    #[value(name = "gpt-5.2-2025-12-11")]
    Gpt52,
    #[value(name = "claude-sonnet-4-6")]
    ClaudeSonnet46,
}

impl From<ModelArg> for ModelKind {
    fn from(value: ModelArg) -> Self {
        match value {
            ModelArg::Gpt52 => ModelKind::Gpt52,
            ModelArg::ClaudeSonnet46 => ModelKind::ClaudeSonnet46,
        }
    }
}

#[tokio::main]
async fn main() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    if let Err(error) = run().await {
        error!(%error, "rho exited with error");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Serve(serve)) => run_serve(serve).await?,
        Some(Command::Tui { url }) => TuiClient::new(url).run().await?,
        None => run_local(cli.local).await?,
    }

    Ok(())
}

fn build_server(
    provider: ProviderArg,
    model: ModelArg,
) -> Result<(ProviderKind, AgentServer), AgentServerError> {
    let runtime = AgentRuntime::new();
    let provider_kind: ProviderKind = provider.into();
    let model_kind: ModelKind = model.into();
    if model_kind.provider_kind() != provider_kind {
        return Err(AgentServerError::UnsupportedModelForProvider {
            provider: provider_kind,
            model: model_kind,
        });
    }
    let provider_impl = build_provider(provider_kind)?;
    let server = AgentServer::new(runtime, provider_impl, model_kind);
    Ok((provider_kind, server))
}

async fn run_serve(serve: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ServeArgs {
        bind,
        provider,
        model,
    } = serve;
    let (provider_kind, server) = build_server(provider, model)?;
    info!(
        bind = %bind,
        provider = ?provider_kind,
        protocol_version = PROTOCOL_VERSION,
        "rho serve listening"
    );
    server.serve(bind).await?;
    Ok(())
}

async fn run_local(local: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ServeArgs {
        bind,
        provider,
        model,
    } = local;
    let (provider_kind, server) = build_server(provider, model)?;
    if bind != DEFAULT_BIND {
        info!(bind = %bind, "local mode ignores --bind when using in-process transport");
    }
    info!(
        provider = ?provider_kind,
        protocol_version = PROTOCOL_VERSION,
        transport = "in-process",
        "rho running local mode"
    );
    let connection = server.connect_in_process();
    TuiClient::run_in_process(connection.client_events, connection.server_events).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_local_mode() {
        let cli = Cli::parse_from(["rho"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.local.bind, DEFAULT_BIND);
        assert_eq!(cli.local.provider, DEFAULT_PROVIDER);
        assert_eq!(cli.local.model, DEFAULT_MODEL);
    }

    #[test]
    fn serve_subcommand_parses_shared_args() {
        let cli = Cli::parse_from([
            "rho",
            "serve",
            "--bind",
            "0.0.0.0:8787",
            "--provider",
            "openai",
            "--model",
            "gpt-5.2-2025-12-11",
        ]);

        let Some(Command::Serve(serve)) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(serve.bind, "0.0.0.0:8787");
        assert_eq!(serve.provider, ProviderArg::Openai);
        assert_eq!(serve.model, ModelArg::Gpt52);
    }

    #[test]
    fn build_server_rejects_provider_model_mismatch() {
        let error = match build_server(ProviderArg::Openai, ModelArg::ClaudeSonnet46) {
            Ok(_) => panic!("mismatched provider/model should fail"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            AgentServerError::UnsupportedModelForProvider {
                provider: ProviderKind::OpenAi,
                model: ModelKind::ClaudeSonnet46,
            }
        ));
    }
}
