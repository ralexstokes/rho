use rho_core::protocol::PROTOCOL_VERSION;

#[derive(Debug, Clone)]
pub struct AgentRuntime {
    protocol_version: u16,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    pub fn start_session(&self, session_id: impl Into<String>) -> AgentSession {
        AgentSession {
            id: session_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub id: String,
}
