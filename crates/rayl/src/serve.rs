//! `rayl serve` — the MCP server, over stdio.
//!
//! Tool responses state their own limits. An agent that does not know what it cannot see
//! will confidently report that an email does not exist, so every response says whether
//! bodies were searched, whether a body was downloaded, and how much of the mailbox the
//! answer covers.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use rayl_core::MailStore;
use rayl_core::emlx::EmlxIndex;
use rayl_core::epoch::format_unix;
use rayl_core::search::{SearchHit, SearchQuery};

/// How many hits a search may return, however large a limit is requested. Keeps a single
/// tool call from consuming an agent's whole context.
const MAX_SEARCH_LIMIT: usize = 100;
const DEFAULT_SEARCH_LIMIT: usize = 20;
/// Cap on a single body. An agent wanting more can ask again with a larger `max_bytes`.
const DEFAULT_MAX_BYTES: usize = 4096;
const MAX_MAX_BYTES: usize = 262_144;

#[derive(Clone)]
pub struct RailServer {
    /// `None` when the mail store is unreachable. The server still starts in that case, so
    /// the reason reaches the client through a tool response rather than as a process that
    /// died at launch.
    store: Option<MailStore>,
    /// Built on first use and reused: walking the store costs milliseconds, and a
    /// long-running server should not repeat it per read.
    emlx: Arc<Mutex<Option<Arc<EmlxIndex>>>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Match against the subject line. Bodies are NOT searched.
    #[serde(default)]
    pub text: Option<String>,
    /// Match against the sender's email address or display name.
    #[serde(default)]
    pub from: Option<String>,
    /// Match against a mailbox or label name, e.g. "INBOX". Use list_mailboxes to see them.
    #[serde(default)]
    pub mailbox: Option<String>,
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: bool,
    /// Only flagged messages.
    #[serde(default)]
    pub flagged_only: bool,
    /// Only messages from the last N days.
    #[serde(default)]
    pub days: Option<i64>,
    /// Maximum hits to return. Default 20, capped at 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// Message id, as reported by search_mail.
    pub id: i64,
    /// Maximum body bytes to return. Default 4096.
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Keep tracking parameters in URLs. Off by default, because they are usually most of
    /// the message and carry no meaning.
    #[serde(default)]
    pub raw_urls: bool,
}

#[tool_router(router = tool_router)]
impl RailServer {
    pub fn new(store: Option<MailStore>) -> Self {
        Self {
            store,
            emlx: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// Searches message metadata. Subjects, senders, mailboxes, flags and dates only.
    #[tool(
        name = "search_mail",
        description = "Search Apple Mail by subject, sender, mailbox, flags or date. \
                       IMPORTANT: this searches metadata only - message bodies are NOT \
                       searched, so a message whose subject omits the term will not match \
                       even if its body contains it. Returns ids for read_message."
    )]
    pub async fn search_mail(&self, params: Parameters<SearchParams>) -> String {
        if let Err(message) = self.store() {
            return message;
        }
        match self.run_search(params.0).await {
            Ok(text) => text,
            Err(err) => format!("Search failed: {err:#}"),
        }
    }

    /// Reads one message body.
    #[tool(
        name = "read_message",
        description = "Read one Apple Mail message by the id search_mail reported. Returns \
                       headers and body. Some messages have only headers downloaded locally; \
                       the response says so explicitly rather than returning an empty body."
    )]
    pub async fn read_message(&self, params: Parameters<ReadParams>) -> String {
        if let Err(message) = self.store() {
            return message;
        }
        match self.run_read(params.0).await {
            Ok(text) => text,
            Err(err) => format!("Read failed: {err:#}"),
        }
    }

    /// Lists mailboxes and labels.
    #[tool(
        name = "list_mailboxes",
        description = "List the mailboxes and labels available to filter on, with message \
                       counts. Call this before guessing a mailbox name for search_mail."
    )]
    pub async fn list_mailboxes(&self) -> String {
        if let Err(message) = self.store() {
            return message;
        }
        match self.run_list_mailboxes() {
            Ok(text) => text,
            Err(err) => format!("Listing mailboxes failed: {err:#}"),
        }
    }

    /// Reports what this installation can and cannot see.
    #[tool(
        name = "mail_status",
        description = "Report which access mode is active, how many messages are indexed, \
                       and how many have bodies available locally. Call this when a search \
                       returns nothing unexpected, to distinguish 'no such message' from \
                       'that part of the mailbox is not readable'."
    )]
    pub async fn mail_status(&self) -> String {
        if let Err(message) = self.store() {
            return message;
        }
        match self.run_status().await {
            Ok(text) => text,
            Err(err) => format!("Status failed: {err:#}"),
        }
    }
}

