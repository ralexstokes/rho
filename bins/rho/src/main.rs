use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use clap::{Args, Parser, Subcommand, ValueEnum};
use rho_agent::{AgentRuntime, AgentServer, AgentServerError, build_provider};
use rho_core::{protocol::PROTOCOL_VERSION, providers::ProviderKind};
use rho_tui::TuiClient;
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(name = "rho", about = "rho CLI scaffold")]
struct Cli {
    #[command(flatten)]
    local: LocalModeArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Args)]
struct LocalModeArgs {
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,
    #[arg(long, value_enum, default_value_t = ProviderArg::Anthropic)]
    provider: ProviderArg,
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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
        Some(Command::Serve {
            bind,
            provider,
            model,
        }) => run_serve(bind, provider, model).await?,
        Some(Command::Tui { url }) => TuiClient::new(url).run().await?,
        None => run_local(cli.local).await?,
    }

    Ok(())
}

fn build_server(
    provider: ProviderArg,
    model: String,
) -> Result<(ProviderKind, AgentServer), AgentServerError> {
    let runtime = AgentRuntime::new();
    let provider_kind = provider.to_provider_kind();
    let provider_impl = build_provider(provider_kind)?;
    let server = AgentServer::new(runtime, provider_impl, model);
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

async fn run_serve(
    bind: String,
    provider: ProviderArg,
    model: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let (provider_kind, server) = build_server(provider, model)?;
    println!(
        "rho serve listening on {bind} with provider={provider_kind:?} protocol_version={}",
        PROTOCOL_VERSION
    );
    server.serve(bind).await?;
    Ok(())
}

async fn run_local(local: LocalModeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let bind_addr = parse_bind_address(&local.bind)?;
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(AgentServerError::Bind)?;
    let listen_addr = listener.local_addr().map_err(AgentServerError::Bind)?;
    let connect_addr = websocket_client_addr(listen_addr);
    let url = format!("ws://{connect_addr}/ws");

    let (provider_kind, server) = build_server(local.provider, local.model)?;
    println!(
        "rho running local mode on {listen_addr} with provider={provider_kind:?} protocol_version={} (tui={url})",
        PROTOCOL_VERSION
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
        assert_eq!(cli.local.bind, "127.0.0.1:0");
        assert_eq!(cli.local.provider, ProviderArg::Openai);
        assert_eq!(cli.local.model, "gpt-4o-mini");
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
