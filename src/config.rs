use regex::Regex;
use serde::{Deserialize, Deserializer};
use url::Url;

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

fn default_false() -> bool {
    false
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
    HardLinks,
}

#[derive(Clone, Deserialize, Debug)]
pub struct TrackerConfig {
    pub name: String,
    pub domains: Vec<String>,
    pub ratio: Option<f64>,
    pub seeding_time: Option<u64>,
    #[serde(default = "default_false")]
    pub require_ratio_and_seeding_time: bool,
    #[serde(default = "default_hard_links_percentage")]
    pub hard_links_percentage: u64,
    pub ignore: Option<TrackerIgnore>,
}

#[derive(Clone, Deserialize, Debug)]
pub struct CategoriesConfig {
    pub name: String,
    pub ignore: Option<bool>,
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
    pub timeout: u64,
    pub dry_run: Option<bool>,
}

fn deserialize_refresh_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer)
        .or_else(|_e| Ok(60))
        .and_then(|s| {
            if s >= 60 && s <= 3600 {
                Ok(s)
            } else {
                Err(serde::de::Error::custom(
                    "Interval should be between 60 and 3600 seconds",
                ))
            }
        })
}

#[derive(Clone, Deserialize, Debug)]
pub struct ConfigData {
    #[serde(deserialize_with = "deserialize_refresh_interval")]
    pub refresh_interval: u64,
    pub cleanup: CleanupConfig,
    pub retry: Option<RetryConfig>,
    pub qbittorrent: QBittorrentConfig,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
    pub dry_run: Option<bool>,
}
