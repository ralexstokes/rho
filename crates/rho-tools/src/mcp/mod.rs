//! MCP stdio client and [`Tool`] adapter.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use rho_ai::{CancellationToken, ToolDefinition, validate_tool_definition};
use rho_core::ReplaySafety;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, Command},
};

use crate::{Tool, ToolFuture, ToolOutput};

mod transport;
mod wire;

use transport::Transport;
use wire::{ListToolsResult, RemoteTool, tool_output};

const CURRENT_PROTOCOL: &str = "2026-07-28";
const LEGACY_PROTOCOL: &str = "2025-11-25";
const LEGACY_PROTOCOLS: &[&str] = &[LEGACY_PROTOCOL, "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_TOOL_PAGES: usize = 100;
const MAX_TOOLS: usize = 4096;

/// Configuration for one client-launched MCP stdio server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpStdioConfig {
    /// Stable name used to namespace the server's model-visible tools.
    pub name: String,
    /// Executable to launch directly, without a shell.
    pub command: String,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Environment entries added to or replacing inherited entries.
    pub env: BTreeMap<String, String>,
    /// Optional working directory for the server process.
    pub cwd: Option<PathBuf>,
    /// Maximum duration of initialization, listing, and tool-call requests.
    pub request_timeout: Duration,
    /// Maximum duration of the modern-era compatibility probe.
    pub probe_timeout: Duration,
}

impl McpStdioConfig {
    /// Creates a server configuration with conservative default timeouts.
    #[must_use]
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }
}

/// A running MCP server connection and its fixed-at-connect tool snapshot.
pub struct McpConnection {
    inner: Arc<Client>,
    tools: Vec<McpTool>,
}

impl McpConnection {
    /// Launches a stdio server, negotiates the protocol era, and lists every tool page.
    pub async fn connect(config: McpStdioConfig) -> Result<Self, McpError> {
        validate_config(&config)?;
        let (transport, child) = spawn(&config).await?;
        let era = negotiate(&transport, &config).await?;
        let inner = Arc::new(Client {
            transport,
            _child: Mutex::new(child),
            era,
            request_timeout: config.request_timeout,
        });
        let remotes = list_tools(&inner).await?;
        let mut exposed = HashSet::with_capacity(remotes.len());
        let mut tools = Vec::with_capacity(remotes.len());
        for remote in remotes {
            let name = exposed_name(&config.name, &remote.name)?;
            if !exposed.insert(name.clone()) {
                return Err(McpError::InvalidTool {
                    name: remote.name,
                    message: format!("multiple remote names map to {name:?}"),
                });
            }
            let description = remote
                .description
                .clone()
                .or_else(|| remote.title.clone())
                .unwrap_or_else(|| {
                    format!("MCP tool {:?} from server {:?}.", remote.name, config.name)
                });
            let definition = ToolDefinition::new(name, description, remote.input_schema.clone());
            validate_tool_definition(&definition).map_err(|error| McpError::InvalidTool {
                name: remote.name.clone(),
                message: error.to_string(),
            })?;
            if remote.requires_task() {
                return Err(McpError::InvalidTool {
                    name: remote.name,
                    message: "requires task-augmented execution, which the simple client does not advertise"
                        .to_owned(),
                });
            }
            tools.push(McpTool {
                definition,
                remote_name: remote.name,
                server_name: config.name.clone(),
                client: Arc::clone(&inner),
            });
        }
        Ok(Self { inner, tools })
    }

    /// Returns the negotiated MCP protocol version.
    #[must_use]
    pub fn protocol_version(&self) -> &'static str {
        self.inner.era.version()
    }

    /// Returns the fixed tool snapshot obtained while connecting.
    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Returns recent MCP server stderr, bounded to 8 KiB.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.inner.transport.stderr_tail()
    }
}

/// One namespaced remote MCP tool.
#[derive(Clone)]
pub struct McpTool {
    definition: ToolDefinition,
    remote_name: String,
    server_name: String,
    client: Arc<Client>,
}

impl McpTool {
    /// Returns the server's original, un-namespaced tool name.
    #[must_use]
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    /// Returns the configured MCP server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

impl Tool for McpTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn replay_safety(&self) -> ReplaySafety {
        // Tool annotations are explicitly untrusted, and a process crash can make
        // an apparently read-only remote operation ambiguous.
        ReplaySafety::Never
    }

