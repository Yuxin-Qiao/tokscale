//! The timezone a scan buckets usage into.
//!
//! Which local calendar day a unit of usage lands in used to be a function of
//! the machine's timezone *at scan time*: every date string was derived from
//! `chrono::Local`, read afresh on every run. Rescanning the same history from
//! another zone therefore re-split it across days, and the server's monotonic
//! per-day guard kept the stale value on one day while accepting the new one on
//! its neighbour — inflating the total permanently, with no way to walk it back.
//!
//! The bucket key has to come from a value the machine cannot silently change
//! underneath a rescan. So the CLI records the zone once and reuses it.
//!
//! # Why a named IANA zone and not a fixed offset
//!
//! A `chrono::FixedOffset` would need no new dependency, but it does not follow
//! DST. Pin `UTC+09:00` in a zone that observes DST and the pinned offset stops
//! matching local midnight the moment the transition happens, so usage within an
//! hour of the boundary lands on the wrong day — a bounded re-run of the very
//! bug being removed here. A named zone carries the transition rules, so local
//! midnight stays local midnight and the fix is exact rather than approximate.
//!
//! # Unpinned is not "pinned to Local"
//!
//! [`BucketTimezone::Local`] exists so that a device which has never pinned
//! keeps today's semantics *exactly*. Callers are expected to skip the rebucket
//! pass entirely when [`BucketTimezone::is_pinned`] is false rather than
//! re-derive dates through `Local`, so an unpinned scan does not depend on this
//! module being byte-identical to what the parsers already computed.

use std::fmt::Display;

use chrono::TimeZone;

/// The zone a scan buckets its day keys into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BucketTimezone {
    /// No zone pinned. Day keys follow `chrono::Local`, re-read every scan.
    /// This is the pre-pinning behaviour and stays the default so an existing
    /// install does not change what it reports until it pins.
    #[default]
    Local,
    /// A pinned IANA zone. Day keys are stable across rescans, machine
    /// relocations, and `TZ` changes.
    Pinned(chrono_tz::Tz),
}

impl BucketTimezone {
    /// Resolve a configured zone name.
    ///
    /// An absent, empty, or unparseable name yields [`BucketTimezone::Local`].
    /// A stale or hand-typo'd `bucketTimezone` must never break a scan — the
    /// same lossy-config posture the rest of settings.json takes — so this
    /// degrades to today's behaviour instead of erroring.
    pub fn from_pinned_name(raw: Option<&str>) -> Self {
        let Some(name) = raw.map(str::trim).filter(|name| !name.is_empty()) else {
            return Self::Local;
        };

        match name.parse::<chrono_tz::Tz>() {
            Ok(tz) => Self::Pinned(tz),
            Err(_) => {
                tracing::warn!(
                    timezone = name,
                    "scanner.bucketTimezone is not a known IANA zone name — \
                     falling back to the machine's local timezone"
                );
                Self::Local
            }
        }
    }

    /// Read the pinned zone out of scanner settings.
    pub fn from_scanner_settings(settings: &crate::scanner::ScannerSettings) -> Self {
        Self::from_pinned_name(settings.bucket_timezone.as_deref())
    }

    /// The canonical IANA name of the pinned zone, or `None` when unpinned.
    pub fn pinned_name(&self) -> Option<&'static str> {
        match self {
            Self::Local => None,
            Self::Pinned(tz) => Some(tz.name()),
        }
    }

    /// Whether a zone is pinned. Callers use this to skip the rebucket pass
    /// entirely rather than re-derive dates through `Local`.
    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Pinned(_))
    }

    /// Today's date in this zone.
    ///
    /// `--today` / `--week` / `--month` filter on the same `date` strings the
    /// buckets are keyed by, so they have to agree on where a day starts. A
    /// device pinned to `Asia/Seoul` and run from California would otherwise
    /// select the host's today out of Seoul-keyed buckets and submit a partial
    /// day — and a partial day is exactly what the server's monotonic guard
    /// then freezes.
    pub fn today(&self) -> chrono::NaiveDate {
        let now = chrono::Utc::now();
        match self {
            Self::Local => now.with_timezone(&chrono::Local).date_naive(),
            Self::Pinned(tz) => now.with_timezone(tz).date_naive(),
        }
    }

    /// The `YYYY-MM-DD` day key this instant falls in.
    pub fn day_key(&self, timestamp_ms: i64) -> String {
        match self {
            Self::Local => format_day_key(timestamp_ms, &chrono::Local),
            Self::Pinned(tz) => format_day_key(timestamp_ms, tz),
        }
    }
}

