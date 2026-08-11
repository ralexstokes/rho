use std::{env, fs, path::PathBuf, sync::Arc};

use rho_ai::{Credential, CredentialError, CredentialSource, CredentialStore, ProviderId};

struct UserCredentialSource;

impl CredentialSource for UserCredentialSource {
    fn resolve(&self, provider: &ProviderId) -> Result<Credential, CredentialError> {
        if let Some(variable) = environment_variable(provider) {
            match env::var(variable) {
                Ok(value) if value.is_empty() => {
                    return Err(CredentialError::Empty {
                        provider: provider.clone(),
                    });
                }
                Ok(value) => return Ok(Credential::ApiKey(value)),
                Err(env::VarError::NotPresent) => {}
                Err(error) => return Err(unavailable(provider, error)),
            }
        }

        let path = credentials_path().map_err(|error| unavailable(provider, error))?;
        let bytes = fs::read(&path).map_err(|error| {
            unavailable(
                provider,
                format!("failed to read {}: {error}", path.display()),
            )
        })?;
        let store = CredentialStore::from_json(&bytes).map_err(|error| {
            unavailable(
                provider,
                format!("failed to parse {}: {error}", path.display()),
            )
        })?;
        store.resolve(provider)
    }
}

pub(crate) fn credential_source() -> Arc<dyn CredentialSource> {
    Arc::new(UserCredentialSource)
}

fn environment_variable(provider: &ProviderId) -> Option<&'static str> {
    match provider.as_str() {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

fn credentials_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("RHO_CREDENTIALS_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or_else(|| {
        "HOME is not set; set RHO_CREDENTIALS_FILE to a credentials file".to_owned()
    })?;
    Ok(PathBuf::from(home).join(".rho").join("credentials.json"))
}

fn unavailable(provider: &ProviderId, error: impl std::fmt::Display) -> CredentialError {
    CredentialError::Unavailable {
        provider: provider.clone(),
        message: error.to_string(),
    }
}
