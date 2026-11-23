//! Renders the startup probe as something a user can act on.

use rayl_core::mode::Availability;
use rayl_core::probe::{Diagnosis, StoreInfo};
use rayl_core::store::SchemaReport;
use rayl_core::{FallbackReason, Verdict};

pub fn report(diagnosis: &Diagnosis) {
    println!("rayl doctor");
    println!();

    println!("Mail store");
    println!("  looking in       {}", diagnosis.mail_root.display());
    match &diagnosis.store {
        Some(store) => store_lines(store),
        None => println!("  store            not readable"),
    }
    match &diagnosis.mail_app {
        Some(path) => println!(
            "  Mail.app         {} ({})",
            path.display(),
            if diagnosis.mail_app_running { "running" } else { "not running" }
        ),
        None => println!("  Mail.app         not installed"),
    }
    println!("  index dir        {}", diagnosis.index_dir.display());
    println!();

    if let Some(schema) = &diagnosis.schema {
        schema_lines(schema);
        println!();
    }

    if let Some(scan) = &diagnosis.emlx {
        let bound = if scan.capped { "at least " } else { "" };
        println!("Message bodies");
        println!("  on disk          {bound}{}", scan.complete_count);
        if scan.partial_count > 0 {
            println!(
                "  headers only     {} (not downloaded by Mail)",
                scan.partial_count
            );
        }
        if scan.unreadable_dirs > 0 {
            println!("  unreadable dirs  {}", scan.unreadable_dirs);
        }
        println!();
    }

    modes_table(diagnosis);
    println!();

    println!("Permissions");
    match &diagnosis.responsible_app {
        Some(app) => println!("  host app         {}", app.display()),
        None => println!("  host app         no enclosing .app bundle"),
    }
    if diagnosis.responsible_app.is_none() {
        if let Some(target) = &diagnosis.likely_grant_target {
            println!("  grant candidate  {target}");
        }
    }
    println!();

    verdict_lines(diagnosis);
    println!("New drafts use Mail Automation. Open Mail.app and allow Automation when prompted.");
}

fn modes_table(diagnosis: &Diagnosis) {
    println!("Modes");
    println!(
        "  {mode:<16} {permission:<22} {persists:<18} {search:<18} status",
        mode = "mode",
        permission = "permission",
        persists = "persists",
        search = "search"
    );
    for (mode, state) in &diagnosis.modes {
        if mode.name() != "direct+live" {
            continue;
        }
        let (marker, status) = match state {
            Availability::Available => ("+", "available".to_string()),
            Availability::NeedsAction(what) => ("~", format!("needs: {what}")),
            Availability::Unknown(why) => ("?", why.clone()),
            Availability::Blocked(reason) => ("-", format!("blocked: {}", summarise(reason))),
        };
        println!(
            "{marker} {:<16} {:<22} {:<18} {:<18} {status}",
            mode.name(),
            mode.permission(),
            mode.persists(),
            mode.search_speed(),
        );
    }
}

/// Short form for the table; the full explanation comes with the verdict.
fn summarise(reason: &FallbackReason) -> String {
    match reason {
        FallbackReason::FullDiskAccessRequired { .. } => "no Full Disk Access".into(),
        FallbackReason::MailNotConfigured => "Mail not configured".into(),
        FallbackReason::EnvelopeIndexMissing => "no envelope index".into(),
        FallbackReason::SchemaUnrecognized { .. } => "unrecognised schema".into(),
        FallbackReason::Other(message) => message.clone(),
    }
}

fn store_lines(store: &StoreInfo) {
    println!("  version          V{}", store.version);
    println!("  path             {}", store.path.display());
    if store.index_bytes > 0 {
        println!(
            "  envelope index   {:.1} MB{}",
            store.index_bytes as f64 / 1_048_576.0,
            if store.index_sidecars.is_empty() {
                String::new()
            } else {
                format!(" (+{})", store.index_sidecars.join(", "))
            }
        );
    }
}

fn schema_lines(schema: &SchemaReport) {
    println!("Schema");
    println!("  tables           {}", schema.tables.len());
    if let Some(count) = schema.message_count {
        println!("  messages         {count}");
    }
    println!("  message columns  {}", schema.message_columns.len());
    if !schema.missing_required.is_empty() {
        println!("  MISSING required {}", schema.missing_required.join(", "));
    }
    if !schema.missing_expected.is_empty() {
        println!(
            "  missing optional {} (reduced filters)",
            schema.missing_expected.join(", ")
        );
    }
}

fn verdict_lines(diagnosis: &Diagnosis) {
    match &diagnosis.verdict {
        Verdict::Direct => {
            println!("Verdict: direct read path available.");
            println!("rayl serve reads through direct+live without keeping an index.");
        }
        Verdict::ScriptFallback(reason) | Verdict::Unavailable(reason) => {
            println!("Verdict: direct read path unavailable.");
            println!();
            explain(reason, diagnosis);
        }
    }
}

fn explain(reason: &FallbackReason, diagnosis: &Diagnosis) {
    match reason {
        FallbackReason::FullDiskAccessRequired { path } => {
            println!("Reason: macOS privacy controls block {}.", path.display());
            println!();
            match &diagnosis.responsible_app {
                Some(app) => {
                    let name = app
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("the host app");
                    println!("Grant Full Disk Access to {name}:");
                    println!("  1. System Settings > Privacy & Security > Full Disk Access");
                    println!("  2. Enable {}", app.display());
                    println!("  3. Quit {name} completely and reopen it");
                    println!();
                    println!(
                        "The permission belongs to {name}, not to rayl: rayl cannot request it."
                    );
                }
                None => {
                    println!("No enclosing .app bundle owns this process, so there is no app to");
                    println!("grant access to. Add the executable itself instead:");
                    println!();
                    match &diagnosis.likely_grant_target {
                        Some(target) => {
                            println!("  1. System Settings > Privacy & Security > Full Disk Access");
                            println!("  2. Click +, press Cmd-Shift-G, and enter:");
                            println!("       {target}");
                            println!("  3. Restart that process");
                        }
                        None => {
                            println!("  System Settings > Privacy & Security > Full Disk Access");
                        }
                    }
                    println!();
                    println!("Process chain, innermost first:");
                    for (depth, path) in diagnosis.process_chain.iter().enumerate() {
                        println!("  {}{}", "  ".repeat(depth), path);
                    }
                    println!();
                    println!("If a terminal app is absent from that chain, a multiplexer has");
                    println!("detached from it and the grant must go to the multiplexer.");
                }
            }
        }
        FallbackReason::MailNotConfigured => {
            println!("Reason: Mail has no local store, which means no account is set up.");
            println!("Open Mail.app and add an account, then let it finish syncing.");
        }
        FallbackReason::EnvelopeIndexMissing => {
            println!("Reason: the store exists but its envelope index does not.");
            println!("Open Mail.app once to let it rebuild the index.");
        }
        FallbackReason::SchemaUnrecognized { missing } => {
            println!(
                "Reason: the envelope index is missing tables rayl needs: {}.",
                missing.join(", ")
            );
            println!("This usually means a macOS release changed the schema.");
            println!("Please report the store version and table list above.");
        }
        FallbackReason::Other(message) => {
            println!("Reason: {message}");
        }
    }
}
