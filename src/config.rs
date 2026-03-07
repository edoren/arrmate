use std::time::Duration;

use serde::{Deserialize, Deserializer};
use url::Url;

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

#[derive(Clone, Deserialize, Debug)]
pub struct CleanupConfig {
    pub ratio: Option<f64>,
    pub trackers: Option<Vec<TrackerConfig>>,
    pub categories: Option<Vec<CategoriesConfig>>,
    pub dry_run: Option<bool>,
}

#[derive(Clone, Deserialize, Debug)]
pub struct RetryConfig {
    #[serde(with = "humantime_serde")]
    #[allow(unused)]
    pub timeout: Duration,
    pub dry_run: Option<bool>,
}

fn deserialize_refresh_interval<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let dur: Duration = humantime_serde::deserialize(deserializer)?;
    if dur < Duration::from_secs(60) || dur > Duration::from_secs(3600) {
        return Err(serde::de::Error::custom(
            "refresh_interval should be between 1min and 1h",
        ));
    }
    Ok(dur)
}

#[derive(Clone, Deserialize, Debug)]
pub struct ConfigData {
    #[serde(deserialize_with = "deserialize_refresh_interval")]
    pub refresh_interval: Duration,
    pub cleanup: Option<CleanupConfig>,
    pub retry: Option<RetryConfig>,
    pub qbittorrent: QBittorrentConfig,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
    pub dry_run: Option<bool>,
}
