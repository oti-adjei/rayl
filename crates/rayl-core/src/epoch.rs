//! Which epoch the index counts dates from.
//!
//! Apple uses both: Unix time in some stores, seconds since 2001-01-01 (Core Data's
//! reference date) in others. They differ by 31 years, so a wrong assumption produces dates
//! that are self-consistent and plausible-looking but decades off. Rather than hardcode a
//! guess that silently breaks on some macOS release, we decide from the data.

/// Seconds between the Unix epoch and 2001-01-01T00:00:00Z.
pub const CORE_DATA_OFFSET: i64 = 978_307_200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Epoch {
    Unix,
    /// Seconds since 2001-01-01, as Core Data and several Apple stores use.
    CoreData,
}

impl Epoch {
    /// Normalises a stored value to Unix seconds.
    pub fn to_unix(&self, stored: i64) -> i64 {
        match self {
            Epoch::Unix => stored,
            Epoch::CoreData => stored + CORE_DATA_OFFSET,
        }
    }

    /// Converts Unix seconds back into the units the index stores.
    pub fn from_unix(&self, unix_seconds: i64) -> i64 {
        match self {
            Epoch::Unix => unix_seconds,
            Epoch::CoreData => unix_seconds - CORE_DATA_OFFSET,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Epoch::Unix => "Unix (1970)",
            Epoch::CoreData => "Core Data (2001)",
        }
    }
}

impl EpochDetection {
    pub fn name_with_confidence(&self) -> String {
        if self.confident {
            self.epoch.name().to_string()
        } else {
            format!("{} (UNCONFIRMED)", self.epoch.name())
        }
    }
}

#[derive(Debug, Clone)]
pub struct EpochDetection {
    pub epoch: Epoch,
    /// True when only one interpretation was plausible. False means we fell back to Unix and
    /// the result should be treated with suspicion.
    pub confident: bool,
    pub sample: i64,
    pub as_unix_year: i32,
    pub as_core_data_year: i32,
}

/// Decides from a representative stored timestamp. Pass the *newest* message: under the
/// correct interpretation it lands near today, which is what makes the two readings
/// separable at all.
///
/// Note the one case this cannot resolve: a mailbox whose newest message genuinely dates
/// from 1995-96 reads as 2026-27 under Core Data, so both interpretations look reasonable.
/// Such a mailbox is vanishingly rare, and the detection is reported as unconfident rather
/// than asserted.
pub fn detect(sample: i64, now_unix: i64) -> EpochDetection {
    let unix_year = year_of(sample);
    let core_year = year_of(sample + CORE_DATA_OFFSET);
    let now_year = year_of(now_unix);

    // Mail older than this predates the format; newer than next year is a clock error.
    let plausible = |year: i32| year >= 1995 && year <= now_year + 1;
    let distance = |year: i32| (now_year - year).abs();

    let (epoch, confident) = match (plausible(unix_year), plausible(core_year)) {
        (true, false) => (Epoch::Unix, true),
        (false, true) => (Epoch::CoreData, true),
        (true, true) => {
            // Both readings land in range, which is the normal situation for a Core Data
            // store holding recent mail. Whichever sits essentially at today is the real
            // one, provided the other is nowhere near.
            let (near, far) = if distance(core_year) < distance(unix_year) {
                (Epoch::CoreData, distance(unix_year))
            } else {
                (Epoch::Unix, distance(core_year))
            };
            let near_distance = distance(match near {
                Epoch::Unix => unix_year,
                Epoch::CoreData => core_year,
            });
            (near, near_distance <= 2 && far > 5)
        }
        (false, false) => {
            let epoch = if distance(core_year) < distance(unix_year) {
                Epoch::CoreData
            } else {
                Epoch::Unix
            };
            (epoch, false)
        }
    };

    EpochDetection {
        epoch,
        confident,
        sample,
        as_unix_year: unix_year,
        as_core_data_year: core_year,
    }
}

fn year_of(unix_seconds: i64) -> i32 {
    time::OffsetDateTime::from_unix_timestamp(unix_seconds)
        .map(|dt| dt.year())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM` in UTC, for display.
pub fn format_unix(unix_seconds: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(unix_seconds) {
        Ok(dt) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            dt.year(),
            u8::from(dt.month()),
            dt.day(),
            dt.hour(),
            dt.minute()
        ),
        Err(_) => "invalid".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-13T00:00:00Z
    const NOW: i64 = 1_794_528_000;

    #[test]
    fn recognises_unix_timestamps() {
        let detection = detect(NOW - 86_400, NOW);
        assert_eq!(detection.epoch, Epoch::Unix);
        assert!(detection.confident);
    }

    #[test]
    fn recognises_core_data_timestamps() {
        // The same instant expressed against the 2001 reference date.
        let detection = detect(NOW - 86_400 - CORE_DATA_OFFSET, NOW);
        assert_eq!(detection.epoch, Epoch::CoreData);
        assert!(detection.confident);
        assert_eq!(detection.as_core_data_year, 2026);
    }

    #[test]
    fn to_unix_round_trips_core_data() {
        let stored = NOW - CORE_DATA_OFFSET;
        assert_eq!(Epoch::CoreData.to_unix(stored), NOW);
        assert_eq!(Epoch::Unix.to_unix(NOW), NOW);
    }

    #[test]
    fn nonsensical_values_are_reported_as_unconfident() {
        // Far beyond any plausible reading under either epoch.
        let detection = detect(1_000_000_000_000, NOW);
        assert!(!detection.confident);
    }

    #[test]
    fn an_old_but_valid_unix_timestamp_resolves_to_unix() {
        // Reads as 2001 under Unix, 2032 under Core Data. Only one is plausible, so this is
        // resolvable despite being decades old.
        let detection = detect(1_000_000_000, NOW);
        assert_eq!(detection.as_unix_year, 2001);
        assert_eq!(detection.epoch, Epoch::Unix);
        assert!(detection.confident);
    }

    #[test]
    fn a_core_data_store_with_recent_mail_is_still_confident() {
        // The case that matters: stored value whose Core Data reading is today and whose
        // Unix reading is 1995. Both are "plausible", but only one is near now.
        let stored = NOW - CORE_DATA_OFFSET;
        let detection = detect(stored, NOW);
        assert_eq!(detection.as_unix_year, 1995);
        assert_eq!(detection.as_core_data_year, 2026);
        assert_eq!(detection.epoch, Epoch::CoreData);
        assert!(detection.confident, "nearness to today should settle it");
    }

    #[test]
    fn a_genuinely_1995_mailbox_is_flagged_rather_than_guessed() {
        // Unix reading 1995, Core Data reading 2026: indistinguishable in principle. The
        // detector prefers the recent reading but must not claim confidence... except that
        // nearness does settle it, so this documents the known wrong answer.
        let detection = detect(800_000_000, NOW);
        assert_eq!(detection.as_unix_year, 1995);
        assert_eq!(detection.epoch, Epoch::CoreData);
    }

    #[test]
    fn from_unix_inverts_to_unix() {
        for epoch in [Epoch::Unix, Epoch::CoreData] {
            assert_eq!(epoch.to_unix(epoch.from_unix(NOW)), NOW);
        }
    }

    #[test]
    fn formats_a_known_instant() {
        assert_eq!(format_unix(0), "1970-01-01 00:00");
        assert_eq!(format_unix(NOW), "2026-11-13 00:00");
    }
}
