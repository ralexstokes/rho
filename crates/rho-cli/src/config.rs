use std::{collections::BTreeMap, env, fs, path::PathBuf, time::Duration};

use anyhow::{Context as _, Result, bail};
use rho_ai::{ModelId, ProviderId, ThinkingLevel};
use rho_core::{CompactionConfig, MachineConfig, ModelRef, ToolSpec};
use rho_tools::McpStdioConfig;
use serde::Deserialize;

const DEFAULT_SYSTEM: &str = concat!(
    "You are rho, a headless coding agent. Work directly in the configured workspace. ",
    "Use tools to inspect relevant context before editing, make focused changes, validate them, ",
    "and report the outcome concisely. The execution environment, not rho, is the security boundary."
);
const DEFAULT_COMPACTION_SYSTEM: &str = concat!(
    "Summarize the conversation for another coding agent. Preserve decisions, constraints, ",
    "current state, exact identifiers and paths, failures, and remaining work."
);

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    provider: String,
    model: Option<String>,
    thinking: ThinkingLevel,
    max_output_tokens: u64,
    system: String,
    compaction: Option<FileCompaction>,
    mcp: Vec<FileMcpServer>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_owned(),
            model: None,
            thinking: ThinkingLevel::High,
            max_output_tokens: 16_384,
            system: DEFAULT_SYSTEM.to_owned(),
            compaction: Some(FileCompaction::default()),
            mcp: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileCompaction {
    threshold_tokens: u64,
    retain_messages: usize,
    system_prompt: String,
}

impl Default for FileCompaction {
    fn default() -> Self {
        Self {
            threshold_tokens: 100_000,
            retain_messages: 20,
            system_prompt: DEFAULT_COMPACTION_SYSTEM.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileMcpServer {
    name: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    request_timeout_seconds: u64,
    probe_timeout_millis: u64,
}

impl Default for FileMcpServer {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            request_timeout_seconds: 60,
            probe_timeout_millis: 1_000,
        }
    }
}

/// Fully resolved host policy shared by one-shot and RPC modes.
#[derive(Clone, Debug)]
pub(crate) struct HostConfig {
    pub(crate) model: ModelRef,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) max_output_tokens: u64,
    pub(crate) system: String,
    pub(crate) compaction: Option<CompactionConfig>,
    pub(crate) mcp: Vec<McpTemplate>,
}

#[derive(Clone, Debug)]
pub(crate) struct McpTemplate {
    config: McpStdioConfig,
    inherit_session_cwd: bool,
}

impl McpTemplate {
    pub(crate) fn for_session(&self, cwd: &str) -> McpStdioConfig {
        let mut config = self.config.clone();
        if self.inherit_session_cwd {
            config.cwd = Some(PathBuf::from(cwd));
        }
        config
    }
}

