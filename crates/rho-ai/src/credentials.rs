use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

use crate::ProviderId;

/// A resolved provider credential.
#[derive(Clone, Eq, PartialEq)]
pub enum Credential {
    /// A provider API key.
    ApiKey(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => formatter.write_str("ApiKey([REDACTED])"),
        }
    }
}

impl Credential {
    /// Borrows the API key without logging it.
    #[must_use]
    pub fn expose_api_key(&self) -> &str {
        match self {
            Self::ApiKey(value) => value,
        }
    }
}

/// Resolves credentials without coupling the provider boundary to an I/O source.
pub trait CredentialSource {
    /// Resolves one provider's credential.
    fn resolve(&self, provider: &ProviderId) -> Result<Credential, CredentialError>;
}

/// Credential-resolution failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialError {
    /// No credential was configured for the provider.
    #[error("no credential configured for provider {provider}")]
    Missing {
        /// Provider whose credential was requested.
        provider: ProviderId,
    },
    /// The configured credential was empty.
    #[error("credential configured for provider {provider} is empty")]
    Empty {
        /// Provider whose credential was empty.
        provider: ProviderId,
    },
}

/// Serializable credential-file entry.
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredCredential {
    /// An API key. Serialization is intended only for the user-owned credential file.
    ApiKey {
        /// Secret API key value.
        api_key: String,
    },
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { .. } => formatter.write_str("ApiKey { api_key: [REDACTED] }"),
        }
    }
}

/// Parsed, I/O-free representation of a credentials file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CredentialStore(BTreeMap<ProviderId, StoredCredential>);

impl CredentialStore {
    /// Parses credential-file JSON bytes. File access remains in the calling shell.
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Adds or replaces one credential.
    pub fn insert(&mut self, provider: ProviderId, credential: StoredCredential) {
        self.0.insert(provider, credential);
    }
}

impl CredentialSource for CredentialStore {
    fn resolve(&self, provider: &ProviderId) -> Result<Credential, CredentialError> {
        let stored = self
            .0
            .get(provider)
            .ok_or_else(|| CredentialError::Missing {
                provider: provider.clone(),
            })?;
        let value = match stored {
            StoredCredential::ApiKey { api_key } => api_key,
        };
        if value.is_empty() {
            return Err(CredentialError::Empty {
                provider: provider.clone(),
            });
        }
        Ok(Credential::ApiKey(value.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_never_exposes_secret() {
        let credential = Credential::ApiKey("secret-value".to_owned());
        let debug = format!("{credential:?}");
        assert!(!debug.contains("secret-value"));
        assert!(debug.contains("REDACTED"));

        let stored = StoredCredential::ApiKey {
            api_key: "stored-secret".to_owned(),
        };
        let debug = format!("{stored:?}");
        assert!(!debug.contains("stored-secret"));

        let mut store = CredentialStore::default();
        store.insert(
            ProviderId::from("test"),
            StoredCredential::ApiKey {
                api_key: "nested-secret".to_owned(),
            },
        );
        let debug = format!("{store:?}");
        assert!(!debug.contains("nested-secret"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn store_parses_file_shape_and_resolves() {
        let store =
            CredentialStore::from_json(br#"{"anthropic":{"type":"api_key","api_key":"test-key"}}"#)
                .unwrap();
        let credential = store.resolve(&ProviderId::from("anthropic")).unwrap();
        assert_eq!(credential.expose_api_key(), "test-key");
    }
}
