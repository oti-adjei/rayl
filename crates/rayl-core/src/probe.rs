//! Startup probe: decides whether the fast direct-read path is available, and when it is not,
//! produces enough detail for the user to fix it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;
use crate::mode::{Availability, Environment, Mode, availability, ensure_index_dir, index_dir};
use crate::store::{EmlxScan, MailStore, SchemaReport, inspect_schema};

/// Which read path a given machine can actually serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Direct file + SQLite access works. Full-text search, fast reads.
    Direct,
    /// Mail's data is unreadable but Mail.app can be scripted: slower, reduced capability.
    ScriptFallback(FallbackReason),
    /// Neither path is available.
    Unavailable(FallbackReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    /// The single most common failure: TCC. Fixed by the user, not by us.
    FullDiskAccessRequired { path: PathBuf },
    MailNotConfigured,
    EnvelopeIndexMissing,
    SchemaUnrecognized { missing: Vec<String> },
    Other(String),
}

#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub mail_root: PathBuf,
    pub store: Option<StoreInfo>,
    pub schema: Option<SchemaReport>,
    pub emlx: Option<EmlxScan>,
    pub mail_app: Option<PathBuf>,
    /// The app bundle macOS holds responsible for our privacy permissions, i.e. the one the
    /// user must grant Full Disk Access to. Not necessarily our direct parent.
    pub responsible_app: Option<PathBuf>,
    /// Fallback when no `.app` bundle owns us: the outermost non-shell process, which is the
    /// best guess at what needs the grant. Terminal multiplexers detach from the terminal
    /// that started them, so the terminal is often absent from the chain entirely.
    pub likely_grant_target: Option<String>,
    pub process_chain: Vec<String>,
    pub verdict: Verdict,
    pub mail_app_running: bool,
    pub index_dir: PathBuf,
    /// Every mode and whether this machine can serve it, in preference order.
    pub modes: Vec<(Mode, Availability)>,
}

#[derive(Debug, Clone)]
pub struct StoreInfo {
    pub version: u32,
    pub path: PathBuf,
    pub index_bytes: u64,
    pub index_sidecars: Vec<String>,
}

impl Diagnosis {
    pub fn can_read_directly(&self) -> bool {
        self.verdict == Verdict::Direct
    }

    /// A short, actionable explanation of why access failed and what fixes it. `None` when
    /// nothing is wrong. Shared by the CLI diagnostic and the MCP server, so an agent gets
    /// the same remedy a human would.
    pub fn remedy(&self) -> Option<String> {
        let reason = match &self.verdict {
            Verdict::Direct => return None,
            Verdict::ScriptFallback(reason) | Verdict::Unavailable(reason) => reason,
        };

        Some(match reason {
            FallbackReason::FullDiskAccessRequired { path } => {
                let target = self
                    .responsible_app
                    .as_ref()
                    .map(|app| app.display().to_string())
                    .or_else(|| self.likely_grant_target.clone())
                    .unwrap_or_else(|| "the application that launched rayl".to_string());
                format!(
                    "macOS privacy controls block {}. Grant Full Disk Access to {} in System Settings > Privacy & Security > Full Disk Access, then restart it completely. rayl cannot request this permission itself.",
                    path.display(),
                    target
                )
            }
            FallbackReason::MailNotConfigured => "Mail has no local store, which means no account is set up. Open Mail and add an account, then let it sync.".to_string(),
            FallbackReason::EnvelopeIndexMissing => "Mail's store exists but its index does not. Open Mail once to rebuild it.".to_string(),
            FallbackReason::SchemaUnrecognized { missing } => format!(
                "Mail's index is missing tables rayl needs ({}). This usually means a macOS release changed the schema.",
                missing.join(", ")
            ),
            FallbackReason::Other(message) => message.clone(),
        })
    }
}

/// How many `.emlx` files the scan will count before giving up and reporting a lower bound.
const EMLX_SCAN_CAP: usize = 5_000;