impl HostConfig {
    pub(crate) fn load(path: Option<PathBuf>) -> Result<Self> {
        let explicit = path.is_some() || env::var_os("RHO_CONFIG_FILE").is_some();
        let path = match path {
            Some(path) => Some(path),
            None => default_config_path()?,
        };
        let file = match path {
            Some(path) => match fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<FileConfig>(&bytes)
                    .with_context(|| format!("failed to parse {}", path.display()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => {
                    FileConfig::default()
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to read {}", path.display()));
                }
            },
            None => FileConfig::default(),
        };
        resolve(file)
    }

    pub(crate) fn machine(&self, tools: Vec<ToolSpec>) -> MachineConfig {
        MachineConfig {
            system: self.system.clone(),
            max_output_tokens: self.max_output_tokens,
            thinking: self.thinking,
            model: self.model.clone(),
            tools,
            hooks: Vec::new(),
            compaction: self.compaction.clone(),
        }
    }
}

pub(crate) fn default_sessions_dir(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path.or_else(|| env::var_os("RHO_SESSIONS_DIR").map(PathBuf::from))
    {
        return Ok(path);
    }
    Ok(rho_home()?.join("sessions"))
}

fn default_config_path() -> Result<Option<PathBuf>> {
    if let Some(path) = env::var_os("RHO_CONFIG_FILE") {
        return Ok(Some(PathBuf::from(path)));
    }
    Ok(Some(rho_home()?.join("config.json")))
}

fn rho_home() -> Result<PathBuf> {
    let home = env::var_os("HOME").context(
        "HOME is not set; pass --config/--sessions-dir or set RHO_CONFIG_FILE/RHO_SESSIONS_DIR",
    )?;
    Ok(PathBuf::from(home).join(".rho"))
}

fn resolve(file: FileConfig) -> Result<HostConfig> {
    if file.max_output_tokens == 0 {
        bail!("max_output_tokens must be greater than zero");
    }
    if file.system.trim().is_empty() {
        bail!("system must not be empty");
    }
    let provider = ProviderId::from(file.provider.as_str());
    let model = ModelId::from(match (file.provider.as_str(), file.model) {
        (_, Some(model)) if !model.is_empty() => model,
        ("openai", None) => "gpt-5.6-luna".to_owned(),
        ("anthropic", None) => "claude-sonnet-5".to_owned(),
        (unknown, None) => bail!("provider {unknown:?} requires an explicit model"),
        (_, Some(_)) => bail!("model must not be empty"),
    });
    if !matches!(provider.as_str(), "openai" | "anthropic") {
        bail!(
            "unsupported provider {:?}; expected openai or anthropic",
            provider.as_str()
        );
    }
    let compaction = file.compaction.map(|compaction| CompactionConfig {
        threshold_tokens: compaction.threshold_tokens,
        retain_messages: compaction.retain_messages,
        system_prompt: compaction.system_prompt,
    });
    if let Some(compaction) = &compaction
        && (compaction.threshold_tokens == 0
            || compaction.retain_messages == 0
            || compaction.system_prompt.trim().is_empty())
    {
        bail!("compaction threshold, retention, and system prompt must be non-empty");
    }
    let mut names = std::collections::BTreeSet::new();
    let mut mcp = Vec::with_capacity(file.mcp.len());
    for server in file.mcp {
        if server.name.is_empty()
            || server.name.len() > 24
            || !server
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            bail!("MCP server name must be 1..=24 ASCII letters, digits, underscores, or hyphens");
        }
        if server.command.is_empty() {
            bail!("MCP server {:?} has an empty command", server.name);
        }
        if !names.insert(server.name.clone()) {
            bail!("duplicate MCP server name {:?}", server.name);
        }
        if server.request_timeout_seconds == 0 || server.probe_timeout_millis == 0 {
            bail!("MCP server {:?} has a zero timeout", server.name);
        }
        let mut config = McpStdioConfig::new(server.name, server.command);
        config.args = server.args;
        config.env = server.env;
        let inherit_session_cwd = server.cwd.is_none();
        config.cwd = server.cwd;
        config.request_timeout = Duration::from_secs(server.request_timeout_seconds);
        config.probe_timeout = Duration::from_millis(server.probe_timeout_millis);
        mcp.push(McpTemplate {
            config,
            inherit_session_cwd,
        });
    }
    Ok(HostConfig {
        model: ModelRef { provider, model },
        thinking: file.thinking,
        max_output_tokens: file.max_output_tokens,
        system: file.system,
        compaction,
        mcp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_a_usable_openai_agent() {
        let config = resolve(FileConfig::default()).unwrap();
        assert_eq!(config.model.provider.as_str(), "openai");
        assert_eq!(config.model.model.as_str(), "gpt-5.6-luna");
        assert!(config.compaction.is_some());
    }

    #[test]
    fn config_is_strict_and_mcp_defaults_to_the_session_cwd() {
        let file: FileConfig = serde_json::from_value(serde_json::json!({
            "provider": "anthropic",
            "model": "claude-sonnet-5",
            "thinking": "medium",
            "mcp": [{"name": "git", "command": "mcp-git"}]
        }))
        .unwrap();
        let config = resolve(file).unwrap();
        assert_eq!(config.thinking, ThinkingLevel::Medium);
        assert_eq!(
            config.mcp[0].for_session("/repo").cwd,
            Some(PathBuf::from("/repo"))
        );
        assert!(
            serde_json::from_value::<FileConfig>(serde_json::json!({"unknown": true})).is_err()
        );
    }

    #[test]
    fn an_explicit_missing_config_is_an_error() {
        let missing = std::env::temp_dir().join(format!(
            "rho-config-that-does-not-exist-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        assert!(HostConfig::load(Some(missing)).is_err());
    }
}
