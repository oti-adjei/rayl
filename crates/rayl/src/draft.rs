//! `rayl draft` and `rayl draft-reply` — create drafts in Mail. Never send.

use anyhow::{Context, Result};
use rayl_core::MailStore;
use rayl_core::script::Draft;

pub fn new_message(to: Vec<String>, cc: Vec<String>, subject: String, body: String) -> Result<()> {
    let draft = Draft { to, cc, subject, body };
    let outcome = rayl_core::script::draft_message(&draft).context("creating the draft")?;
    println!("{}", outcome.detail);
    println!("Not sent. Open Mail to review and send it.");
    Ok(())
}

pub fn reply(id: i64, body: String, reply_all: bool) -> Result<()> {
    let store = MailStore::discover().context("locating the mail store")?;
    let snapshot = store
        .snapshot_envelope_index()
        .context("snapshotting the envelope index")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let hit = rayl_core::search::hit_by_rowid(&snapshot.db, id, now)
        .context("looking up the message")?
        .ok_or_else(|| anyhow::anyhow!("no message with id {id}; ids come from `rayl search`"))?;

    let message_id = hit.message_id_header.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "message {id} has no RFC Message-ID recorded, so Mail cannot locate it to build \
             a threaded reply"
        )
    })?;

    // On Gmail the primary mailbox is All Mail, which Mail.app does not expose by that name,
    // so a label mailbox is the better place to look.
    let mailbox = hit
        .mailboxes
        .iter()
        .chain(std::iter::once(&hit.mailbox_url))
        .filter_map(|url| rayl_core::mailbox::short_name(url))
        .next()
        .unwrap_or_else(|| "INBOX".to_string());

    let outcome = rayl_core::script::draft_reply(message_id, &mailbox, &body, reply_all)
        .context("creating the reply draft")?;

    println!("{}", outcome.detail);
    println!("Replying to: {}", hit.subject);
    println!("Not sent. Open Mail to review and send it.");
    Ok(())
}