    fn execute(&self, arguments: Value, cancellation: CancellationToken) -> ToolFuture<'_> {
        Box::pin(async move {
            let params = self.client.era.params(json!({
                "name": self.remote_name,
                "arguments": arguments,
            }));
            match self
                .client
                .transport
                .request(
                    "tools/call",
                    params,
                    self.client.request_timeout,
                    Some(cancellation),
                )
                .await
            {
                Ok(result) => tool_output(result),
                Err(error) => ToolOutput::error(format!(
                    "MCP server {:?} failed to call tool {:?}: {error}",
                    self.server_name, self.remote_name
                )),
            }
        })
    }
}

struct Client {
    transport: Transport,
    // Retaining the child keeps kill-on-drop tied to the lifetime of cloned tools.
    _child: Mutex<Child>,
    era: Era,
    request_timeout: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Era {
    Current,
    Legacy(&'static str),
}

impl Era {
    fn version(self) -> &'static str {
        match self {
            Self::Current => CURRENT_PROTOCOL,
            Self::Legacy(version) => version,
        }
    }

    fn params(self, mut params: Value) -> Value {
        if self == Self::Current {
            params
                .as_object_mut()
                .expect("MCP params are objects")
                .insert(
                    "_meta".to_owned(),
                    json!({
                        "io.modelcontextprotocol/protocolVersion": CURRENT_PROTOCOL,
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "rho",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "io.modelcontextprotocol/clientCapabilities": {},
                    }),
                );
        }
        params
    }
}

async fn spawn(config: &McpStdioConfig) -> Result<(Transport, Child), McpError> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .envs(&config.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|error| McpError::Spawn {
        command: config.command.clone(),
        message: error.to_string(),
    })?;
    let stdin = child.stdin.take().ok_or_else(|| McpError::Transport {
        message: "spawned MCP server has no stdin".to_owned(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| McpError::Transport {
        message: "spawned MCP server has no stdout".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| McpError::Transport {
        message: "spawned MCP server has no stderr".to_owned(),
    })?;
    let transport = Transport::new(stdin, stdout);
    let stderr_tail = transport.stderr_buffer();
    tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr);
        let mut buffer = [0_u8; 1024];
        loop {
            let read = match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let mut tail = match stderr_tail.lock() {
                Ok(tail) => tail,
                Err(poisoned) => poisoned.into_inner(),
            };
            tail.extend_from_slice(&buffer[..read]);
            if tail.len() > 8 * 1024 {
                let drain = tail.len() - 8 * 1024;
                tail.drain(..drain);
            }
        }
    });
    Ok((transport, child))
}

async fn negotiate(transport: &Transport, config: &McpStdioConfig) -> Result<Era, McpError> {
    let probe = transport
        .request(
            "server/discover",
            Era::Current.params(json!({})),
            config.probe_timeout,
            None,
        )
        .await;
    match probe {
        Ok(result) => {
            let versions = result
                .get("supportedVersions")
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol {
                    message: "server/discover result omitted supportedVersions".to_owned(),
                })?;
            if result.pointer("/capabilities/tools").is_none() {
                return Err(McpError::Protocol {
                    message: "MCP server did not declare its tools capability".to_owned(),
                });
            }
            if versions.iter().any(|version| version == CURRENT_PROTOCOL) {
                Ok(Era::Current)
            } else {
                Err(McpError::UnsupportedProtocol {
                    supported: versions
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                })
            }
        }
        Err(McpError::Remote { code, data, .. }) if (-32_022..=-32_020).contains(&code) => {
            Err(McpError::UnsupportedProtocol {
                supported: data
                    .as_ref()
                    .and_then(|data| data.get("supported"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            })
        }
        Err(McpError::Remote { .. } | McpError::Timeout { .. }) => {
            transport.set_legacy();
            let version = initialize_legacy(transport, config.request_timeout).await?;
            Ok(Era::Legacy(version))
        }
        Err(error) => Err(error),
    }
}

async fn initialize_legacy(
    transport: &Transport,
    timeout: Duration,
) -> Result<&'static str, McpError> {
    let result = transport
        .request(
            "initialize",
            json!({
                "protocolVersion": LEGACY_PROTOCOL,
                "capabilities": {},
                "clientInfo": {
                    "name": "rho",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            timeout,
            None,
        )
        .await?;
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol {
            message: "initialize result omitted protocolVersion".to_owned(),
        })?;
    let version = LEGACY_PROTOCOLS
        .iter()
        .copied()
        .find(|supported| *supported == version)
        .ok_or_else(|| McpError::UnsupportedProtocol {
            supported: vec![version.to_owned()],
        })?;
    if result.pointer("/capabilities/tools").is_none() {
        return Err(McpError::Protocol {
            message: "MCP server did not declare its tools capability".to_owned(),
        });
    }
    transport.notify("notifications/initialized", None).await?;
    Ok(version)
}

async fn list_tools(client: &Arc<Client>) -> Result<Vec<RemoteTool>, McpError> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    for _ in 0..MAX_TOOL_PAGES {
        let params = client.era.params(match &cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        });
        let value = client
            .transport
            .request("tools/list", params, client.request_timeout, None)
            .await?;
        let page: ListToolsResult =
            serde_json::from_value(value).map_err(|error| McpError::Protocol {
                message: format!("invalid tools/list result: {error}"),
            })?;
        page.require_complete()?;
        tools.extend(page.tools);
        if tools.len() > MAX_TOOLS {
            return Err(McpError::Protocol {
                message: format!("MCP server exposed more than {MAX_TOOLS} tools"),
            });
        }
        let Some(next) = page.next_cursor else {
            let mut names = HashSet::with_capacity(tools.len());
            if let Some(duplicate) = tools.iter().find(|tool| !names.insert(tool.name.clone())) {
                return Err(McpError::InvalidTool {
                    name: duplicate.name.clone(),
                    message: "duplicate remote tool name".to_owned(),
                });
            }
            return Ok(tools);
        };
        if !seen_cursors.insert(next.clone()) {
            return Err(McpError::Protocol {
                message: format!("tools/list repeated cursor {next:?}"),
            });
        }
        cursor = Some(next);
    }
    Err(McpError::Protocol {
        message: format!("tools/list exceeded {MAX_TOOL_PAGES} pages"),
    })
}

