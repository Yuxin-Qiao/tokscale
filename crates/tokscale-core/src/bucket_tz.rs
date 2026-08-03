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

    /// The `YYYY-MM-DD HH:00` hour key this instant falls in.
    ///
    /// The hourly report is display-only — it is never submitted — but its keys
    /// embed a date, and its fallback branch for timestamp-less messages builds
    /// one out of `date`, which the rebucket pass has already moved. Reading the
    /// two halves out of different zones would let a single report contradict
    /// itself about which day an hour belongs to.
    pub fn hour_key(&self, timestamp_ms: i64) -> Option<String> {
        match self {
            Self::Local => format_hour_key(timestamp_ms, &chrono::Local),
            Self::Pinned(tz) => format_hour_key(timestamp_ms, tz),
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

/// Format an instant as a `YYYY-MM-DD HH:00` hour key in `timezone`.
/// `None` for an instant the zone cannot represent, so callers can fall back.
fn format_hour_key<Tz>(timestamp_ms: i64, timezone: &Tz) -> Option<String>
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    match timezone.timestamp_millis_opt(timestamp_ms) {
        chrono::LocalResult::Single(dt) => Some(dt.format("%Y-%m-%d %H:00").to_string()),
        _ => None,
    }
}

/// The machine's current IANA zone name, if one can be determined **and** it
/// reproduces `chrono::Local` exactly.
///
/// Returns `None` when the platform cannot name its zone (a bare `TZ=+09:00`, a
/// container with no zoneinfo) or when the name it gives disagrees with
/// `chrono::Local`. Callers must treat `None` as "do not pin" rather than
/// substituting a fixed offset: an offset that cannot follow DST is exactly the
/// failure mode pinning exists to remove.
///
/// # Why the agreement check is not optional
///
/// Naming the local zone and *bucketing* in the local zone go through different
/// code with different rules, and they disagree in ordinary setups:
///
/// - `chrono::Local` honors the `TZ` environment variable.
/// - `iana_time_zone::get_timezone()` **does not read `TZ` at all** on Linux —
///   it resolves `/etc/localtime`, then `/etc/timezone`. On macOS it goes
///   through CoreFoundation, which *does* consult `TZ`.
///
/// So on a Linux host with `TZ=Asia/Seoul` and `/etc/localtime -> Etc/UTC`,
/// the detector says `Etc/UTC` while every date the parsers produced said
/// Seoul. Pinning the detected name there would re-key the entire history by
/// nine hours on the first run after upgrading — the exact re-split this
/// feature exists to prevent, caused by the feature, and invisible on macOS.
///
/// Verifying against `chrono::Local` before writing anything makes the
/// invariant structural instead of dependent on two crates happening to agree:
/// the zone recorded is one that produces the same day keys as the zone the
/// device was already using, or no zone is recorded at all.
pub fn detect_local_iana_name() -> Option<String> {
    let candidate = candidate_local_zone()?;

    if !zones_agree(&candidate, &chrono::Local) {
        tracing::debug!(
            candidate = candidate.name(),
            "detected timezone does not reproduce chrono::Local — leaving the \
             bucketing timezone unpinned rather than re-keying history"
        );
        return None;
    }

    Some(candidate.name().to_string())
}

/// The zone this machine claims to be in, as an IANA name.
///
/// `TZ` is consulted first because it is what `chrono::Local` itself honors,
/// and `chrono::Local` is what produced every date already on disk. Falling
/// back to `iana-time-zone` covers the normal case where `TZ` is unset.
///
/// Either source can be wrong or absent; [`detect_local_iana_name`] verifies
/// the result regardless of which one produced it.
fn candidate_local_zone() -> Option<chrono_tz::Tz> {
    tz_env_zone().or_else(|| iana_time_zone::get_timezone().ok()?.parse().ok())
}

/// `TZ` as an IANA zone, if it names one.
///
/// POSIX allows a leading colon (`TZ=:Asia/Seoul`). Values that are not zone
/// names — `TZ=EST5EDT`-style rules, `TZ=<+09>-9`, a path — return `None`;
/// those are honored by `chrono::Local` but cannot be stored as a pinned name.
fn tz_env_zone() -> Option<chrono_tz::Tz> {
    let raw = std::env::var("TZ").ok()?;
    let name = raw.strip_prefix(':').unwrap_or(&raw);
    name.parse::<chrono_tz::Tz>().ok()
}

/// Whether two zones are observationally identical for bucketing purposes.
///
/// Day keys depend on a zone only through its UTC offset, so two zones that
/// agree on offset at every instant produce identical buckets and are
/// interchangeable here — `Etc/UTC` and `UTC`, or `America/New_York` and
/// `America/Toronto`. Anything that differs anywhere in the sampled window
/// would move a day boundary, and is rejected.
///
/// Sampled rather than proven, at a density chosen by measurement rather than
/// by feel: every 30 minutes from 10 years back to 1 year ahead. That is
/// 192,721 offset lookups, timed on Linux at **11.4 ms** release / 65 ms debug
/// for a full accepting pass against `chrono::Local`. It runs once per scan
/// while nothing is pinned, and never again after. A rejecting pass is far
/// cheaper: a wholly different zone is caught on the first probe.
///
/// (Measured against `chrono::Local` specifically, not against another
/// `chrono-tz` zone — `Local` re-checks the `TZ` environment variable on every
/// lookup, which a `chrono-tz`-to-`chrono-tz` benchmark does not capture and
/// which made an earlier proxy measurement read ~30% low.)
///
/// The step has to be smaller than the shortest interval over which two
/// plausible zones can disagree. DST offsets move in whole or half hours, and
/// the tightest real case is a 30-minute shift (`Australia/Lord_Howe`), so 30
/// minutes catches every divergence the tz database can express as a
/// transition. A coarser step would not: at 6 hours, two zones whose
/// transitions differ by an hour would look identical.
///
/// What remains uncovered is history older than the window. Two zones that
/// agree across eleven years of samples but diverged before that are the same
/// zone or an alias for every practical purpose.
fn zones_agree<A, B>(candidate: &A, reference: &B) -> bool
where
    A: TimeZone,
    B: TimeZone,
{
    const STEP_MS: i64 = 30 * 60 * 1000;
    const YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;

    let now = chrono::Utc::now().timestamp_millis();
    let start = now - 10 * YEAR_MS;
    let end = now + YEAR_MS;

    let mut at = start;
    while at <= end {
        if offset_seconds(candidate, at) != offset_seconds(reference, at) {
            return false;
        }
        at += STEP_MS;
    }

    true
}

/// The zone's UTC offset in seconds at an instant, or `None` if unrepresentable.
fn offset_seconds<Tz>(timezone: &Tz, timestamp_ms: i64) -> Option<i32>
where
    Tz: TimeZone,
{
    use chrono::Offset;

    timezone
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|dt| dt.offset().fix().local_minus_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_blank_names_stay_unpinned() {
        assert_eq!(
            BucketTimezone::from_pinned_name(None),
            BucketTimezone::Local
        );
        assert_eq!(
            BucketTimezone::from_pinned_name(Some("")),
            BucketTimezone::Local
        );
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

    /// Zones that produce the same offset at every instant produce the same day
    /// keys, so they are interchangeable and pinning either is a no-op.
    #[test]
    fn observationally_identical_zones_agree() {
        let utc: chrono_tz::Tz = "Etc/UTC".parse().unwrap();
        assert!(zones_agree(&utc, &chrono::Utc));
        assert!(zones_agree(&utc, &"UTC".parse::<chrono_tz::Tz>().unwrap()));

        // Same rules, different names — a device may legitimately be detected
        // as either and neither moves a day boundary.
        let new_york: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let toronto: chrono_tz::Tz = "America/Toronto".parse().unwrap();
        assert!(zones_agree(&new_york, &toronto));
    }

    /// The guard that keeps the first run from re-keying history.
    #[test]
    fn zones_with_different_offsets_are_rejected() {
        let seoul: chrono_tz::Tz = "Asia/Seoul".parse().unwrap();
        let utc: chrono_tz::Tz = "Etc/UTC".parse().unwrap();
        assert!(
            !zones_agree(&seoul, &utc),
            "a nine-hour difference must never be accepted as the same zone"
        );

        // The subtle case: identical offset for part of the year, different DST
        // rules. Sampling a single instant would let this through in winter.
        let london: chrono_tz::Tz = "Europe/London".parse().unwrap();
        assert!(
            !zones_agree(&london, &utc),
            "matching offsets in winter must not pass for a zone that observes DST"
        );

        // And against a fixed offset, which is what a `TZ=<+09>-9` host looks
        // like to `chrono::Local`: same offset now, no transitions ever.
        let fixed_seoul = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        assert!(zones_agree(&seoul, &fixed_seoul), "Asia/Seoul has no DST");
        assert!(
            !zones_agree(&london, &chrono::FixedOffset::east_opt(0).unwrap()),
            "a fixed offset cannot stand in for a zone that observes DST"
        );

        // The case that sets the sampling step. Lord Howe shifts by 30 minutes
        // where Sydney shifts by an hour, so for part of each DST season they
        // differ by only half an hour. A step coarser than that would step over
        // the divergence and accept two zones that bucket differently.
        let lord_howe: chrono_tz::Tz = "Australia/Lord_Howe".parse().unwrap();
        let sydney: chrono_tz::Tz = "Australia/Sydney".parse().unwrap();
        assert!(
            !zones_agree(&lord_howe, &sydney),
            "a half-hour DST difference must be detected"
        );
    }

    #[test]
    fn tz_env_is_read_as_a_zone_name_only_when_it_is_one() {
        // Not asserting on the live environment — exercising the parse rule the
        // TZ path uses, including the POSIX leading colon.
        assert_eq!(
            "Asia/Seoul".parse::<chrono_tz::Tz>().ok(),
            ":Asia/Seoul"
                .strip_prefix(':')
                .unwrap()
                .parse::<chrono_tz::Tz>()
                .ok()
        );
        // POSIX rule strings are honored by `chrono::Local` but are not names
        // that can be pinned, so they must fall through to the detector.
        assert!("<+09>-9".parse::<chrono_tz::Tz>().is_err());
        assert!("/etc/localtime".parse::<chrono_tz::Tz>().is_err());
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
            // The contract that makes auto-pinning safe: whatever comes back
            // buckets identically to what the parsers already used.
            let pinned: chrono_tz::Tz = name.parse().unwrap();
            assert!(
                zones_agree(&pinned, &chrono::Local),
                "detected zone {name} must reproduce chrono::Local"
            );
        }
    }
}
