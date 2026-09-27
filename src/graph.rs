//! [`Mailbox`] over Microsoft Graph, for Outlook.
//!
//! Every request this file can make is listed in the table in the README:
//! reads, a `PATCH` of `isRead`, creating drafts, and a move whose destination
//! is always the `HAL-processed` folder. There is no `DELETE` and no `send`
//! anywhere here, and the token could not send even if there were.

use anyhow::{Context as _, Result, anyhow, bail};
use reqwest::{Method, Url};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::TokenSource;
use crate::mailbox::{
    Draft, Folder, Mailbox, Message, PROCESSED_FOLDER, Summary, cap, html_to_text,
};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// The fields a listing needs, so Graph does not send whole bodies.
const SUMMARY_FIELDS: &str = "id,from,subject,receivedDateTime,isRead,bodyPreview";

/// Folder names Graph accepts in place of an id.
const WELL_KNOWN: [&str; 6] = [
    "inbox",
    "drafts",
    "sentitems",
    "archive",
    "junkemail",
    "deleteditems",
];

/// The Outlook mailbox of whoever signed in.
pub struct GraphMailbox {
    http: reqwest::Client,
    tokens: TokenSource,
    processed_id: Mutex<Option<String>>,
}

impl GraphMailbox {
    /// A mailbox that signs its requests with `tokens`.
    #[must_use]
    pub fn new(http: reqwest::Client, tokens: TokenSource) -> Self {
        Self {
            http,
            tokens,
            processed_id: Mutex::new(None),
        }
    }

    /// Sends one request to `/me/<segments>` and returns the JSON answer
    /// (`Null` for an empty body).
    async fn call(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let url = graph_url(segments, query);
        let token = self.tokens.access_token().await?;
        let mut req = self.http.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .await
            .context("cannot reach graph.microsoft.com")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Outlook answered {status}: {}", graph_error(&text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("Outlook sent something that is not JSON")
    }

    /// The Graph id for `folder`: a well-known name as is, else the folder
    /// whose display name matches, ignoring case.
    async fn folder_id(&self, folder: &str) -> Result<String> {
        let lower = folder.to_ascii_lowercase();
        if WELL_KNOWN.contains(&lower.as_str()) {
            return Ok(lower);
        }
        let folders = self.raw_folders().await?;
        folders
            .iter()
            .find(|f| {
                f["displayName"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(folder))
            })
            .and_then(|f| f["id"].as_str().map(str::to_owned))
            .ok_or_else(|| anyhow!("no folder named {folder:?}; list_folders shows what there is"))
    }

    async fn raw_folders(&self) -> Result<Vec<Value>> {
        let v = self
            .call(
                Method::GET,
                &["mailFolders"],
                &[
                    ("$top", "200".to_owned()),
                    (
                        "$select",
                        "id,displayName,unreadItemCount,totalItemCount".to_owned(),
                    ),
                ],
                None,
            )
            .await?;
        Ok(v["value"].as_array().cloned().unwrap_or_default())
    }

    /// The `HAL-processed` folder's id, creating the folder the first time.
    async fn processed_folder(&self) -> Result<String> {
        let mut cached = self.processed_id.lock().await;
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let existing = self.raw_folders().await?.into_iter().find_map(|f| {
            (f["displayName"].as_str() == Some(PROCESSED_FOLDER))
                .then(|| f["id"].as_str().map(str::to_owned))
                .flatten()
        });
        let id = match existing {
            Some(id) => id,
            None => self
                .call(
                    Method::POST,
                    &["mailFolders"],
                    &[],
                    Some(json!({ "displayName": PROCESSED_FOLDER })),
                )
                .await?["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("Outlook created {PROCESSED_FOLDER} but returned no id"))?,
        };
        *cached = Some(id.clone());
        Ok(id)
    }
}

impl Mailbox for GraphMailbox {
    async fn list_folders(&self) -> Result<Vec<Folder>> {
        Ok(self.raw_folders().await?.iter().map(parse_folder).collect())
    }

