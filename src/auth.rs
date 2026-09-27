//! Signing in to Microsoft: the device-code flow, token refresh, and the
//! refresh token's home in the Keychain.
//!
//! The scopes are the safety boundary. `Mail.ReadWrite` lets the server read,
//! flag, draft and move; `Mail.Send` is never requested, so no token this
//! program holds can send mail, whatever the code above it does.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use tokio::sync::Mutex;

/// The Microsoft identity platform, for work and personal accounts alike.
const AUTHORITY: &str = "https://login.microsoftonline.com/common/oauth2/v2.0";

/// What the token may do. No `Mail.Send`: sending is impossible, not merely
/// unimplemented.
pub const SCOPES: &str = "https://graph.microsoft.com/Mail.ReadWrite https://graph.microsoft.com/User.Read offline_access";

/// The Keychain service the refresh token is stored under.
const KEYCHAIN_SERVICE: &str = "plank-mail-mcp";

/// The device-code endpoint's answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceAuthorization {
    /// Sent back while polling.
    pub device_code: String,
    /// What the user types at the verification page.
    pub user_code: String,
    /// Where they type it.
    pub verification_uri: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Microsoft's own one-line instruction for the user.
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

/// A successful token response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tokens {
    /// The bearer token for Graph.
    pub access_token: String,
    /// Seconds it is valid for.
    pub expires_in: u64,
    /// Present when `offline_access` was granted; may rotate on refresh.
    pub refresh_token: Option<String>,
}

/// What one poll of the token endpoint came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Signed in.
    Done(Tokens),
    /// The user has not approved yet; ask again after the interval.
    Pending,
    /// Asked too often; ask again after a longer interval.
    SlowDown,
    /// The flow ended without a sign-in.
    Failed(String),
}

/// An OAuth error answer.
#[derive(Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// Classifies a token-endpoint response body from the device-code poll.
#[must_use]
pub fn classify_poll(body: &str) -> Poll {
    if let Ok(tokens) = serde_json::from_str::<Tokens>(body) {
        return Poll::Done(tokens);
    }
    match serde_json::from_str::<OauthError>(body) {
        Ok(e) if e.error == "authorization_pending" => Poll::Pending,
        Ok(e) if e.error == "slow_down" => Poll::SlowDown,
        Ok(e) => Poll::Failed(match e.error.as_str() {
            "authorization_declined" => "the sign-in was declined".to_owned(),
            "expired_token" => "the code expired before the sign-in finished".to_owned(),
            _ => e.error_description.unwrap_or(e.error),
        }),
        Err(_) => Poll::Failed(format!("unexpected answer from Microsoft: {body}")),
    }
}

/// Runs the device-code flow: prints the code, waits for approval, and stores
/// the refresh token. Returns the access token of the new session.
///
/// # Errors
/// A network failure, a declined or expired sign-in, or no refresh token.
pub async fn login(http: &reqwest::Client, client_id: &str, account: &str) -> Result<String> {
    let code: DeviceAuthorization = http
        .post(format!("{AUTHORITY}/devicecode"))
        .form(&[("client_id", client_id), ("scope", SCOPES)])
        .send()
        .await
        .context("cannot reach login.microsoftonline.com")?
        .error_for_status()
        .context("Microsoft refused the sign-in request (is the client ID right?)")?
        .json()
        .await?;
    eprintln!("{}", code.message);
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = code.interval.max(1);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if Instant::now() > deadline {
            bail!("the code expired before the sign-in finished");
        }
        let body = http
            .post(format!("{AUTHORITY}/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", code.device_code.as_str()),
            ])
            .send()
            .await?
            .text()
            .await?;
        match classify_poll(&body) {
            Poll::Done(tokens) => {
                let refresh = tokens
                    .refresh_token
                    .ok_or_else(|| anyhow!("Microsoft returned no refresh token"))?;
                store_refresh_token(account, &refresh)?;
                return Ok(tokens.access_token);
            }
            Poll::Pending => {}
            Poll::SlowDown => interval += 5,
            Poll::Failed(why) => bail!(why),
        }
    }
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).context("cannot open the Keychain")
}

