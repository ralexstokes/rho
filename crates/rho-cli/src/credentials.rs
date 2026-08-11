use std::{env, fs, path::PathBuf};

use anyhow::{Context as _, Result, anyhow};
use rho_ai::{Credential, CredentialError, CredentialSource, CredentialStore, ProviderId};

struct EnvironmentCredentialSource;

impl CredentialSource for EnvironmentCredentialSource {
    fn resolve(&self, provider: &ProviderId) -> Result<Credential, CredentialError> {
        let Some(variable) = environment_variable(provider) else {
            return Err(CredentialError::Missing {
                provider: provider.clone(),
            });
        };
        let value = env::var(variable).map_err(|_| CredentialError::Missing {
            provider: provider.clone(),
        })?;
        if value.is_empty() {
            return Err(CredentialError::Empty {
                provider: provider.clone(),
            });
        }
        Ok(Credential::ApiKey(value))
    }
}

pub(crate) fn resolve_credential(provider: &ProviderId) -> Result<Credential> {
    match EnvironmentCredentialSource.resolve(provider) {
        Ok(credential) => return Ok(credential),
        Err(CredentialError::Missing { .. }) => {}
        Err(error) => return Err(error.into()),
    }

    let path = credentials_path()?;
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "no {} credential in the environment and failed to read {}",
            provider,
            path.display()
        )
    })?;
    let store = CredentialStore::from_json(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    store.resolve(provider).map_err(Into::into)
}

fn environment_variable(provider: &ProviderId) -> Option<&'static str> {
    match provider.as_str() {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

fn credentials_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("RHO_CREDENTIALS_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or_else(|| {
        anyhow!("HOME is not set; set RHO_CREDENTIALS_FILE to a credentials file")
    })?;
    Ok(PathBuf::from(home).join(".rho").join("credentials.json"))
}
