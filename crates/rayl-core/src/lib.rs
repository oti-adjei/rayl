//! Access to the local Apple Mail store.
//!
//! Two read paths exist. The direct path reads Mail's SQLite envelope index and `.emlx`
//! bodies straight off disk, which is fast but requires Full Disk Access and a recognised
//! schema. The script path drives Mail.app through Apple Events, which always works but is
//! slow and offers less. [`probe::run`] decides which one a machine can serve.

pub mod emlx;
pub mod epoch;
pub mod error;
pub mod keys;
pub mod mailbox;
pub mod mode;
pub mod probe;
pub mod search;
pub mod store;
pub mod urls;
pub mod verify;

pub use error::{Error, Result};
pub use mode::{Availability, Mode, Source, Strategy};
pub use probe::{Diagnosis, FallbackReason, Verdict};
pub use store::MailStore;