impl RailServer {
    /// The store, or an explanation suitable for returning to the client verbatim.
    fn store(&self) -> std::result::Result<&MailStore, String> {
        match &self.store {
            Some(store) => Ok(store),
            None => {
                let diagnosis = rayl_core::probe::run();
                let remedy = diagnosis
                    .remedy()
                    .unwrap_or_else(|| "the mail store is unreadable".to_string());
                Err(format!(
                    "Apple Mail is not readable, so no mail can be searched or read.\n\n{remedy}\n\nTell the user this: it is a one-time permission they must grant, and no retry will succeed until they do."
                ))
            }
        }
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Snapshots per call rather than once at startup: mail arrives while the server runs,
    /// and a stale snapshot would quietly answer from the past.
    fn snapshot(&self) -> Result<rayl_core::store::IndexSnapshot> {
        self.store()
            .map_err(|message| anyhow::anyhow!(message))?
            .snapshot_envelope_index()
            .context("snapshotting the envelope index")
    }

    async fn emlx_index(&self) -> Result<Arc<EmlxIndex>> {
        let mut guard = self.emlx.lock().await;
        if let Some(index) = guard.as_ref() {
            return Ok(index.clone());
        }
        let store = self.store().map_err(|message| anyhow::anyhow!(message))?;
        let index = Arc::new(EmlxIndex::build(store).context("indexing message files")?);
        *guard = Some(index.clone());
        Ok(index)
    }

    async fn run_search(&self, params: SearchParams) -> Result<String> {
        let snapshot = self.snapshot()?;
        let now = Self::now();
        let limit = params
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);

        let query = SearchQuery {
            text: params.text,
            from: params.from,
            mailbox: params.mailbox,
            unread_only: params.unread_only,
            flagged_only: params.flagged_only,
            since: params.days.map(|d| now - d * 86_400),
            until: None,
            limit,
        };

        let results = rayl_core::search::search(&snapshot.db, &query, now)?;

        let mut out = String::new();
        if results.hits.is_empty() {
            out.push_str("No messages matched.\n\n");
            out.push_str(
                "Note: only subjects and senders were searched, not message bodies. \
                 A message may exist whose body mentions the term.\n",
            );
            return Ok(out);
        }

        for hit in &results.hits {
            out.push_str(&format_hit(hit));
        }

        out.push_str(&format!(
            "\nShowing {} of {} matching message{}.\n",
            results.hits.len(),
            results.total_matching,
            if results.total_matching == 1 { "" } else { "s" }
        ));
        out.push_str("Searched metadata only; message bodies were not searched.\n");
        if !results.epoch_confident {
            out.push_str(
                "Warning: the date format in this mail store could not be confirmed, \
                 so dates may be wrong.\n",
            );
        }
        Ok(out)
    }

    async fn run_read(&self, params: ReadParams) -> Result<String> {
        let index = self.emlx_index().await?;
        let max_bytes = params
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, MAX_MAX_BYTES);

        let Some(file) = index.get(params.id) else {
            let (complete, partial) = index.coverage();
            return Ok(format!(
                "No message file for id {}. {} message files are present \
                 ({complete} with bodies, {partial} headers-only). \
                 Ids come from search_mail; this one may be from a different mail store.\n",
                params.id,
                index.len()
            ));
        };

        let message = rayl_core::emlx::read_with(file, !params.raw_urls)?;

