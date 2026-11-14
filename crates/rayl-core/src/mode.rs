//! The operating modes rayl can serve, as two independent axes.
//!
//! Where data comes from ([`Source`]) is separate from how a search is answered
//! ([`Strategy`]), which makes four combinations rather than a list of special cases. Each
//! trades permission breadth against speed and against what ends up persisted on disk.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::probe::FallbackReason;

/// Where message data is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Mail's own files. Fast, but macOS requires Full Disk Access to read them.
    Direct,
    /// Mail.app over Apple Events. Slow per message, but the permission is scoped to Mail
    /// alone rather than to the whole disk.
    Script,
}

/// How a search is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Query the source on every search. Nothing is persisted; speed tracks the source.
    Live,
    /// Maintain our own index outside Mail's directory. Fast for any source, at the cost of
    /// keeping mail-derived data somewhere macOS does not protect.
    Indexed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub source: Source,
    pub strategy: Strategy,
}

impl Mode {
    pub const ALL: [Mode; 4] = [
        Mode { source: Source::Direct, strategy: Strategy::Live },
        Mode { source: Source::Direct, strategy: Strategy::Indexed },
        Mode { source: Source::Script, strategy: Strategy::Indexed },
        Mode { source: Source::Script, strategy: Strategy::Live },
    ];

    pub fn name(&self) -> &'static str {
        match (self.source, self.strategy) {
            (Source::Direct, Strategy::Live) => "direct+live",
            (Source::Direct, Strategy::Indexed) => "direct+indexed",
            (Source::Script, Strategy::Indexed) => "script+indexed",
            (Source::Script, Strategy::Live) => "script+live",
        }
    }

    /// The permission the user must grant for this mode.
    pub fn permission(&self) -> &'static str {
        match self.source {
            Source::Direct => "Full Disk Access",
            Source::Script => "Automation (Mail.app)",
        }
    }

    /// What this mode leaves on disk between runs.
    pub fn persists(&self) -> &'static str {
        match self.strategy {
            Strategy::Live => "nothing",
            Strategy::Indexed => "metadata + index",
        }
    }

    pub fn search_speed(&self) -> &'static str {
        match (self.source, self.strategy) {
            (Source::Direct, Strategy::Live) => "fast",
            (_, Strategy::Indexed) => "fast after import",
            (Source::Script, Strategy::Live) => "slow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// Usable, but something must happen first that the user can trivially do.
    NeedsAction(String),
    /// Cannot be determined without triggering a permission prompt, so we do not guess.
    Unknown(String),
    Blocked(FallbackReason),
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }
}

/// Facts the availability rules depend on, gathered once by the probe.
#[derive(Debug, Clone)]
pub struct Environment {
    /// `None` when the direct path works; otherwise why it does not.
    pub direct_blocker: Option<FallbackReason>,
    pub mail_app_installed: bool,
    pub mail_app_running: bool,
    /// `None` when our index directory is writable; otherwise why it is not.
    pub index_blocker: Option<String>,
}

pub fn availability(mode: Mode, env: &Environment) -> Availability {
    // The strategy's own requirement: somewhere to put an index.
    if mode.strategy == Strategy::Indexed {
        if let Some(reason) = &env.index_blocker {
            return Availability::Blocked(FallbackReason::Other(reason.clone()));
        }
    }

    match mode.source {
        Source::Direct => match &env.direct_blocker {
            None => Availability::Available,
            Some(reason) => Availability::Blocked(reason.clone()),
        },
        Source::Script => {
            if !env.mail_app_installed {
                return Availability::Blocked(FallbackReason::Other(
                    "Mail.app is not installed".into(),
                ));
            }
            if !env.mail_app_running {
                return Availability::NeedsAction("launch Mail.app".into());
            }
            // Probing Automation access would itself raise the consent dialog, so we report
            // it honestly rather than provoking a prompt during a diagnostic.
            Availability::Unknown("Automation consent is requested on first use".into())
        }
    }
}

