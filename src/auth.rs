//! Signing in to Microsoft in the browser, token refresh, and the refresh
//! token's home in the Keychain.
//!
//! The scopes are the safety boundary. `Mail.ReadWrite` lets the server read,
//! flag, draft and move; `Mail.Send` is never requested, so no token this
//! program holds can send mail, whatever the code above it does.
//!
//! Sign-in is the authorization-code flow for installed apps: the browser
//! opens Microsoft's page, Microsoft redirects back to a one-shot listener on
//! `127.0.0.1`, and the code is exchanged for tokens. PKCE ties the exchange to
//! this process, and the `state` value ties the redirect to this sign-in.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex;

/// The Microsoft identity platform, for work and personal accounts alike.
const AUTHORITY: &str = "https://login.microsoftonline.com/common/oauth2/v2.0";

/// What the token may do. No `Mail.Send`: sending is impossible, not merely
/// unimplemented.
pub const SCOPES: &str = "https://graph.microsoft.com/Mail.ReadWrite https://graph.microsoft.com/User.Read offline_access";

/// The Keychain service the refresh token is stored under.
const KEYCHAIN_SERVICE: &str = "plank-mail-mcp";

/// How long the browser has to come back before `login` gives up.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

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

/// `len` random bytes as URL-safe base64 without padding.
fn random_token(len: usize) -> Result<String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("no randomness available: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE `S256` challenge for `verifier`.
#[must_use]
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// The address the browser is sent to.
#[must_use]
pub fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    login_hint: Option<&str>,
) -> String {
    let mut url =
        reqwest::Url::parse(&format!("{AUTHORITY}/authorize")).expect("the authority URL parses");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", SCOPES)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(hint) = login_hint {
            q.append_pair("login_hint", hint);
        }
    }
    url.into()
}

/// What one request to the local listener carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// The authorization code, with a matching `state`.
    Code(String),
    /// Microsoft reported a failure, or the redirect was not for this sign-in.
    Failed(String),
    /// Not the redirect at all (a favicon request, say): keep listening.
    Other,
}

/// Reads the request line of an HTTP request to the listener.
#[must_use]
pub fn parse_callback(request: &str, state: &str) -> Callback {
    let Some(target) = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
    else {
        return Callback::Other;
    };
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
        return Callback::Other;
    };
    let get = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(error) = get("error") {
        return Callback::Failed(get("error_description").unwrap_or(error));
    }
    match (get("code"), get("state")) {
        (Some(code), Some(s)) if s == state => Callback::Code(code),
        (Some(_), _) => Callback::Failed(
            "the redirect did not come from this sign-in (state mismatch)".to_owned(),
        ),
        _ => Callback::Other,
    }
}

const PAGE_DONE: &str = "<!doctype html><meta charset=utf-8><title>plank-mail-mcp</title><body style=\"font-family:system-ui;margin:3em\"><h1>Signed in</h1><p>You can close this tab and go back to the terminal.</p>";
const PAGE_FAILED: &str = "<!doctype html><meta charset=utf-8><title>plank-mail-mcp</title><body style=\"font-family:system-ui;margin:3em\"><h1>Sign-in failed</h1><p>The terminal says why.</p>";

/// Answers the browser with `page` and closes the connection.
async fn respond(stream: &mut tokio::net::TcpStream, page: &str) {
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Waits on `listener` for the redirect carrying the code.
async fn wait_for_code(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        match parse_callback(&request, state) {
            Callback::Code(code) => {
                respond(&mut stream, PAGE_DONE).await;
                return Ok(code);
            }
            Callback::Failed(why) => {
                respond(&mut stream, PAGE_FAILED).await;
                bail!(why);
            }
            Callback::Other => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        }
    }
}

/// Signs in in the browser and stores the refresh token. Returns the access
/// token of the new session.
///
/// # Errors
/// No browser, a declined or timed-out sign-in, a redirect that does not
/// match, or a failed code exchange.
pub async fn login(
    http: &reqwest::Client,
    client_id: &str,
    account: &str,
    login_hint: Option<&str>,
) -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("cannot open a local port for the sign-in redirect")?;
    let redirect_uri = format!("http://localhost:{}", listener.local_addr()?.port());
    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let url = authorize_url(
        client_id,
        &redirect_uri,
        &state,
        &pkce_challenge(&verifier),
        login_hint,
    );
    let opened = std::process::Command::new("open")
        .arg(&url)
        .status()
        .is_ok_and(|s| s.success());
    if opened {
        eprintln!("Your browser is open at the Microsoft sign-in page; finish there.");
    } else {
        eprintln!("Open this address in a browser to sign in:\n{url}");
    }
    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(listener, &state))
        .await
        .map_err(|_| anyhow!("the sign-in did not finish within five minutes"))??;
    let resp = http
        .post(format!("{AUTHORITY}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
            ("scope", SCOPES),
        ])
        .send()
        .await
        .context("cannot reach login.microsoftonline.com")?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Microsoft refused the sign-in: {body}");
    }
    let tokens: Tokens = resp.json().await?;
    let refresh = tokens
        .refresh_token
        .ok_or_else(|| anyhow!("Microsoft returned no refresh token"))?;
    store_refresh_token(account, &refresh)?;
    Ok(tokens.access_token)
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
    fn the_pkce_challenge_is_sha256_in_url_safe_base64() {
        // Expected value computed independently with Python's hashlib.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mJ92IG0osCwGAPJqWxnaWA3XQuzZMs"),
            "KJEAtFijzef3OWMnazFd42FLCnR6poyQveJl9A95EEk"
        );
        assert_eq!(random_token(48).expect("random").len(), 64);
        assert_ne!(random_token(24).expect("a"), random_token(24).expect("b"));
    }

    #[test]
    fn the_authorize_url_carries_pkce_state_and_scopes() {
        let url = authorize_url("cid", "http://localhost:5555", "st", "ch", Some("me@x.org"));
        let parsed = reqwest::Url::parse(&url).expect("url");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["redirect_uri"], "http://localhost:5555");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "st");
        assert_eq!(q["login_hint"], "me@x.org");
        assert!(!q["scope"].to_ascii_lowercase().contains("send"));
    }

    #[test]
    fn the_redirect_is_accepted_only_with_the_right_state() {
        assert_eq!(
            parse_callback("GET /?code=abc%2Fd&state=st HTTP/1.1\r\nHost: x\r\n", "st"),
            Callback::Code("abc/d".to_owned())
        );
        assert!(matches!(
            parse_callback("GET /?code=abc&state=forged HTTP/1.1", "st"),
            Callback::Failed(_)
        ));
        assert_eq!(
            parse_callback(
                "GET /?error=access_denied&error_description=User+declined HTTP/1.1",
                "st"
            ),
            Callback::Failed("User declined".to_owned())
        );
        assert_eq!(
            parse_callback("GET /favicon.ico HTTP/1.1", "st"),
            Callback::Other
        );
        assert_eq!(parse_callback("", "st"), Callback::Other);
    }
}