/// Saves the refresh token in the Keychain.
///
/// # Errors
/// The Keychain refused the write.
pub fn store_refresh_token(account: &str, token: &str) -> Result<()> {
    entry(account)?
        .set_password(token)
        .context("cannot save the sign-in to the Keychain")
}

/// The stored refresh token, or `None` when not signed in.
///
/// # Errors
/// The Keychain could not be read for a reason other than there being no
/// entry.
pub fn stored_refresh_token(account: &str) -> Result<Option<String>> {
    match entry(account)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("cannot read the sign-in from the Keychain"),
    }
}

/// Forgets the sign-in. Not being signed in is not an error.
///
/// # Errors
/// The Keychain refused the delete.
pub fn logout(account: &str) -> Result<()> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("cannot remove the sign-in from the Keychain"),
    }
}

/// The error every tool returns before `login` has run.
pub const NOT_SIGNED_IN: &str =
    "not signed in to Outlook; run `plank-mail-mcp login` in a terminal";

/// Hands out a valid access token, refreshing it from the Keychain's refresh
/// token when it is missing or about to expire.
pub struct TokenSource {
    http: reqwest::Client,
    client_id: String,
    account: String,
    current: Mutex<Option<(String, Instant)>>,
}

impl TokenSource {
    /// A source that has not fetched anything yet.
    #[must_use]
    pub fn new(http: reqwest::Client, client_id: String, account: String) -> Self {
        Self {
            http,
            client_id,
            account,
            current: Mutex::new(None),
        }
    }

    /// A valid access token.
    ///
    /// # Errors
    /// [`NOT_SIGNED_IN`] when there is no stored sign-in; a refresh failure
    /// otherwise.
    pub async fn access_token(&self) -> Result<String> {
        let mut current = self.current.lock().await;
        if let Some((token, expires)) = current.as_ref()
            && Instant::now() + Duration::from_secs(60) < *expires
        {
            return Ok(token.clone());
        }
        let refresh = stored_refresh_token(&self.account)?.ok_or_else(|| anyhow!(NOT_SIGNED_IN))?;
        let resp = self
            .http
            .post(format!("{AUTHORITY}/token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("refresh_token", refresh.as_str()),
                ("scope", SCOPES),
            ])
            .send()
            .await
            .context("cannot reach login.microsoftonline.com")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "the Outlook sign-in has expired or was revoked; run `plank-mail-mcp login` again ({body})"
            );
        }
        let tokens: Tokens = resp.json().await?;
        if let Some(rotated) = tokens.refresh_token.as_deref()
            && rotated != refresh
        {
            store_refresh_token(&self.account, rotated)?;
        }
        let expires = Instant::now() + Duration::from_secs(tokens.expires_in);
        *current = Some((tokens.access_token.clone(), expires));
        Ok(tokens.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scopes_never_include_sending() {
        assert!(!SCOPES.to_ascii_lowercase().contains("send"));
        assert!(SCOPES.contains("Mail.ReadWrite"));
        assert!(SCOPES.contains("offline_access"));
    }

    #[test]
    fn the_device_code_answer_parses() {
        let code: DeviceAuthorization = serde_json::from_str(
            r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://microsoft.com/devicelogin","expires_in":900,"interval":5,"message":"To sign in, use a web browser"}"#,
        )
        .expect("parses");
        assert_eq!(code.user_code, "ABCD-EFGH");
        assert_eq!(code.interval, 5);
    }

    #[test]
    fn polling_answers_are_classified() {
        assert_eq!(
            classify_poll(r#"{"error":"authorization_pending"}"#),
            Poll::Pending
        );
        assert_eq!(classify_poll(r#"{"error":"slow_down"}"#), Poll::SlowDown);
        assert_eq!(
            classify_poll(r#"{"error":"expired_token","error_description":"x"}"#),
            Poll::Failed("the code expired before the sign-in finished".to_owned())
        );
        let done = classify_poll(
            r#"{"access_token":"a","expires_in":3600,"refresh_token":"r","token_type":"Bearer"}"#,
        );
        assert!(matches!(done, Poll::Done(ref t) if t.refresh_token.as_deref() == Some("r")));
        assert!(matches!(classify_poll("<html>"), Poll::Failed(_)));
    }
}
