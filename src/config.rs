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

#[derive(Clone, Deserialize, Debug)]
pub struct CleanupIgnoredTrackersConfig {
    pub trackers: Option<Vec<String>>,
    pub categories: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub enum RatioQualifier {
    Less,
    LessOrEqual,
    Higher,
    HigherOrEqual,
}

#[derive(Clone, Debug)]
pub struct RatioConfig {
    pub value: f64,
    pub qualifier: RatioQualifier,
}

fn deserialize_ratio<'de, D>(deserializer: D) -> Result<Option<RatioConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let ratio_str: &str = match Deserialize::deserialize(deserializer) {
        Ok(val) => val,
        Err(_e) => return Ok(None),
    };

    let re = Regex::new(r"^(>|<|<=|>=|=)((?:[0-9]*[.])?[0-9]+)$")
        .map_err(|_e| serde::de::Error::custom("Val"))?;
    let caps = re
        .captures(ratio_str)
        .ok_or(serde::de::Error::custom("Invalid ratio format"))?;

    let ratio_qualifier_str = caps
        .get(1)
        .map(|val| val.as_str())
        .ok_or(serde::de::Error::custom("Missing ratio qualifier"))?
        .to_string();

    let ratio_qualifier = match ratio_qualifier_str.as_str() {
        ">" => RatioQualifier::Higher,
        "<" => RatioQualifier::Less,
        ">=" => RatioQualifier::HigherOrEqual,
        "<=" => RatioQualifier::LessOrEqual,
        _ => return Err(serde::de::Error::custom("Invalid ratio qualifier")),
    };

    let ratio = caps
        .get(2)
        .map(|val| val.as_str().parse::<f64>())
        .ok_or(serde::de::Error::custom("Missing ratio"))?
        .map_err(|_e| serde::de::Error::custom("Ratio is not a number"))?;

    Ok(Some(RatioConfig {
        value: ratio,
        qualifier: ratio_qualifier,
    }))
}

#[derive(Clone, Deserialize, Debug)]
pub struct CleanupConfig {
    #[serde(deserialize_with = "deserialize_ratio")]
    pub ratio: Option<RatioConfig>,
    pub ignored: Option<CleanupIgnoredTrackersConfig>,
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
