//! `mail.toml`: which mailbox to serve and how to sign in to it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// The Microsoft Entra application plank-mail-mcp signs in as.
///
/// A public client (no secret), registered for any organisational directory
/// and personal Microsoft accounts, with public client flows enabled. Empty
/// until that registration exists; `client_id` in `mail.toml` overrides it.
pub const DEFAULT_CLIENT_ID: &str = "";

/// The one provider v1 serves.
pub const OUTLOOK: &str = "outlook";

/// The parsed config file. Every key is optional.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Config {
    /// `outlook`, the only value v1 accepts.
    pub provider: Option<String>,
    /// The mailbox address: a sign-in hint and the Keychain entry's name.
    pub account: Option<String>,
    /// Overrides [`DEFAULT_CLIENT_ID`].
    pub client_id: Option<String>,
}

impl Config {
    /// Reads `path`; a file that does not exist is the default config.
    ///
    /// # Errors
    /// An unreadable or invalid file, or a provider other than `outlook`.
    pub fn load(path: &Path) -> Result<Self> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("in {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Ok(config)
    }

    /// Parses config text.
    ///
    /// # Errors
    /// Invalid TOML, an unknown key, or a provider other than `outlook`.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        if let Some(p) = config.provider.as_deref()
            && p != OUTLOOK
        {
            bail!("provider {p:?} is not supported yet; only \"outlook\" is");
        }
        Ok(config)
    }

    /// The client ID to sign in with.
    ///
    /// # Errors
    /// When there is neither a built-in ID nor a `client_id` override.
    pub fn client_id(&self) -> Result<&str> {
        let id = self.client_id.as_deref().unwrap_or(DEFAULT_CLIENT_ID);
        if id.trim().is_empty() {
            bail!(
                "no Microsoft client ID: this build has none built in, so set client_id in mail.toml"
            );
        }
        Ok(id.trim())
    }

    /// The name the refresh token is stored under in the Keychain.
    #[must_use]
    pub fn keychain_account(&self) -> &str {
        self.account.as_deref().unwrap_or("default")
    }
}

/// Expands a leading `~/` against `$HOME`, since the path usually arrives from
/// a `.mcp.json` argument that no shell ever expanded.
#[must_use]
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// The config path used when `--config` is not given.
#[must_use]
pub fn default_path() -> PathBuf {
    expand_home("~/.plank/hal/mail.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_optional() {
        assert_eq!(Config::parse("").expect("empty parses"), Config::default());
        let c = Config::parse(
            "provider = \"outlook\"\naccount = \"me@outlook.com\"\nclient_id = \"abc\"\n",
        )
        .expect("parses");
        assert_eq!(c.account.as_deref(), Some("me@outlook.com"));
        assert_eq!(c.client_id().expect("id"), "abc");
        assert_eq!(c.keychain_account(), "me@outlook.com");
    }

    #[test]
    fn gmail_and_unknown_keys_are_refused() {
        assert!(Config::parse("provider = \"gmail\"").is_err());
        assert!(Config::parse("send = true").is_err());
    }

    #[test]
    fn a_missing_client_id_is_a_clear_error() {
        if DEFAULT_CLIENT_ID.is_empty() {
            let err = Config::default().client_id().expect_err("no id");
            assert!(err.to_string().contains("client_id"), "{err}");
        }
    }

    #[test]
    fn a_missing_file_is_the_default_config() {
        let path = std::env::temp_dir().join("plank-mail-mcp-no-such-config.toml");
        assert_eq!(Config::load(&path).expect("defaults"), Config::default());
    }

    #[test]
    fn a_leading_tilde_is_expanded() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand_home("~/x/y"), PathBuf::from(home).join("x/y"));
        }
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
