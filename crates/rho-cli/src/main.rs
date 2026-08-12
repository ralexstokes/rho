//! `rho`: one-shot coding agent and versioned headless RPC host.

#![allow(clippy::disallowed_methods)]

mod config;
mod credentials;
mod host;

use std::{
    env,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};
use config::{HostConfig, default_sessions_dir};
use host::{HeadlessHost, RunOutput, run_once};

#[derive(Debug, Eq, PartialEq)]
struct Cli {
    config: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
    command: Command,
}

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Run {
        cwd: Option<PathBuf>,
        prompt: String,
        output: RunOutput,
    },
    Rpc {
        listen: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_args(env::args().skip(1))?;
    let config = HostConfig::load(cli.config)?;
    let sessions = default_sessions_dir(cli.sessions_dir)?;
    match cli.command {
        Command::Run {
            cwd,
            prompt,
            output,
        } => {
            let cwd = cwd.unwrap_or(env::current_dir().context("resolve current directory")?);
            let cwd = cwd
                .canonicalize()
                .with_context(|| format!("resolve working directory {}", cwd.display()))?;
            let session = run_once(
                &sessions,
                config,
                cwd.to_string_lossy().into_owned(),
                prompt,
                output,
            )
            .await?;
            if output == RunOutput::Text {
                eprintln!("rho session: {session}");
            }
            Ok(())
        }
        Command::Rpc { listen: None } => serve_stdio(sessions, config).await,
        Command::Rpc { listen: Some(path) } => serve_unix(path, sessions, config).await,
    }
}

async fn serve_stdio(sessions: PathBuf, config: HostConfig) -> Result<()> {
    let host = Arc::new(HeadlessHost::new(sessions, config)?);
    let result = rho_rpc::serve(tokio::io::stdin(), tokio::io::stdout(), Arc::clone(&host)).await;
    host.shutdown().await;
    result.context("serve RPC over stdio")
}

async fn serve_unix(path: PathBuf, sessions: PathBuf, config: HostConfig) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("bind Unix socket {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict Unix socket {}", path.display()))?;
    let socket = UnixSocketGuard::new(path.clone())?;
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted.context("accept Unix RPC client")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                break;
            }
        };
        let host = Arc::new(HeadlessHost::new(sessions.clone(), config.clone())?);
        let (reader, writer) = stream.into_split();
        if let Err(error) = rho_rpc::serve(reader, writer, Arc::clone(&host)).await {
            eprintln!("rho RPC connection failed: {error}");
        }
        host.shutdown().await;
    }
    drop(listener);
    drop(socket);
    Ok(())
}

struct UnixSocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl UnixSocketGuard {
    fn new(path: PathBuf) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect Unix socket {}", path.display()))?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for UnixSocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut config = None;
    let mut sessions_dir = None;
    let mut json = false;
    let mut arguments = arguments.into_iter().peekable();
    loop {
        match arguments.peek().map(String::as_str) {
            Some("--config") => {
                arguments.next();
                config = Some(PathBuf::from(
                    arguments.next().context("--config requires a path")?,
                ));
            }
            Some("--sessions-dir") => {
                arguments.next();
                sessions_dir = Some(PathBuf::from(
                    arguments.next().context("--sessions-dir requires a path")?,
                ));
            }
            Some("--json") => {
                arguments.next();
                json = true;
            }
            Some("-h" | "--help") => {
                print_help();
                std::process::exit(0);
            }
            _ => break,
        }
    }
    let first = arguments.next();
    let command = match first.as_deref() {
        Some("rpc") => {
            if json {
                bail!("--json is only valid for one-shot runs");
            }
            let mut listen = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--listen" => {
                        listen = Some(PathBuf::from(
                            arguments.next().context("--listen requires a path")?,
                        ));
                    }
                    "-h" | "--help" => {
                        print_help();
                        std::process::exit(0);
                    }
                    unknown => bail!("unknown rpc option {unknown:?}"),
                }
            }
            Command::Rpc { listen }
        }
        Some("run") => parse_run(arguments, json)?,
        Some(option) if option.starts_with('-') => bail!("unknown option {option:?}"),
        Some(first) => {
            let mut words = vec![first.to_owned()];
            words.extend(arguments);
            Command::Run {
                cwd: None,
                prompt: words.join(" "),
                output: if json {
                    RunOutput::Json
                } else {
                    RunOutput::Text
                },
            }
        }
        None => bail!("a command or prompt is required; run rho --help for usage"),
    };
    Ok(Cli {
        config,
        sessions_dir,
        command,
    })
}

