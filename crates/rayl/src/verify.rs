//! `rayl verify` — resolves, against real data, what the schema shape leaves open.

use anyhow::{Context, Result};
use rayl_core::MailStore;
use rayl_core::verify::{JoinResolution, Verification};

pub fn run() -> Result<()> {
    let store = MailStore::discover().context("locating the mail store")?;
    let snapshot = store
        .snapshot_envelope_index()
        .context("snapshotting the envelope index")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let report = rayl_core::verify::run(&snapshot.db, now).context("verifying the schema")?;

    println!("rayl verify  (Apple Mail V{})", store.version);
    println!("All output is counts and distributions; no message content is read.");
    println!();
    print_report(&report);
    Ok(())
}

fn print_report(report: &Verification) {
    println!("Dates");
    match &report.epoch {
        Some(epoch) => {
            println!("  epoch            {}", epoch.name_with_confidence());
            println!("  sample value     {}", epoch.sample);
            println!(
                "  as Unix          {}   as Core Data   {}",
                epoch.as_unix_year, epoch.as_core_data_year
            );
            if let Some(newest) = &report.newest_message {
                println!("  newest message   {newest}");
            }
            if let Some(oldest) = &report.oldest_message {
                println!("  oldest message   {oldest}");
            }
        }
        None => println!("  no usable timestamps found"),
    }
    println!();

    println!("message_global_data.message_id references");
    match &report.global_data_join {
        JoinResolution::Rowid { matches, rival } => {
            println!("  messages.ROWID         {matches} matches (vs {rival} for message_id)");
        }
        JoinResolution::MessageIdColumn { matches, rival } => {
            println!("  messages.message_id    {matches} matches (vs {rival} for ROWID)");
        }
        JoinResolution::Ambiguous { rowid, message_id } => {
            println!("  AMBIGUOUS: ROWID {rowid}, message_id {message_id}");
        }
        JoinResolution::Unavailable(why) => println!("  unavailable: {why}"),
    }
    println!();

    println!("Messages");
    println!("  total            {}", report.messages_total);
    println!("  not deleted      {}", report.messages_live);
    println!("  distinct global  {}", report.distinct_global_ids);
    let dupes = report.messages_live - report.distinct_global_ids;
    if dupes > 0 {
        println!("  duplicate rows   {dupes} (same global id, several mailboxes)");
    }
    println!("  multi-mailbox    {} (via labels)", report.multi_mailbox_messages);
    println!(
        "  RFC Message-ID   {} of {}",
        report.message_id_header_present, report.messages_total
    );
    println!("  with a summary   {}", report.summaries_linked);
    println!();

    distribution("download_state (message_global_data)", &report.download_states);
    distribution("recipients.type", &report.recipient_types);
    distribution(
        "message_body_indexed (searchable_messages)",
        &report.body_indexed,
    );
}

fn distribution(label: &str, values: &[(i64, i64)]) {
    println!("{label}");
    if values.is_empty() {
        println!("  (no rows)");
    } else {
        for (value, count) in values {
            let shown = if *value == -1 { "NULL".to_string() } else { value.to_string() };
            println!("  {shown:>6}  {count}");
        }
    }
    println!();
}
