//! `tokscale config` — read and write persistent settings from the CLI.
//!
//! Currently exposes exactly one key, `timezone`, because it is the one setting
//! a user has a concrete reason to change by hand: the timezone a device
//! buckets usage days into is pinned automatically on first run, and someone
//! who relocates needs a deliberate way to move it.

use anyhow::{bail, Result};
use colored::Colorize;
use tokscale_core::bucket_tz::BucketTimezone;

use crate::tui::settings::Settings;

/// The settings `tokscale config` can address.
///
/// Kept as an explicit list rather than a free-form path into settings.json so
/// a typo is rejected instead of silently writing a key nothing reads.
const KNOWN_KEYS: &[&str] = &["timezone"];

pub fn run_get(key: &str) -> Result<()> {
    let key = normalize_key(key)?;
    let settings = Settings::load();

    match key {
        "timezone" => match settings.scanner.bucket_timezone.as_deref() {
            Some(zone) => println!("{zone}"),
            None => {
                println!("{}", "(unset)".dimmed());
                eprintln!(
                    "No bucketing timezone is pinned. Day boundaries follow this machine's \
                     current timezone and will move if it changes."
                );
            }
        },
        _ => unreachable!("normalize_key only returns keys handled here"),
    }

    Ok(())
}

pub fn run_set(key: &str, value: &str) -> Result<()> {
    let key = normalize_key(key)?;
    let mut settings = load_for_write()?;

    match key {
        "timezone" => {
            let resolved = resolve_timezone_value(value)?;
            let previous = settings.scanner.bucket_timezone.clone();
            settings.scanner.bucket_timezone = Some(resolved.clone());
            settings.save()?;

            println!("{} timezone = {}", "set".green().bold(), resolved.bold());
            match previous {
                Some(previous) if previous == resolved => {
                    println!("(unchanged)");
                }
                Some(previous) => {
                    println!("  was: {previous}");
                    // Say the cost out loud. Repointing the zone re-keys every
                    // day boundary, so the next scan reports a different split
                    // of the same history — once.
                    eprintln!(
                        "Day boundaries move to {resolved}. The next scan re-splits history \
                         across days one final time, then stays stable."
                    );
                }
                None => {
                    eprintln!(
                        "Day boundaries are now fixed to {resolved} and no longer follow this \
                         machine's timezone."
                    );
                }
            }
        }
        _ => unreachable!("normalize_key only returns keys handled here"),
    }

    Ok(())
}

pub fn run_unset(key: &str) -> Result<()> {
    let key = normalize_key(key)?;
    let mut settings = load_for_write()?;

    match key {
        "timezone" => {
            let previous = settings.scanner.bucket_timezone.take();
            settings.save()?;

            match previous {
                Some(previous) => println!("{} timezone (was {previous})", "unset".green().bold()),
                None => println!("timezone was already unset"),
            }
            // Unset means "re-detect", not "stop pinning": the next run pins
            // this machine's current zone again. That is the useful reading —
            // it is how someone who moved re-pins without typing a zone name.
            eprintln!(
                "The next tokscale run re-pins this machine's current timezone. \
                 Use `tokscale config set timezone <zone>` to choose one explicitly."
            );
        }
        _ => unreachable!("normalize_key only returns keys handled here"),
    }

    Ok(())
}

pub fn run_list() -> Result<()> {
    let settings = Settings::load();

    let timezone = settings
        .scanner
        .bucket_timezone
        .clone()
        .unwrap_or_else(|| "(unset)".to_string());
    println!("{:<12} {}", "timezone", timezone);

    Ok(())
}

/// Load settings for a command that is going to write them straight back.
///
/// `Settings::load()` answers an unparseable settings.json with
/// `Settings::default()`, so saving after it would replace a file we could not
/// read with defaults we invented — losing scanner paths, aliases, autosubmit
/// config and UI preferences to fix one field. `tokscale config` is a
/// deliberate, interactive command, so it says so and stops instead of guessing
/// which is worse.
fn load_for_write() -> Result<Settings> {
    let (settings, origin) = Settings::load_with_origin();
    if !origin.is_safe_to_overwrite() {
        // Deliberately does not name settings.json: this also fires when the
        // config *directory* cannot be resolved or created, where there is no
        // file to fix or remove and telling someone to delete one is a dead end.
        bail!(
            "could not read this machine's tokscale settings, so writing them would \
             replace every setting with a default. Check that the config directory \
             is readable and writable, and that settings.json in it is valid JSON."
        );
    }
    Ok(settings)
}

fn normalize_key(key: &str) -> Result<&'static str> {
    let candidate = key.trim().to_ascii_lowercase();
    KNOWN_KEYS
        .iter()
        .copied()
        .find(|known| *known == candidate)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown config key `{key}` (known keys: {})",
                KNOWN_KEYS.join(", ")
            )
        })
}

/// Resolve a user-supplied timezone value to a canonical IANA name.
///
/// `auto` re-detects from the machine. Anything else must be a name the tz
/// database knows: a raw UTC offset is rejected rather than accepted as a fixed
/// offset, because an offset cannot follow DST and a pinned offset drifts off
/// local midnight twice a year — the failure pinning exists to prevent.
fn resolve_timezone_value(value: &str) -> Result<String> {
    let trimmed = value.trim();

    if trimmed.eq_ignore_ascii_case("auto") {
        return tokscale_core::bucket_tz::detect_local_iana_name().ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine this machine's IANA timezone name — \
                 pass one explicitly, e.g. `tokscale config set timezone Asia/Seoul`"
            )
        });
    }

    match BucketTimezone::from_pinned_name(Some(trimmed)) {
        BucketTimezone::Pinned(tz) => Ok(tz.name().to_string()),
        BucketTimezone::Local => bail!(
            "`{trimmed}` is not a known IANA timezone name (expected something like \
             `Asia/Seoul` or `America/New_York`). Fixed UTC offsets are not accepted: \
             they cannot follow daylight saving time, so a pinned offset would drift \
             off local midnight twice a year."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_keys_are_matched_case_insensitively_and_trimmed() {
        assert_eq!(normalize_key(" TimeZone ").unwrap(), "timezone");
        assert!(normalize_key("timezon").is_err());
        assert!(normalize_key("scanner.bucketTimezone").is_err());
    }

    #[test]
    fn timezone_values_canonicalize_through_the_tz_database() {
        assert_eq!(resolve_timezone_value("Asia/Seoul").unwrap(), "Asia/Seoul");
        assert_eq!(
            resolve_timezone_value("  America/New_York  ").unwrap(),
            "America/New_York"
        );
    }

    #[test]
    fn fixed_offsets_are_rejected_rather_than_pinned() {
        for value in ["+09:00", "UTC+9", "-0500", "9"] {
            let error = resolve_timezone_value(value)
                .expect_err("a fixed offset must not be accepted as a pinned zone");
            assert!(
                error.to_string().contains("not a known IANA timezone name"),
                "unexpected error for {value}: {error}"
            );
        }
    }

    #[test]
    fn utc_is_a_real_zone_and_stays_acceptable() {
        // `UTC` is in the tz database and has no DST, so unlike `+00:00` it is
        // a legitimate pin — useful for servers and CI that genuinely run on it.
        assert_eq!(resolve_timezone_value("UTC").unwrap(), "UTC");
    }
}