/// Where an index would live. Deliberately outside `~/Library/Mail`, which is also why it
/// falls outside macOS's protection: ordinary file permissions are all that guard it.
pub fn index_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(Error::NoHome)?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("rayl"))
}

/// Creates the index directory restricted to the owner, and reports why if it cannot.
/// Returns `None` on success, matching [`Environment::index_blocker`].
pub fn ensure_index_dir() -> Option<String> {
    let dir = match index_dir() {
        Ok(dir) => dir,
        Err(err) => return Some(err.to_string()),
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return Some(format!("{}: {err}", dir.display()));
    }
    // Mail-derived data should not be world-readable just because it left Mail's directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)) {
            return Some(format!("cannot restrict {}: {err}", dir.display()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Environment {
        Environment {
            direct_blocker: None,
            mail_app_installed: true,
            mail_app_running: true,
            index_blocker: None,
        }
    }

    const DIRECT_LIVE: Mode = Mode { source: Source::Direct, strategy: Strategy::Live };
    const DIRECT_INDEXED: Mode = Mode { source: Source::Direct, strategy: Strategy::Indexed };
    const SCRIPT_LIVE: Mode = Mode { source: Source::Script, strategy: Strategy::Live };
    const SCRIPT_INDEXED: Mode = Mode { source: Source::Script, strategy: Strategy::Indexed };

    #[test]
    fn all_four_modes_have_distinct_names() {
        let names: std::collections::HashSet<_> = Mode::ALL.iter().map(|m| m.name()).collect();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn direct_modes_available_when_nothing_blocks() {
        assert!(availability(DIRECT_LIVE, &env()).is_available());
        assert!(availability(DIRECT_INDEXED, &env()).is_available());
    }

    #[test]
    fn full_disk_access_blocks_only_direct_modes() {
        let mut env = env();
        env.direct_blocker = Some(FallbackReason::FullDiskAccessRequired {
            path: PathBuf::from("/Users/x/Library/Mail"),
        });

        assert!(matches!(
            availability(DIRECT_LIVE, &env),
            Availability::Blocked(FallbackReason::FullDiskAccessRequired { .. })
        ));
        // The script cells are exactly the point: they survive a missing FDA grant.
        assert!(matches!(availability(SCRIPT_LIVE, &env), Availability::Unknown(_)));
        assert!(matches!(availability(SCRIPT_INDEXED, &env), Availability::Unknown(_)));
    }

    #[test]
    fn unwritable_index_dir_blocks_only_indexed_strategies() {
        let mut env = env();
        env.index_blocker = Some("read-only volume".into());

        assert!(availability(DIRECT_LIVE, &env).is_available());
        assert!(matches!(availability(DIRECT_INDEXED, &env), Availability::Blocked(_)));
        assert!(matches!(availability(SCRIPT_INDEXED, &env), Availability::Blocked(_)));
    }

    #[test]
    fn script_modes_need_mail_running() {
        let mut env = env();
        env.mail_app_running = false;
        assert!(matches!(availability(SCRIPT_LIVE, &env), Availability::NeedsAction(_)));
    }

    #[test]
    fn script_modes_blocked_without_mail_app() {
        let mut env = env();
        env.mail_app_installed = false;
        assert!(matches!(availability(SCRIPT_LIVE, &env), Availability::Blocked(_)));
    }

    #[test]
    fn live_strategies_persist_nothing() {
        assert_eq!(DIRECT_LIVE.persists(), "nothing");
        assert_eq!(SCRIPT_LIVE.persists(), "nothing");
        assert_eq!(DIRECT_INDEXED.persists(), "metadata + index");
    }

    #[test]
    fn index_dir_sits_outside_the_protected_mail_directory() {
        let dir = index_dir().unwrap();
        assert!(dir.ends_with("Library/Application Support/rayl"));
        assert!(!dir.starts_with(crate::store::mail_root().unwrap()));
    }
}