    async fn list_messages(
        &self,
        folder: &str,
        limit: u32,
        unread_only: bool,
    ) -> Result<Vec<Summary>> {
        let id = self.folder_id(folder).await?;
        let v = self
            .call(
                Method::GET,
                &["mailFolders", &id, "messages"],
                &list_query(limit, unread_only),
                None,
            )
            .await?;
        Ok(parse_summaries(&v))
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Summary>> {
        let v = self
            .call(
                Method::GET,
                &["messages"],
                &[
                    ("$search", search_term(query)),
                    ("$top", limit.clamp(1, 50).to_string()),
                    ("$select", SUMMARY_FIELDS.to_owned()),
                ],
                None,
            )
            .await?;
        Ok(parse_summaries(&v))
    }

    async fn read(&self, id: &str) -> Result<Message> {
        let v = self
            .call(
                Method::GET,
                &["messages", id],
                &[(
                    "$select",
                    "id,from,toRecipients,ccRecipients,subject,receivedDateTime,isRead,body,hasAttachments"
                        .to_owned(),
                )],
                None,
            )
            .await?;
        let mut message = parse_message(&v);
        if v["hasAttachments"].as_bool() == Some(true) {
            let a = self
                .call(
                    Method::GET,
                    &["messages", id, "attachments"],
                    &[("$select", "name".to_owned())],
                    None,
                )
                .await?;
            message.attachments = a["value"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
        }
        Ok(message)
    }

    async fn set_read(&self, id: &str, read: bool) -> Result<()> {
        self.call(
            Method::PATCH,
            &["messages", id],
            &[],
            Some(json!({ "isRead": read })),
        )
        .await
        .map(|_| ())
    }

    async fn create_draft(&self, draft: Draft) -> Result<String> {
        let created = match draft.reply_to_id.as_deref() {
            Some(original) => {
                let reply = self
                    .call(
                        Method::POST,
                        &["messages", original, "createReply"],
                        &[],
                        Some(json!({ "comment": draft.body })),
                    )
                    .await?;
                if !draft.to.is_empty() || !draft.cc.is_empty() {
                    let id = reply["id"].as_str().unwrap_or_default().to_owned();
                    let mut to = recipients_of(&reply["toRecipients"]);
                    to.extend(draft.to.iter().cloned());
                    let mut cc = recipients_of(&reply["ccRecipients"]);
                    cc.extend(draft.cc.iter().cloned());
                    self.call(
                        Method::PATCH,
                        &["messages", &id],
                        &[],
                        Some(json!({ "toRecipients": recipients(&to), "ccRecipients": recipients(&cc) })),
                    )
                    .await?;
                }
                reply
            }
            None => {
                self.call(
                    Method::POST,
                    &["messages"],
                    &[],
                    Some(new_draft_body(&draft)),
                )
                .await?
            }
        };
        created["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Outlook saved the draft but returned no id"))
    }

    async fn move_to_processed(&self, id: &str) -> Result<String> {
        let destination = self.processed_folder().await?;
        let moved = self
            .call(
                Method::POST,
                &["messages", id, "move"],
                &[],
                Some(json!({ "destinationId": destination })),
            )
            .await?;
        Ok(moved["id"].as_str().unwrap_or(id).to_owned())
    }
}

/// `GRAPH/me/<segments>?<query>`, each segment percent-encoded, so an id with
/// a `/` or `=` in it stays one path segment.
fn graph_url(segments: &[&str], query: &[(&str, String)]) -> Url {
    let mut url = Url::parse(GRAPH).expect("the Graph base URL parses");
    url.path_segments_mut()
        .expect("an https URL has path segments")
        .push("me")
        .extend(segments);
    if !query.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
    }
    url
}

/// The query for a folder listing. Graph refuses an `$orderby` field that the
/// `$filter` does not name first, so the unread filter carries a
/// `receivedDateTime` clause that matches everything.
fn list_query(limit: u32, unread_only: bool) -> Vec<(&'static str, String)> {
    let mut q = vec![
        ("$top", limit.clamp(1, 50).to_string()),
        ("$select", SUMMARY_FIELDS.to_owned()),
        ("$orderby", "receivedDateTime desc".to_owned()),
    ];
    if unread_only {
        q.push((
            "$filter",
            "receivedDateTime ge 1900-01-01T00:00:00Z and isRead eq false".to_owned(),
        ));
    }
    q
}

/// A `$search` value: Graph wants the whole term in double quotes.
fn search_term(query: &str) -> String {
    format!("\"{}\"", query.replace('"', ""))
}

fn graph_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

fn parse_folder(v: &Value) -> Folder {
    Folder {
        name: v["displayName"].as_str().unwrap_or_default().to_owned(),
        unread: v["unreadItemCount"].as_u64().unwrap_or(0),
        total: v["totalItemCount"].as_u64().unwrap_or(0),
    }
}

fn address(v: &Value) -> String {
    let email = &v["emailAddress"];
    let addr = email["address"].as_str().unwrap_or_default();
    match email["name"].as_str() {
        Some(name) if !name.is_empty() && name != addr => format!("{name} <{addr}>"),
        _ => addr.to_owned(),
    }
}

fn recipients_of(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|r| r["emailAddress"]["address"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn recipients(addresses: &[String]) -> Value {
    Value::Array(
        addresses
            .iter()
            .map(|a| json!({ "emailAddress": { "address": a } }))
            .collect(),
    )
}

fn parse_summaries(v: &Value) -> Vec<Summary> {
    v["value"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|m| Summary {
                    id: m["id"].as_str().unwrap_or_default().to_owned(),
                    from: address(&m["from"]),
                    subject: m["subject"].as_str().unwrap_or_default().to_owned(),
                    received: m["receivedDateTime"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    is_read: m["isRead"].as_bool().unwrap_or(false),
                    preview: m["bodyPreview"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_message(m: &Value) -> Message {
    let content = m["body"]["content"].as_str().unwrap_or_default();
    let body = if m["body"]["contentType"].as_str() == Some("html") {
        html_to_text(content)
    } else {
        cap(content.trim())
    };
    Message {
        id: m["id"].as_str().unwrap_or_default().to_owned(),
        from: address(&m["from"]),
        to: m["toRecipients"]
            .as_array()
            .map(|r| r.iter().map(address).collect())
            .unwrap_or_default(),
        cc: m["ccRecipients"]
            .as_array()
            .map(|r| r.iter().map(address).collect())
            .unwrap_or_default(),
        subject: m["subject"].as_str().unwrap_or_default().to_owned(),
        received: m["receivedDateTime"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        is_read: m["isRead"].as_bool().unwrap_or(false),
        body,
        attachments: Vec::new(),
    }
}

fn new_draft_body(draft: &Draft) -> Value {
    json!({
        "subject": draft.subject.clone().unwrap_or_default(),
        "body": { "contentType": "Text", "content": draft.body },
        "toRecipients": recipients(&draft.to),
        "ccRecipients": recipients(&draft.cc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_stay_one_path_segment() {
        let url = graph_url(&["messages", "AA/BB=="], &[]);
        assert_eq!(
            url.as_str(),
            "https://graph.microsoft.com/v1.0/me/messages/AA%2FBB=="
        );
    }

    #[test]
    fn the_unread_filter_names_the_order_field_first() {
        let q = list_query(500, true);
        assert!(q.contains(&("$top", "50".to_owned())), "limit is clamped");
        let filter = &q.iter().find(|(k, _)| *k == "$filter").expect("filter").1;
        assert!(filter.starts_with("receivedDateTime"), "{filter}");
        assert!(!list_query(10, false).iter().any(|(k, _)| *k == "$filter"));
    }

    #[test]
    fn a_search_term_is_quoted_once() {
        assert_eq!(search_term("invoice \"march\""), "\"invoice march\"");
    }

    #[test]
    fn a_listing_parses_into_summaries() {
        let v: Value = serde_json::from_str(
            r#"{"value":[{"id":"1","from":{"emailAddress":{"name":"Ada","address":"ada@x.org"}},"subject":"Hi","receivedDateTime":"2026-09-27T10:00:00Z","isRead":false,"bodyPreview":"Hello"}]}"#,
        )
        .expect("json");
        let s = parse_summaries(&v);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].from, "Ada <ada@x.org>");
        assert!(!s[0].is_read);
    }

    #[test]
    fn an_html_message_is_read_as_text() {
        let v: Value = serde_json::from_str(
            r#"{"id":"1","from":{"emailAddress":{"address":"a@x.org"}},"toRecipients":[{"emailAddress":{"name":"Me","address":"me@x.org"}}],"subject":"S","receivedDateTime":"t","isRead":true,"body":{"contentType":"html","content":"<p>Dear <b>you</b></p>"}}"#,
        )
        .expect("json");
        let m = parse_message(&v);
        assert_eq!(m.from, "a@x.org");
        assert_eq!(m.to, ["Me <me@x.org>"]);
        assert!(
            m.body.contains("Dear") && !m.body.contains('<'),
            "{}",
            m.body
        );
    }

    #[test]
    fn a_new_draft_is_plain_text_with_its_recipients() {
        let body = new_draft_body(&Draft {
            to: vec!["a@x.org".to_owned()],
            body: "hi".to_owned(),
            ..Draft::default()
        });
        assert_eq!(body["body"]["contentType"], "Text");
        assert_eq!(
            body["toRecipients"][0]["emailAddress"]["address"],
            "a@x.org"
        );
    }
}
