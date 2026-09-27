//! What the server can do to a mailbox, and nothing more.
//!
//! The trait is the whole surface: there is no method that sends, deletes, or
//! moves mail anywhere except the `HAL-processed` folder, so no provider
//! implementation can be asked to.

use std::future::Future;

use anyhow::Result;
use serde::Serialize;

/// The folder [`Mailbox::move_to_processed`] moves into, created on first use.
pub const PROCESSED_FOLDER: &str = "HAL-processed";

/// A mail folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Folder {
    /// Display name.
    pub name: String,
    /// Unread messages in it.
    pub unread: u64,
    /// All messages in it.
    pub total: u64,
}

/// One line of a message listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The id the other tools take.
    pub id: String,
    /// Sender, as `Name <address>`.
    pub from: String,
    /// Subject line.
    pub subject: String,
    /// When it arrived, RFC 3339 in UTC.
    pub received: String,
    /// Whether it has been read.
    pub is_read: bool,
    /// The first part of the body, as the provider previews it.
    pub preview: String,
}

/// A whole message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// The id the other tools take.
    pub id: String,
    /// Sender, as `Name <address>`.
    pub from: String,
    /// Recipients.
    pub to: Vec<String>,
    /// Copied recipients.
    pub cc: Vec<String>,
    /// Subject line.
    pub subject: String,
    /// When it arrived, RFC 3339 in UTC.
    pub received: String,
    /// Whether it has been read.
    pub is_read: bool,
    /// The body as plain text, HTML converted, capped at [`BODY_LIMIT`].
    pub body: String,
    /// Attachment names; their contents are never fetched.
    pub attachments: Vec<String>,
}

/// The longest body [`Mailbox::read`] returns, in characters.
pub const BODY_LIMIT: usize = 20_000;

/// A draft to create. Drafts are only ever saved, never sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    /// Recipients; with `reply_to_id`, added to the reply's own.
    pub to: Vec<String>,
    /// Copied recipients.
    pub cc: Vec<String>,
    /// Subject; ignored for a reply, which keeps the thread's.
    pub subject: Option<String>,
    /// The text of the message.
    pub body: String,
    /// When set, the draft is a reply to this message.
    pub reply_to_id: Option<String>,
}

/// The operations the tools are built on.
pub trait Mailbox: Send + Sync + 'static {
    /// Every folder.
    fn list_folders(&self) -> impl Future<Output = Result<Vec<Folder>>> + Send;
    /// The newest `limit` messages of `folder` (a display name, or a
    /// well-known name such as `inbox`).
    fn list_messages(
        &self,
        folder: &str,
        limit: u32,
        unread_only: bool,
    ) -> impl Future<Output = Result<Vec<Summary>>> + Send;
    /// Messages matching `query`, most relevant first.
    fn search(&self, query: &str, limit: u32) -> impl Future<Output = Result<Vec<Summary>>> + Send;
    /// One message in full.
    fn read(&self, id: &str) -> impl Future<Output = Result<Message>> + Send;
    /// Marks a message read or unread.
    fn set_read(&self, id: &str, read: bool) -> impl Future<Output = Result<()>> + Send;
    /// Saves a draft; returns its id.
    fn create_draft(&self, draft: Draft) -> impl Future<Output = Result<String>> + Send;
    /// Moves a message to [`PROCESSED_FOLDER`]; returns its new id.
    fn move_to_processed(&self, id: &str) -> impl Future<Output = Result<String>> + Send;
}

/// `html` as plain text, capped at [`BODY_LIMIT`] characters.
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let text = html2text::from_read(html.as_bytes(), 100).unwrap_or_else(|_| html.to_owned());
    cap(text.trim())
}

/// `text` cut to [`BODY_LIMIT`] characters, saying so when it was cut.
#[must_use]
pub fn cap(text: &str) -> String {
    if text.chars().count() <= BODY_LIMIT {
        return text.to_owned();
    }
    let kept: String = text.chars().take(BODY_LIMIT).collect();
    format!("{kept}\n[... truncated at {BODY_LIMIT} characters]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text() {
        let text = html_to_text("<p>Hello <b>there</b></p><ul><li>one</li><li>two</li></ul>");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("one") && text.contains("two"), "{text}");
        assert!(!text.contains('<'), "{text}");
    }

    #[test]
    fn a_long_body_is_capped_and_says_so() {
        let long = "x".repeat(BODY_LIMIT + 10);
        let out = cap(&long);
        assert!(out.ends_with("characters]"));
        assert_eq!(cap("short"), "short");
    }
}
