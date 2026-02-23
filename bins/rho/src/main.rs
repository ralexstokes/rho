use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use clap::{Args, Parser, Subcommand, ValueEnum};
use rho_agent::{AgentRuntime, AgentServer, AgentServerError, build_provider};
use rho_core::{
    protocol::PROTOCOL_VERSION,
    providers::{ModelKind, ProviderKind},
};
use rho_tui::TuiClient;
use tokio::net::TcpListener;
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

fn parse_bind_address(bind: &str) -> Result<SocketAddr, AgentServerError> {
    bind.parse::<SocketAddr>()
        .map_err(|error| AgentServerError::InvalidBindAddress {
            bind: bind.to_string(),
            error,
        })
}

fn websocket_client_addr(listener_addr: SocketAddr) -> SocketAddr {
    let ip = match listener_addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listener_addr.port())
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
    let bind_addr = parse_bind_address(&bind)?;
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(AgentServerError::Bind)?;
    let listen_addr = listener.local_addr().map_err(AgentServerError::Bind)?;
    let connect_addr = websocket_client_addr(listen_addr);
    let url = format!("ws://{connect_addr}/ws");

    let (provider_kind, server) = build_server(provider, model)?;
    info!(
        listen_addr = %listen_addr,
        provider = ?provider_kind,
        protocol_version = PROTOCOL_VERSION,
        tui_url = %url,
        "rho running local mode"
    );

    let server_task = tokio::spawn(server.serve_with_listener(listener));
    let tui_result = TuiClient::new(url).run().await;

    server_task.abort();
    let server_result = server_task.await;

    tui_result?;

    match server_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Box::new(error)),
        Err(join_error) if join_error.is_cancelled() => Ok(()),
        Err(join_error) => Err(Box::new(join_error)),
    }
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

    #[test]
    fn websocket_client_addr_rewrites_unspecified_ipv4() {
        let listener_addr: SocketAddr = "0.0.0.0:8787".parse().expect("valid socket address");
        let client_addr = websocket_client_addr(listener_addr);
        assert_eq!(client_addr.to_string(), "127.0.0.1:8787");
    }

    #[test]
    fn websocket_client_addr_rewrites_unspecified_ipv6() {
        let listener_addr: SocketAddr = "[::]:8787".parse().expect("valid socket address");
        let client_addr = websocket_client_addr(listener_addr);
        assert_eq!(client_addr.to_string(), "[::1]:8787");
    }
}