pub fn run() -> Diagnosis {
    let mail_root = crate::store::mail_root().unwrap_or_else(|_| PathBuf::from("$HOME/Library/Mail"));
    let process_chain = process_chain();
    let responsible_app = responsible_app(&process_chain);
    let likely_grant_target = likely_grant_target(&process_chain);
    let mail_app = locate_mail_app();

    let mut diagnosis = Diagnosis {
        mail_root,
        store: None,
        schema: None,
        emlx: None,
        mail_app,
        responsible_app,
        likely_grant_target,
        process_chain,
        verdict: Verdict::Direct,
        mail_app_running: mail_app_running(),
        index_dir: index_dir().unwrap_or_else(|_| PathBuf::from("$HOME/Library/Application Support/rayl")),
        modes: Vec::new(),
    };

    let store = match MailStore::discover() {
        Ok(store) => store,
        Err(err) => {
            diagnosis.verdict = degrade(&diagnosis, reason_for(&err, &diagnosis.mail_root));
            return finish(diagnosis);
        }
    };

    let snapshot = match store.snapshot_envelope_index() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            diagnosis.store = Some(StoreInfo {
                version: store.version,
                path: store.path.clone(),
                index_bytes: 0,
                index_sidecars: Vec::new(),
            });
            diagnosis.verdict = degrade(&diagnosis, reason_for(&err, &store.envelope_index()));
            return finish(diagnosis);
        }
    };

    diagnosis.store = Some(StoreInfo {
        version: store.version,
        path: store.path.clone(),
        index_bytes: snapshot.bytes,
        index_sidecars: snapshot.sidecars.clone(),
    });

    match inspect_schema(&snapshot.db) {
        Ok(report) => {
            if !report.is_usable() {
                let missing = report.missing_required.clone();
                diagnosis.verdict =
                    degrade(&diagnosis, FallbackReason::SchemaUnrecognized { missing });
            }
            diagnosis.schema = Some(report);
        }
        Err(err) => {
            diagnosis.verdict = degrade(&diagnosis, FallbackReason::Other(err.to_string()));
        }
    }

    // Informational: tells us whether bodies are actually on disk or only headers are.
    diagnosis.emlx = store.scan_emlx(EMLX_SCAN_CAP).ok();

    finish(diagnosis)
}

/// Fills in the mode grid from whatever the probe established.
fn finish(mut diagnosis: Diagnosis) -> Diagnosis {
    let environment = Environment {
        direct_blocker: match &diagnosis.verdict {
            Verdict::Direct => None,
            Verdict::ScriptFallback(reason) | Verdict::Unavailable(reason) => Some(reason.clone()),
        },
        mail_app_installed: diagnosis.mail_app.is_some(),
        mail_app_running: diagnosis.mail_app_running,
        index_blocker: ensure_index_dir(),
    };

    diagnosis.modes = Mode::ALL
        .iter()
        .map(|mode| (*mode, availability(*mode, &environment)))
        .collect();
    diagnosis
}

fn mail_app_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-f", "MacOS/Mail"])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}

/// A reason only means "fall back" if Mail.app is installed to fall back onto.
fn degrade(diagnosis: &Diagnosis, reason: FallbackReason) -> Verdict {
    if diagnosis.mail_app.is_some() {
        Verdict::ScriptFallback(reason)
    } else {
        Verdict::Unavailable(reason)
    }
}

fn reason_for(err: &Error, path: &Path) -> FallbackReason {
    match err {
        Error::AccessDenied { path } => {
            FallbackReason::FullDiskAccessRequired { path: path.clone() }
        }
        Error::MailDirMissing { .. } | Error::NoVersionedStore { .. } => {
            FallbackReason::MailNotConfigured
        }
        Error::EnvelopeIndexMissing { .. } => FallbackReason::EnvelopeIndexMissing,
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied => {
            FallbackReason::FullDiskAccessRequired { path: path.to_path_buf() }
        }
        other => FallbackReason::Other(other.to_string()),
    }
}

fn locate_mail_app() -> Option<PathBuf> {
    ["/System/Applications/Mail.app", "/Applications/Mail.app"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.try_exists().unwrap_or(false))
}

/// Walks the parent chain, innermost first. Used to name the app that owns our TCC grants.
fn process_chain() -> Vec<String> {
    let mut chain = Vec::new();
    let mut pid = std::process::id();

    for _ in 0..12 {
        let Some((ppid, path)) = process_info(pid) else { break };
        chain.push(path);
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }

    chain
}

fn process_info(pid: u32) -> Option<(u32, String)> {
    let output = Command::new("/bin/ps")
        .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim();
    let (ppid, path) = line.split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, path.trim().to_string()))
}

