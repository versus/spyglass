//! API keys live in the OS keyring (Secret Service / Keychain / Credential Manager).
//! An `SPYGLASS_*` environment variable overrides the keyring (useful for CI).
//! Secret values are never printed.

use anyhow::{Result, bail};

const SERVICE: &str = "spyglass";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret {
    GithubToken,
    BraveApiKey,
    ExaApiKey,
}

impl Secret {
    pub const ALL: [Secret; 3] = [Secret::GithubToken, Secret::BraveApiKey, Secret::ExaApiKey];

    pub fn name(self) -> &'static str {
        match self {
            Secret::GithubToken => "github-token",
            Secret::BraveApiKey => "brave-api-key",
            Secret::ExaApiKey => "exa-api-key",
        }
    }

    pub fn env_var(self) -> &'static str {
        match self {
            Secret::GithubToken => "SPYGLASS_GITHUB_TOKEN",
            Secret::BraveApiKey => "SPYGLASS_BRAVE_API_KEY",
            Secret::ExaApiKey => "SPYGLASS_EXA_API_KEY",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        match Self::ALL.into_iter().find(|s| s.name() == name) {
            Some(s) => Ok(s),
            None => bail!("unknown secret {name:?}; known: github-token, brave-api-key, exa-api-key"),
        }
    }
}

/// Environment first, then keyring. Empty values count as unset.
pub fn get(secret: Secret) -> Option<String> {
    if let Some(v) = std::env::var(secret.env_var()).ok().filter(|v| !v.trim().is_empty()) {
        return Some(v.trim().to_string());
    }
    keyring::Entry::new(SERVICE, secret.name()).ok()?.get_password().ok().filter(|v| !v.is_empty())
}

pub fn set(secret: Secret, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        bail!("secret value is empty or contains control characters");
    }
    keyring::Entry::new(SERVICE, secret.name())?.set_password(value)?;
    Ok(())
}

pub fn remove(secret: Secret) -> Result<()> {
    keyring::Entry::new(SERVICE, secret.name())?.delete_credential()?;
    Ok(())
}

/// Keep API keys out of long-lived child processes (daemon, Chrome).
pub fn scrub_env(cmd: &mut std::process::Command) {
    for secret in Secret::ALL {
        cmd.env_remove(secret.env_var());
    }
}

/// Test helper: does `cmd` remove every secret variable from its environment?
#[cfg(test)]
pub fn scrubs_all(cmd: &std::process::Command) -> bool {
    let removed: Vec<_> = cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_owned()).collect();
    Secret::ALL.iter().all(|s| removed.iter().any(|k| k == s.env_var()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_env_removes_every_secret() {
        let mut cmd = std::process::Command::new("true");
        assert!(!scrubs_all(&cmd));
        scrub_env(&mut cmd);
        assert!(scrubs_all(&cmd));
    }

    #[test]
    fn names_round_trip() {
        for s in Secret::ALL {
            assert_eq!(Secret::parse(s.name()).unwrap(), s);
            assert!(s.env_var().starts_with("SPYGLASS_"));
        }
        assert_eq!(Secret::GithubToken.name(), "github-token");
        assert_eq!(Secret::BraveApiKey.env_var(), "SPYGLASS_BRAVE_API_KEY");
        assert!(Secret::parse("aws-secret").is_err());
    }
}
