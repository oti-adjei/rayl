//! `rayl schema` — dumps the shape of the envelope index.
//!
//! The schema is undocumented and shifts between macOS releases, so the read path has to be
//! written against a real one. This prints that shape and nothing else: column names and row
//! counts, never row values.

use anyhow::{Context, Result};
use rayl_core::MailStore;
use rayl_core::store::{CORE_TABLES, TableShape, schema_shapes};

pub fn run(all: bool, table: Vec<String>, names_only: bool) -> Result<()> {
    let store = MailStore::discover().context("locating the mail store")?;
    let snapshot = store
        .snapshot_envelope_index()
        .context("snapshotting the envelope index")?;

    let filter: Option<Vec<String>> = if !table.is_empty() {
        Some(table.clone())
    } else if all || names_only {
        None
    } else {
        Some(CORE_TABLES.iter().map(|t| t.to_string()).collect())
    };

    let shapes = schema_shapes(&snapshot.db, filter.as_deref()).context("reading the schema")?;

    println!("Apple Mail V{} envelope index", store.version);
    if !all && !names_only && table.is_empty() {
        println!("(core tables only; use --all for every table)");
    }
    println!();

    if shapes.is_empty() {
        if table.is_empty() {
            println!("No matching tables.");
        } else {
            println!("No tables named: {}", table.join(", "));
        }
        return Ok(());
    }

    if names_only {
        for shape in &shapes {
            let rows = shape.rows.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
            println!("  {:<40} {:>8} rows  {:>3} cols", shape.name, rows, shape.columns.len());
        }
        return Ok(());
    }

    for shape in &shapes {
        print_table(shape);
    }

    // Threading has no table of its own in V10, so point at whatever carries it instead.
    if all || table.is_empty() {
        let hints = threading_hints(&shapes);
        if !hints.is_empty() {
            println!("Threading-related columns: {}", hints.join(", "));
        }
    }

    Ok(())
}

fn print_table(shape: &TableShape) {
    let rows = shape
        .rows
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    println!("{} ({} columns, {rows} rows)", shape.name, shape.columns.len());

    for column in &shape.columns {
        let mut flags = Vec::new();
        if column.pk {
            flags.push("pk");
        }
        if column.notnull {
            flags.push("not null");
        }
        let decl = if column.decl_type.is_empty() {
            "-"
        } else {
            &column.decl_type
        };
        let suffix = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join(", "))
        };
        println!("  {:<28} {:<10}{suffix}", column.name, decl);
    }
    println!();
}

/// Columns that look like they carry conversation grouping, wherever they live.
fn threading_hints(shapes: &[TableShape]) -> Vec<String> {
    const NEEDLES: &[&str] = &["thread", "conversation", "reference", "in_reply", "message_id"];

    let mut hits = Vec::new();
    for shape in shapes {
        for column in &shape.columns {
            let lowered = column.name.to_lowercase();
            if NEEDLES.iter().any(|needle| lowered.contains(needle)) {
                hits.push(format!("{}.{}", shape.name, column.name));
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayl_core::store::Column;

    fn column(name: &str) -> Column {
        Column { name: name.into(), decl_type: "INTEGER".into(), notnull: false, pk: false }
    }

    #[test]
    fn finds_threading_columns_across_tables() {
        let shapes = vec![
            TableShape {
                name: "messages".into(),
                columns: vec![column("ROWID"), column("conversation_id"), column("subject")],
                rows: Some(1),
            },
            TableShape {
                name: "message_references".into(),
                columns: vec![column("message_id")],
                rows: Some(1),
            },
        ];

        let hints = threading_hints(&shapes);
        assert_eq!(
            hints,
            vec![
                "messages.conversation_id".to_string(),
                "message_references.message_id".to_string()
            ]
        );
    }

    #[test]
    fn no_threading_columns_is_not_an_error() {
        let shapes = vec![TableShape {
            name: "messages".into(),
            columns: vec![column("ROWID"), column("flags")],
            rows: Some(0),
        }];
        assert!(threading_hints(&shapes).is_empty());
    }
}