/// The nearest enclosing `.app` bundle. macOS attributes privacy permissions to it, which is
/// why telling the user to grant access to "the terminal" is usually wrong — it may be
/// Claude, iTerm, VS Code or something else entirely.
fn responsible_app(chain: &[String]) -> Option<PathBuf> {
    chain.iter().find_map(|path| bundle_root(path))
}

/// Shells are never the right thing to grant access to: they are transient and shared.
const SHELLS: &[&str] = &["zsh", "bash", "sh", "fish", "dash", "tcsh", "csh", "nu", "ksh"];

fn likely_grant_target(chain: &[String]) -> Option<String> {
    chain
        .iter()
        .rev()
        .find(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            name != "launchd" && !SHELLS.contains(&name)
        })
        .cloned()
}

fn bundle_root(exec_path: &str) -> Option<PathBuf> {
    let marker = ".app/Contents/MacOS/";
    let idx = exec_path.find(marker)?;
    Some(PathBuf::from(&exec_path[..idx + ".app".len()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bundle_root_from_executable_path() {
        assert_eq!(
            bundle_root("/Applications/iTerm.app/Contents/MacOS/iTerm2"),
            Some(PathBuf::from("/Applications/iTerm.app"))
        );
        assert_eq!(bundle_root("/bin/zsh"), None);
        assert_eq!(bundle_root("/usr/local/bin/rayl"), None);
    }

    #[test]
    fn picks_innermost_app_bundle_from_chain() {
        let chain = vec![
            "/usr/local/bin/rayl".to_string(),
            "/bin/zsh".to_string(),
            "/Applications/Ghostty.app/Contents/MacOS/ghostty".to_string(),
            "/sbin/launchd".to_string(),
        ];
        assert_eq!(
            responsible_app(&chain),
            Some(PathBuf::from("/Applications/Ghostty.app"))
        );
    }

    #[test]
    fn no_app_bundle_in_chain_is_not_an_error() {
        let chain = vec!["/usr/local/bin/rayl".to_string(), "/sbin/launchd".to_string()];
        assert_eq!(responsible_app(&chain), None);
    }

    #[test]
    fn grant_target_skips_shells_and_launchd() {
        let chain = vec![
            "/usr/local/bin/rayl".to_string(),
            "/bin/zsh".to_string(),
            "/opt/homebrew/bin/zellij".to_string(),
            "/sbin/launchd".to_string(),
        ];
        assert_eq!(
            likely_grant_target(&chain),
            Some("/opt/homebrew/bin/zellij".to_string())
        );
    }

    #[test]
    fn grant_target_falls_back_to_our_own_binary() {
        let chain = vec!["/usr/local/bin/rayl".to_string(), "/bin/zsh".to_string()];
        assert_eq!(
            likely_grant_target(&chain),
            Some("/usr/local/bin/rayl".to_string())
        );
    }

    #[test]
    fn access_denied_maps_to_full_disk_access() {
        let err = Error::AccessDenied { path: PathBuf::from("/x") };
        assert!(matches!(
            reason_for(&err, Path::new("/x")),
            FallbackReason::FullDiskAccessRequired { .. }
        ));
    }

    #[test]
    fn missing_store_maps_to_not_configured() {
        let err = Error::NoVersionedStore { path: PathBuf::from("/x") };
        assert_eq!(reason_for(&err, Path::new("/x")), FallbackReason::MailNotConfigured);
    }

    #[test]
    fn process_chain_starts_with_our_own_executable() {
        let chain = process_chain();
        assert!(!chain.is_empty(), "expected at least our own process");
    }

    #[test]
    fn verdict_is_unavailable_without_mail_app() {
        let diagnosis = Diagnosis {
            mail_root: PathBuf::from("/x"),
            store: None,
            schema: None,
            emlx: None,
            mail_app: None,
            responsible_app: None,
            likely_grant_target: None,
            process_chain: Vec::new(),
            verdict: Verdict::Direct,
            mail_app_running: false,
            index_dir: PathBuf::from("/x"),
            modes: Vec::new(),
        };
        assert!(matches!(
            degrade(&diagnosis, FallbackReason::MailNotConfigured),
            Verdict::Unavailable(_)
        ));
    }
}