        let mut out = String::new();
        if let Some(subject) = &message.subject {
            out.push_str(&format!("Subject: {subject}\n"));
        }
        if let Some(from) = &message.from {
            out.push_str(&format!("From: {from}\n"));
        }
        if !message.to.is_empty() {
            out.push_str(&format!("To: {}\n", message.to.join(", ")));
        }
        if !message.cc.is_empty() {
            out.push_str(&format!("Cc: {}\n", message.cc.join(", ")));
        }
        if let Some(date) = &message.date {
            out.push_str(&format!("Date: {date}\n"));
        }
        if let Some(message_id) = &message.message_id {
            out.push_str(&format!("Message-ID: <{message_id}>\n"));
        }
        if !message.attachments.is_empty() {
            out.push_str(&format!("Attachments: {}\n", message.attachments.join(", ")));
        }
        out.push('\n');

        match message.body() {
            None if message.partial => {
                out.push_str(
                    "BODY NOT AVAILABLE: Mail has downloaded only this message's headers. \
                     The account is configured not to keep full copies locally, so the body \
                     is not on this machine. This is not an empty message.\n",
                );
            }
            None => out.push_str("No readable body found in this message.\n"),
            Some(body) => {
                let (shown, truncated) = truncate_on_boundary(body, max_bytes);
                out.push_str(shown);
                if !shown.ends_with('\n') {
                    out.push('\n');
                }
                if truncated {
                    out.push_str(&format!(
                        "\n[truncated: showed {} of {} bytes. Call again with a larger \
                         max_bytes for more.]\n",
                        shown.len(),
                        body.len()
                    ));
                }
                if message.text.is_none() {
                    out.push_str("\n[converted from HTML; no plain-text part existed.]\n");
                }
                if message.urls_compacted > 0 {
                    out.push_str(&format!(
                        "\n[removed tracking parameters from {} URL(s), saving {} bytes. \
                         Links remain usable. Pass raw_urls to see them unmodified.]\n",
                        message.urls_compacted, message.bytes_saved
                    ));
                }
            }
        }
        Ok(out)
    }

    fn run_list_mailboxes(&self) -> Result<String> {
        let snapshot = self.snapshot()?;
        let boxes = rayl_core::mailbox::list(&snapshot.db)?;

        if boxes.is_empty() {
            return Ok("No mailboxes found.\n".into());
        }

        let mut out = String::from("Mailboxes (use the name with search_mail's mailbox):\n\n");
        for mailbox in &boxes {
            out.push_str(&format!(
                "- {} — {} messages, {} unread\n",
                mailbox.display_name(),
                mailbox.total,
                mailbox.unread
            ));
        }
        out.push_str(
            "\nOn Gmail accounts most messages are filed under All Mail and also appear \
             under one or more labels; filtering by a label name works.\n",
        );
        Ok(out)
    }

    async fn run_status(&self) -> Result<String> {
        let snapshot = self.snapshot()?;
        let index = self.emlx_index().await?;
        let (complete, partial) = index.coverage();

        let total = rayl_core::search::live_message_count(&snapshot.db).unwrap_or(0);

        let mut out = String::new();
        out.push_str(&format!(
            "Mode: direct+live (reading Mail's own files; nothing cached)\n\
             Store: Apple Mail V{}\n\
             Messages indexed: {total}\n\
             Bodies available locally: {complete}\n\
             Headers only (body not downloaded by Mail): {partial}\n\n",
            self.store().map(|store| store.version).unwrap_or(0)
        ));
        out.push_str(
            "What this means for searches: search_mail covers subjects and senders for all \
             messages. Message bodies are not searched at all in this mode. Of the messages \
             that exist, only those with bodies available can be read in full.\n",
        );
        if partial > 0 {
            out.push_str(&format!(
                "\n{partial} message(s) have no local body. read_message on those reports it \
                 explicitly. To change this, set the account in Mail to keep full copies.\n"
            ));
        }
        Ok(out)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RailServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rayl", env!("CARGO_PKG_VERSION")))
            .with_instructions(
            "Read-only access to local Apple Mail. Call list_mailboxes before guessing a \
             mailbox name, and mail_status when a result looks surprisingly empty. \
             search_mail matches subjects and senders only - it does NOT search message \
             bodies, so do not conclude from an empty result that no such mail exists. \
             Nothing here can send, delete or modify mail.",
        )
    }
}