fn parse_run(arguments: impl IntoIterator<Item = String>, mut json: bool) -> Result<Command> {
    let mut cwd = None;
    let mut words = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--json" if words.is_empty() => json = true,
            "--cwd" if words.is_empty() => {
                cwd = Some(PathBuf::from(
                    arguments.next().context("--cwd requires a path")?,
                ));
            }
            option if option.starts_with('-') && words.is_empty() => {
                bail!("unknown run option {option:?}")
            }
            _ => {
                words.push(argument);
                words.extend(arguments);
                break;
            }
        }
    }
    if words.is_empty() {
        bail!("rho run requires a prompt");
    }
    Ok(Command::Run {
        cwd,
        prompt: words.join(" "),
        output: if json {
            RunOutput::Json
        } else {
            RunOutput::Text
        },
    })
}

fn print_help() {
    println!(
        "rho [--config PATH] [--sessions-dir PATH] [--json] [run [--cwd PATH]] PROMPT...\n\
         rho [--config PATH] [--sessions-dir PATH] rpc [--listen SOCKET]\n\n\
         With a prompt, creates a durable session and runs the coding agent to completion.\n\
         --json emits versioned agent events and a final authoritative snapshot as JSON Lines.\n\
         `rpc` serves versioned JSON Lines on stdio, or one controlling client at a time on\n\
         a Unix socket. Tools are unsandboxed; run rho inside the intended security boundary."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_alias_and_explicit_run_parse() {
        assert_eq!(
            parse_args(["inspect".to_owned(), "the repo".to_owned()])
                .unwrap()
                .command,
            Command::Run {
                cwd: None,
                prompt: "inspect the repo".to_owned(),
                output: RunOutput::Text,
            }
        );
        assert_eq!(
            parse_args([
                "run".to_owned(),
                "--cwd".to_owned(),
                "/repo".to_owned(),
                "fix".to_owned()
            ])
            .unwrap()
            .command,
            Command::Run {
                cwd: Some(PathBuf::from("/repo")),
                prompt: "fix".to_owned(),
                output: RunOutput::Text,
            }
        );

        assert_eq!(
            parse_args([
                "run".to_owned(),
                "--json".to_owned(),
                "summarize".to_owned()
            ])
            .unwrap()
            .command,
            Command::Run {
                cwd: None,
                prompt: "summarize".to_owned(),
                output: RunOutput::Json,
            }
        );
    }

    #[test]
    fn rpc_and_global_paths_parse() {
        let cli = parse_args([
            "--config".to_owned(),
            "config.json".to_owned(),
            "--sessions-dir".to_owned(),
            "sessions".to_owned(),
            "rpc".to_owned(),
            "--listen".to_owned(),
            "rho.sock".to_owned(),
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("config.json")));
        assert_eq!(cli.sessions_dir, Some(PathBuf::from("sessions")));
        assert_eq!(
            cli.command,
            Command::Rpc {
                listen: Some(PathBuf::from("rho.sock"))
            }
        );
    }

    #[test]
    fn socket_guard_removes_only_its_bound_socket() {
        let path = std::env::temp_dir().join(format!(
            "rho-socket-guard-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let guard = UnixSocketGuard::new(path.clone()).unwrap();
        drop(listener);
        drop(guard);
        assert!(!path.exists());
    }
}
