use std::{str::FromStr, time::Duration};

use chrono::TimeZone;
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Schedule(String);

impl Schedule {
    pub fn next_date(&self, from: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
        let schedule = croner::Cron::from_str(&self.0).ok();
        let from_chrono = chrono::Local
            .timestamp_opt(from.unix_timestamp(), 0)
            .single();
        schedule
            .zip(from_chrono)
            .and_then(|(schedule, from_chrono)| {
                schedule.find_next_occurrence(&from_chrono, false).ok()
            })
            .and_then(|next_chrono: chrono::DateTime<chrono::Local>| {
                let offset_secs = next_chrono.offset().local_minus_utc();
                let offset = time::UtcOffset::from_whole_seconds(offset_secs.into());
                let dt = time::OffsetDateTime::from_unix_timestamp(next_chrono.timestamp()).ok();
                offset.ok().zip(dt).map(|(offset, dt)| dt.to_offset(offset))
            })
    }
}

impl std::str::FromStr for Schedule {
    type Err = croner::errors::CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        croner::Cron::from_str(s).map(|_| Schedule(s.to_owned()))
    }
}

impl<'de> Deserialize<'de> for Schedule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).and_then(|s| {
            Schedule::from_str(&s)
                .map_err(|e| serde::de::Error::custom(format!("invalid schedule: {e}")))
        })
    }
}

fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("string or list of strings")
        }

        fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![s.to_owned()])
        }

        fn visit_seq<S>(self, seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

#[derive(Clone, Deserialize, Debug)]
pub struct QBittorrentConfig {
    pub username: String,
    pub password: String,
    pub host: Url,
}

#[derive(Clone, Deserialize, Debug)]
pub struct SonarrConfig {
    pub host: Url,
    pub api_key: String,
}

#[derive(Clone, Deserialize, Debug)]
pub struct RadarrConfig {
    pub host: Url,
    pub api_key: String,
}

fn default_hard_links_percentage() -> u64 {
    50
}

#[derive(Clone, Deserialize, PartialEq, Default, Debug)]
#[serde(rename_all(serialize = "snake_case", deserialize = "snake_case"))]
pub enum TrackerIgnore {
    Never,
    Always,
    #[default]
    WhenHardLinked,
}

#[derive(Clone, Deserialize, Debug)]
pub struct TrackerConfig {
    pub name: String,
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub domain: Vec<String>,
    pub ratio: Option<f64>,
    #[serde(with = "humantime_serde::option", default)]
    pub seeding_time: Option<Duration>,
    #[serde(default)]
    pub require_both: bool,
    #[serde(default = "default_hard_links_percentage")]
    pub hard_links_percentage: u64,
    pub ignore: Option<TrackerIgnore>,
}

#[derive(Clone, Deserialize, Debug)]
pub struct CategoriesConfig {
    pub name: String,
    #[serde(default)]
    pub ignore: bool,
}

fn default_cleanup_schedule() -> Schedule {
    Schedule("*/5 * * * *".to_owned())
}

#[derive(Clone, Deserialize, Debug)]
pub struct CleanupConfig {
    #[serde(default = "default_cleanup_schedule")]
    pub schedule: Schedule,
    pub ratio: Option<f64>,
    pub trackers: Option<Vec<TrackerConfig>>,
    pub categories: Option<Vec<CategoriesConfig>>,
    pub dry_run: Option<bool>,
}

fn default_retry_schedule() -> Schedule {
    Schedule("*/5 * * * *".to_owned())
}

#[derive(Clone, Deserialize, Debug)]
pub struct RetryConfig {
    #[serde(default = "default_retry_schedule")]
    pub schedule: Schedule,
    #[serde(with = "humantime_serde::option", default)]
    #[allow(unused)]
    pub timeout: Option<Duration>,
    pub dry_run: Option<bool>,
}

#[derive(Clone, Deserialize, Debug)]
pub struct ConfigData {
    pub cleanup: Option<CleanupConfig>,
    pub retry: Option<RetryConfig>,
    pub qbittorrent: Option<QBittorrentConfig>,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── deserialize_string_or_vec ────────────────────────────────────────────

    fn parse_tracker(yaml: &str) -> TrackerConfig {
        serde_yaml::from_str(yaml).expect("valid tracker yaml")
    }

    #[test]
    fn domain_single_string() {
        let t = parse_tracker("name: t\ndomain: example.com");
        assert_eq!(t.domain, vec!["example.com"]);
    }

    #[test]
    fn domain_list_of_strings() {
        let t = parse_tracker("name: t\ndomain:\n  - a.com\n  - b.com");
        assert_eq!(t.domain, vec!["a.com", "b.com"]);
    }

    #[test]
    fn domain_invalid_type_errors() {
        let result: Result<TrackerConfig, _> = serde_yaml::from_str("name: t\ndomain: 123");
        assert!(result.is_err());
    }

    // ── TrackerConfig serde defaults ─────────────────────────────────────────

    #[test]
    fn tracker_require_both_defaults_false() {
        let t = parse_tracker("name: t\ndomain: example.com");
        assert!(!t.require_both);
    }

    #[test]
    fn tracker_hard_links_percentage_defaults_50() {
        let t = parse_tracker("name: t\ndomain: example.com");
        assert_eq!(t.hard_links_percentage, 50);
    }

    #[test]
    fn tracker_seeding_time_defaults_none() {
        let t = parse_tracker("name: t\ndomain: example.com");
        assert!(t.seeding_time.is_none());
    }

    #[test]
    fn tracker_seeding_time_parses_duration() {
        let t = parse_tracker("name: t\ndomain: example.com\nseeding_time: 2h");
        assert_eq!(t.seeding_time, Some(Duration::from_secs(7200)));
    }

    // ── TrackerIgnore deserialization ────────────────────────────────────────

    fn parse_ignore(value: &str) -> TrackerIgnore {
        let yaml = format!("name: t\ndomain: d\nignore: {value}");
        parse_tracker(&yaml).ignore.expect("ignore present")
    }

    #[test]
    fn tracker_ignore_never() {
        assert_eq!(parse_ignore("never"), TrackerIgnore::Never);
    }

    #[test]
    fn tracker_ignore_always() {
        assert_eq!(parse_ignore("always"), TrackerIgnore::Always);
    }

    #[test]
    fn tracker_ignore_when_hard_linked() {
        assert_eq!(
            parse_ignore("when_hard_linked"),
            TrackerIgnore::WhenHardLinked
        );
    }

    #[test]
    fn tracker_ignore_defaults_to_when_hard_linked() {
        assert_eq!(TrackerIgnore::default(), TrackerIgnore::WhenHardLinked);
    }

    // ── CategoriesConfig serde defaults ─────────────────────────────────────

    #[test]
    fn categories_ignore_defaults_false() {
        let c: CategoriesConfig = serde_yaml::from_str("name: movies").unwrap();
        assert!(!c.ignore);
    }

    #[test]
    fn categories_ignore_explicit_true() {
        let c: CategoriesConfig = serde_yaml::from_str("name: movies\nignore: true").unwrap();
        assert!(c.ignore);
    }

    // ── CleanupConfig serde defaults ─────────────────────────────────────

    #[test]
    fn cleanup_config_defaults() {
        let c: CleanupConfig = serde_yaml::from_str("").unwrap();
        assert_eq!(c.schedule, Schedule("*/5 * * * *".to_owned()));
    }

    // ── RetryConfig serde defaults ─────────────────────────────────────

    #[test]
    fn retry_config_defaults() {
        let c: RetryConfig = serde_yaml::from_str("").unwrap();
        assert_eq!(c.schedule, Schedule("*/5 * * * *".to_owned()));
    }

    // ── Schedule deserialization ──────────────────────────────────────────────

    fn parse_schedule(s: &str) -> Result<Schedule, serde_yaml::Error> {
        serde_yaml::from_str(&format!("'{s}'"))
    }

    #[test]
    fn schedule_parses_cron_expression() {
        let s = parse_schedule("0 */5 * * * * *").unwrap();
        assert!(matches!(s, Schedule(_)));
    }

    #[test]
    fn schedule_invalid_errors() {
        assert!(parse_schedule("not-a-schedule").is_err());
    }

    #[test]
    fn schedule_next_date_is_in_the_future() {
        let s = parse_schedule("0 */5 * * * * *").unwrap();
        let now = time::OffsetDateTime::now_utc();
        let next = s.next_date(now).expect("should have a next occurrence");
        assert!(next > now);
    }

    #[test]
    fn schedule_next_date_respects_cron_minute_alignment() {
        // "every minute at second 0" — next occurrence should be within 60 seconds
        let s = parse_schedule("0 * * * * * *").unwrap();
        let now = time::OffsetDateTime::now_utc();
        let next = s.next_date(now).expect("should have a next occurrence");
        let delta = next - time::OffsetDateTime::now_utc();
        assert!(delta <= time::Duration::seconds(60));
    }
}