/// Format an instant as a `YYYY-MM-DD` day key in `timezone`.
///
/// Returns an empty string for an instant the zone cannot represent, matching
/// what the pre-pinning `timestamp_to_date` did. Mapping an *instant* into a
/// zone is unambiguous for every real zone — the ambiguity in `chrono` runs the
/// other way, local wall-clock to instant — so the non-`Single` arm is a
/// defensive floor, not a live path.
pub(crate) fn format_day_key<Tz>(timestamp_ms: i64, timezone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    match timezone.timestamp_millis_opt(timestamp_ms) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

/// The machine's current IANA zone name, if it can be determined.
///
/// `iana-time-zone` is already in the dependency graph — `chrono` pulls it in
/// to implement `Local` — so reading the name costs no new code in the binary.
///
/// Returns `None` when the platform cannot name its zone (a bare `TZ=+09:00`,
/// a container with no zoneinfo). Callers must treat that as "do not pin"
/// rather than substituting a fixed offset: an offset that cannot follow DST is
/// exactly the failure mode pinning exists to remove.
pub fn detect_local_iana_name() -> Option<String> {
    let name = iana_time_zone::get_timezone().ok()?;
    // Round-trip through the tz database. A name we cannot parse back is a
    // name we could not honor on a later scan, and pinning it would silently
    // fall back to `Local` forever.
    name.parse::<chrono_tz::Tz>()
        .ok()
        .map(|tz| tz.name().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_blank_names_stay_unpinned() {
        assert_eq!(BucketTimezone::from_pinned_name(None), BucketTimezone::Local);
        assert_eq!(BucketTimezone::from_pinned_name(Some("")), BucketTimezone::Local);
        assert_eq!(
            BucketTimezone::from_pinned_name(Some("   ")),
            BucketTimezone::Local
        );
        assert!(!BucketTimezone::from_pinned_name(None).is_pinned());
    }

    #[test]
    fn unknown_zone_name_degrades_to_local_instead_of_failing() {
        assert_eq!(
            BucketTimezone::from_pinned_name(Some("Mars/Olympus_Mons")),
            BucketTimezone::Local
        );
        // A fixed-offset string is not an IANA name and must not be accepted:
        // silently honoring it would pin a zone that cannot follow DST.
        assert_eq!(
            BucketTimezone::from_pinned_name(Some("+09:00")),
            BucketTimezone::Local
        );
    }

    #[test]
    fn pinned_zone_keys_the_same_instant_the_same_way_regardless_of_host() {
        let tz = BucketTimezone::from_pinned_name(Some("Asia/Seoul")).clone();
        assert_eq!(tz.pinned_name(), Some("Asia/Seoul"));
        assert!(tz.is_pinned());

        // 2026-03-02T18:00:00Z — 2026-03-03 03:00 in Seoul, 2026-03-02 10:00 in
        // Los Angeles. The day key follows the pinned zone, not the host.
        let instant = 1_772_474_400_000;
        assert_eq!(tz.day_key(instant), "2026-03-03");
        assert_eq!(
            BucketTimezone::from_pinned_name(Some("America/Los_Angeles")).day_key(instant),
            "2026-03-02"
        );
    }

    /// The reason this module does not use `FixedOffset`. A zone that observes
    /// DST changes its offset mid-year; an offset pinned before the transition
    /// keys instants after it onto the wrong day near midnight.
    #[test]
    fn named_zone_follows_dst_where_a_fixed_offset_would_not() {
        let ny = BucketTimezone::from_pinned_name(Some("America/New_York"));

        // 2026-01-15T04:30:00Z — 23:30 on the 14th in EST (UTC-5).
        let winter = chrono::DateTime::parse_from_rfc3339("2026-01-15T04:30:00Z")
            .unwrap()
            .timestamp_millis();
        // 2026-07-15T03:30:00Z — 23:30 on the 14th in EDT (UTC-4).
        let summer = chrono::DateTime::parse_from_rfc3339("2026-07-15T03:30:00Z")
            .unwrap()
            .timestamp_millis();

        assert_eq!(ny.day_key(winter), "2026-01-14");
        assert_eq!(ny.day_key(summer), "2026-07-14");

        // The same instants under the winter offset frozen as a fixed value:
        // the summer one lands a day late.
        let frozen = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
        assert_eq!(format_day_key(winter, &frozen), "2026-01-14");
        assert_eq!(
            format_day_key(summer, &frozen),
            "2026-07-14",
            "sanity: 03:30Z is 22:30 EST, still the 14th"
        );

        // And where it actually bites: 00:30 EDT on the 15th is 23:30 EST on
        // the 14th under a frozen winter offset.
        let after_midnight_edt = chrono::DateTime::parse_from_rfc3339("2026-07-15T04:30:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(ny.day_key(after_midnight_edt), "2026-07-15");
        assert_eq!(
            format_day_key(after_midnight_edt, &frozen),
            "2026-07-14",
            "a frozen offset buckets an hour of every DST-shifted day onto the wrong date"
        );
    }

    #[test]
    fn detection_either_names_a_real_zone_or_declines() {
        // Host-dependent, so assert the contract rather than a value: whatever
        // comes back must round-trip through the tz database, because a name
        // that does not would pin to something later scans silently ignore.
        if let Some(name) = detect_local_iana_name() {
            assert!(
                BucketTimezone::from_pinned_name(Some(&name)).is_pinned(),
                "detected zone {name} must be re-resolvable"
            );
        }
    }
}
