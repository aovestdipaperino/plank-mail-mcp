//! `plank-mail-mcp`: an MCP server over one Outlook mailbox for plank's HAL
//! profile. `serve` (the default) speaks MCP on stdio; `login`, `logout` and
//! `status` manage the sign-in from a terminal.

mod auth;
mod config;
mod graph;
mod mailbox;
mod server;

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use rmcp::ServiceExt as _;

use crate::config::Config;

const USAGE: &str = "usage: plank-mail-mcp [--config PATH] [serve|login|logout|status]";

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("plank-mail-mcp: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The parsed command line: the config path and the subcommand.
fn parse_args(args: &[String]) -> Result<(std::path::PathBuf, String)> {
    let mut config = config::default_path();
    let mut command = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                let path = args.get(i + 1).context("--config needs a path")?;
                config = config::expand_home(path);
                i += 1;
            }
            "-h" | "--help" => bail!(USAGE),
            c @ ("serve" | "login" | "logout" | "status") if command.is_none() => {
                command = Some(c.to_owned());
            }
            other => bail!("unexpected argument {other:?}\n{USAGE}"),
        }
        i += 1;
    }
    Ok((config, command.unwrap_or_else(|| "serve".to_owned())))
}

async fn run(args: Vec<String>) -> Result<()> {
    let (path, command) = parse_args(&args)?;
    let config = Config::load(&path)?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("plank-mail-mcp/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let account = config.keychain_account().to_owned();
    match command.as_str() {
        "login" => {
            auth::login(&http, config.client_id()?, &account).await?;
            eprintln!("signed in; HAL can read this mailbox now");
            Ok(())
        }
        "logout" => {
            auth::logout(&account)?;
            eprintln!("signed out");
            Ok(())
        }
        "status" => {
            let tokens =
                auth::TokenSource::new(http.clone(), config.client_id()?.to_owned(), account);
            let token = tokens.access_token().await?;
            let me: serde_json::Value = http
                .get("https://graph.microsoft.com/v1.0/me?$select=mail,userPrincipalName")
                .bearer_auth(token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let who = me["mail"]
                .as_str()
                .or(me["userPrincipalName"].as_str())
                .unwrap_or("?");
            eprintln!("signed in as {who}");
            Ok(())
        }
        _ => serve(http, &config, account).await,
    }
}

/// Speaks MCP on stdio until the client disconnects. A missing sign-in is not
/// fatal here: the server still starts, and every tool explains how to sign
/// in, which is more useful to the model than a server that failed to start.
async fn serve(http: reqwest::Client, config: &Config, account: String) -> Result<()> {
    let client_id = config.client_id()?.to_owned();
    let tokens = auth::TokenSource::new(http.clone(), client_id, account);
    let mailbox = graph::GraphMailbox::new(http, tokens);
    let running = server::MailServer::new(mailbox)
        .serve(rmcp::transport::stdio())
        .await
        .context("MCP handshake failed")?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn serve_is_the_default_command() {
        let (_, command) = parse_args(&args(&["--config", "/x/mail.toml"])).expect("parses");
        assert_eq!(command, "serve");
    }

    #[test]
    fn the_config_path_and_command_are_read() {
        let (path, command) =
            parse_args(&args(&["login", "--config", "/x/m.toml"])).expect("parses");
        assert_eq!(path, std::path::PathBuf::from("/x/m.toml"));
        assert_eq!(command, "login");
    }

    #[test]
    fn unknown_arguments_and_send_are_refused() {
        assert!(parse_args(&args(&["send"])).is_err());
        assert!(parse_args(&args(&["--config"])).is_err());
    }
}