fn format_hit(hit: &SearchHit) -> String {
    let date = hit
        .date_sent_unix
        .map(format_unix)
        .unwrap_or_else(|| "(no date)".into());

    let sender = match (&hit.sender_name, &hit.sender_address) {
        (Some(name), Some(address)) => format!("{name} <{address}>"),
        (None, Some(address)) => address.clone(),
        (Some(name), None) => name.clone(),
        (None, None) => "(unknown sender)".into(),
    };

    let folders: Vec<String> = hit
        .mailboxes
        .iter()
        .filter_map(|url| rayl_core::mailbox::short_name(url))
        .collect();
    let location = if folders.is_empty() {
        rayl_core::mailbox::short_name(&hit.mailbox_url).unwrap_or_default()
    } else {
        folders.join(", ")
    };

    let mut flags = Vec::new();
    if !hit.read {
        flags.push("unread");
    }
    if hit.flagged {
        flags.push("flagged");
    }
    let flag_note = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };

    format!(
        "id {} — {date}{flag_note}\n  From: {sender}\n  Subject: {}\n  In: {location}\n",
        hit.rowid, hit.subject
    )
}

/// Truncates without splitting a UTF-8 character.
fn truncate_on_boundary(body: &str, max_bytes: usize) -> (&str, bool) {
    if body.len() <= max_bytes {
        return (body, false);
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    (&body[..end], true)
}

pub fn run() -> Result<()> {
    // Deliberately tolerant: a server that exits here appears to its client as a crash with
    // no explanation. Starting anyway lets the reason travel over the protocol.
    let store = match MailStore::discover() {
        Ok(store) => Some(store),
        Err(err) => {
            eprintln!("rayl: mail store unavailable: {err}");
            eprintln!("rayl: starting anyway; tools will explain the problem to the client");
            None
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;

    runtime.block_on(async {
        let service = RailServer::new(store)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting the MCP server on stdio")?;
        service.waiting().await.context("serving")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_character_boundaries() {
        let (shown, truncated) = truncate_on_boundary("日本語", 4);
        assert!(truncated);
        assert_eq!(shown, "日");
    }

    #[test]
    fn formats_a_hit_with_flags_and_folders() {
        let hit = SearchHit {
            rowid: 72,
            message_id_header: Some("<a@b.test>".into()),
            subject: "Your invoice is available".into(),
            sender_address: Some("noreply@notify.cloudflare.com".into()),
            sender_name: Some("Cloudflare".into()),
            date_sent_unix: Some(1_794_528_000),
            read: false,
            flagged: true,
            size: 67_000,
            conversation_id: 1,
            mailbox_url: "imap://host/%5BGmail%5D/All%20Mail".into(),
            mailboxes: vec!["imap://host/INBOX".into(), "imap://host/Important".into()],
        };

        let text = format_hit(&hit);
        assert!(text.contains("id 72"));
        assert!(text.contains("[unread, flagged]"));
        assert!(text.contains("Cloudflare <noreply@notify.cloudflare.com>"));
        assert!(text.contains("In: INBOX, Important"), "got {text}");
    }

    #[test]
    fn a_hit_with_no_labels_falls_back_to_the_primary_mailbox() {
        let hit = SearchHit {
            rowid: 1,
            message_id_header: None,
            subject: "Hi".into(),
            sender_address: None,
            sender_name: None,
            date_sent_unix: None,
            read: true,
            flagged: false,
            size: 10,
            conversation_id: 1,
            mailbox_url: "imap://host/Drafts".into(),
            mailboxes: Vec::new(),
        };

        let text = format_hit(&hit);
        assert!(text.contains("In: Drafts"));
        assert!(text.contains("(unknown sender)"));
        assert!(text.contains("(no date)"));
        assert!(!text.contains('['), "no flag note expected: {text}");
    }
}
