//! `rayl search` — metadata search, the user-facing half of `direct+live`.

use anyhow::{Context, Result};
use rayl_core::MailStore;
use rayl_core::epoch::format_unix;
use rayl_core::mailbox::short_name;
use rayl_core::search::{SearchHit, SearchQuery, SearchResults};

#[allow(clippy::too_many_arguments)]
pub fn run(
    text: Option<String>,
    from: Option<String>,
    mailbox: Option<String>,
    unread: bool,
    flagged: bool,
    days: Option<i64>,
    limit: usize,
) -> Result<()> {
    let store = MailStore::discover().context("locating the mail store")?;
    let snapshot = store
        .snapshot_envelope_index()
        .context("snapshotting the envelope index")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let query = SearchQuery {
        text,
        from,
        mailbox,
        unread_only: unread,
        flagged_only: flagged,
        since: days.map(|d| now - d * 86_400),
        until: None,
        limit,
    };

    let results = rayl_core::search::search(&snapshot.db, &query, now).context("searching")?;
    render(&results);
    Ok(())
}

fn render(results: &SearchResults) {
    if results.hits.is_empty() {
        println!("No matches.");
        return;
    }

    for hit in &results.hits {
        print_hit(hit);
    }

    println!(
        "{} of {} message{}",
        results.hits.len(),
        results.total_matching,
        if results.total_matching == 1 { "" } else { "s" }
    );
    if !results.epoch_confident {
        println!(
            "Warning: date epoch could not be confirmed ({}); dates may be wrong.",
            results.epoch.name()
        );
    }
    println!("Bodies are not searched in this mode; subjects and senders only.");
}

fn print_hit(hit: &SearchHit) {
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

    let mut marks = String::new();
    if !hit.read {
        marks.push('*');
    }
    if hit.flagged {
        marks.push('!');
    }

    println!("{date}  {marks:<2} {sender}");
    println!("    {}", hit.subject);

    let mut notes = vec![format!("#{}", hit.rowid), human_size(hit.size)];

    // Prefer the label-derived mailboxes: on Gmail the primary mailbox is "All Mail" for
    // nearly every message, which tells the reader nothing about where it is filed.
    let folders: Vec<String> = hit
        .mailboxes
        .iter()
        .filter_map(|url| short_name(url))
        .collect();
    if folders.is_empty() {
        if let Some(mailbox) = short_name(&hit.mailbox_url) {
            notes.push(mailbox);
        }
    } else {
        notes.push(folders.join(", "));
    }
    println!("    {}", notes.join("  ·  "));
    println!();
}

fn human_size(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = KB * 1024;
    match bytes {
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{} KB", b / KB),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_readably() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