fn validate_config(config: &McpStdioConfig) -> Result<(), McpError> {
    if config.command.is_empty() {
        return Err(McpError::InvalidConfig {
            message: "command must not be empty".to_owned(),
        });
    }
    if config.name.is_empty()
        || config.name.len() > 24
        || !config
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(McpError::InvalidConfig {
            message: "name must be 1..=24 ASCII letters, digits, underscores, or hyphens"
                .to_owned(),
        });
    }
    if config.request_timeout.is_zero() || config.probe_timeout.is_zero() {
        return Err(McpError::InvalidConfig {
            message: "request and probe timeouts must be positive".to_owned(),
        });
    }
    Ok(())
}

fn exposed_name(server: &str, remote: &str) -> Result<String, McpError> {
    if remote.is_empty()
        || remote.len() > 128
        || !remote
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(McpError::InvalidTool {
            name: remote.to_owned(),
            message: "name violates MCP's portable tool-name profile".to_owned(),
        });
    }
    let remote = remote.replace('.', "_");
    let exposed = format!("mcp__{server}__{remote}");
    if exposed.len() > 64 {
        return Err(McpError::InvalidTool {
            name: remote,
            message: format!("provider-compatible namespaced form exceeds 64 bytes: {exposed:?}"),
        });
    }
    Ok(exposed)
}

/// MCP connection, negotiation, or protocol failure.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// The local server configuration is unusable.
    #[error("invalid MCP configuration: {message}")]
    InvalidConfig { message: String },
    /// The stdio server could not be launched.
    #[error("failed to launch MCP command {command:?}: {message}")]
    Spawn { command: String, message: String },
    /// The byte transport failed or closed.
    #[error("MCP transport failed: {message}")]
    Transport { message: String },
    /// A response violated the negotiated protocol.
    #[error("MCP protocol error: {message}")]
    Protocol { message: String },
    /// The server returned a JSON-RPC error.
    #[error("MCP JSON-RPC error {code}: {message}")]
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// A request exceeded its configured deadline.
    #[error("MCP request {method:?} timed out")]
    Timeout { method: String },
    /// The caller cancelled a request.
    #[error("MCP request {method:?} was cancelled")]
    Cancelled { method: String },
    /// The server and client have no mutually supported protocol version.
    #[error(
        "MCP server does not support a compatible protocol version (advertised: {supported:?})"
    )]
    UnsupportedProtocol { supported: Vec<String> },
    /// A remote tool cannot be safely exposed to providers.
    #[error("invalid MCP tool {name:?}: {message}")]
    InvalidTool { name: String, message: String },
}

#[cfg(test)]
mod tests;
