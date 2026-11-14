mod doctor;
mod read;
mod schema;
mod search;
mod serve;
mod verify;

use clap::{Parser, Subcommand};

/// MCP server for Apple Mail.
#[derive(Parser)]
#[command(name = "rayl", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether this machine can serve the fast direct read path, and explain what to
    /// fix when it cannot. Exits non-zero unless the direct path is available.
    Doctor,
    /// Print the shape of the envelope index: tables, columns, row counts. Never row values.
    Schema {
        /// Every table, not just the ones the read path is built against.
        #[arg(long)]
        all: bool,
        /// Only these tables. Repeatable: --table messages --table subjects
        #[arg(long = "table", conflicts_with = "all")]
        table: Vec<String>,
        /// Table names and row counts only, no columns.
        #[arg(long)]
        tables: bool,
    },
    /// Search message metadata: subject, sender, mailbox, flags, date. Bodies are not
    /// searched in this mode.
    Search {
        /// Match against the subject.
        text: Option<String>,
        /// Match against the sender address or display name.
        #[arg(long)]
        from: Option<String>,
        /// Match against the mailbox name.
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        flagged: bool,
        /// Only messages from the last N days.
        #[arg(long)]
        days: Option<i64>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Read one message body by the id `rayl search` reports.
    Read {
        /// Message id, as shown by `rayl search` (the #N). Omit with --coverage.
        id: Option<i64>,
        /// Cap the body at this many bytes.
        #[arg(long, default_value_t = 4096)]
        max_bytes: usize,
        /// Report how many message bodies exist on disk instead of reading one.
        #[arg(long)]
        coverage: bool,
        /// Keep tracking parameters in URLs instead of stripping them.
        #[arg(long)]
        raw_urls: bool,
    },
    /// Resolve what the schema shape leaves open: date epoch, foreign keys, enum values.
    /// Reports counts only, never message content.
    Verify,
    /// Serve the MCP protocol over stdio.
    Serve,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Doctor => {
            let diagnosis = rayl_core::probe::run();
            doctor::report(&diagnosis);
            if !diagnosis.can_read_directly() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Schema { all, table, tables } => schema::run(all, table, tables),
        Command::Search { text, from, mailbox, unread, flagged, days, limit } => {
            search::run(text, from, mailbox, unread, flagged, days, limit)
        }
        Command::Read { id, max_bytes, coverage, raw_urls } => {
            read::run(id, max_bytes, coverage, raw_urls)
        }
        Command::Verify => verify::run(),
        Command::Serve => serve::run(),
    }
}
