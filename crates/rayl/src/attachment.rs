//! `rayl attachment` — list or save a message's attachments.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rayl_core::MailStore;
use rayl_core::emlx::EmlxIndex;

pub fn run(id: i64, which: Option<String>, dir: Option<String>, list: bool) -> Result<()> {
    let store = MailStore::discover().context("locating the mail store")?;
    let index = EmlxIndex::build(&store).context("indexing message files")?;

    let Some(file) = index.get(id) else {
        bail!("no message file for id {id}; ids come from `rayl search`");
    };
    if file.partial {
        bail!(
            "message {id} has only its headers downloaded, so its attachments are not on this \
             machine"
        );
    }

    let found = rayl_core::emlx::extract_attachments(file, which.as_deref())
        .context("reading attachments")?;

    if found.is_empty() {
        match &which {
            Some(which) => println!("No attachment matching {which:?} on message {id}."),
            None => println!("Message {id} has no attachments."),
        }
        return Ok(());
    }

    if list {
        for (position, attachment) in found.iter().enumerate() {
            println!(
                "{position}  {}  {}  {} bytes",
                attachment.filename,
                attachment.content_type.as_deref().unwrap_or("-"),
                attachment.bytes.len()
            );
        }
        return Ok(());
    }

    let target = match dir {
        Some(dir) => PathBuf::from(dir),
        None => default_dir()?,
    };
    std::fs::create_dir_all(&target)
        .with_context(|| format!("creating {}", target.display()))?;

    for attachment in &found {
        // The filename is already reduced to a safe leaf by the core, so joining cannot
        // escape the target directory.
        let path = unique_path(&target, &attachment.filename);
        std::fs::write(&path, &attachment.bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("{} ({} bytes)", path.display(), attachment.bytes.len());
    }
    Ok(())
}

fn default_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Downloads"))
}

/// Never silently overwrite: `report.pdf` becomes `report-2.pdf` if taken.
pub fn unique_path(dir: &std::path::Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let path = std::path::Path::new(filename);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let extension = path.extension().and_then(|s| s.to_str());

    for n in 2..1000 {
        let name = match extension {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_avoids_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_path(dir.path(), "report.pdf");
        assert!(first.ends_with("report.pdf"));

        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(dir.path(), "report.pdf");
        assert!(second.ends_with("report-2.pdf"), "got {second:?}");

        std::fs::write(&second, b"x").unwrap();
        assert!(unique_path(dir.path(), "report.pdf").ends_with("report-3.pdf"));
    }

    #[test]
    fn unique_path_handles_a_name_without_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_path(dir.path(), "notes");
        std::fs::write(&first, b"x").unwrap();
        assert!(unique_path(dir.path(), "notes").ends_with("notes-2"));
    }
}
