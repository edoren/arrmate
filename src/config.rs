use serde::Deserialize;
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
}

#[derive(Clone, Deserialize, Debug)]
pub struct CleanupConfig {
    pub ratio: Option<String>,
    pub ignored: Option<CleanupIgnoredTrackersConfig>,
}

#[derive(Clone, Deserialize, Debug)]
pub struct RetryConfig {
    pub timeout: u64,
}

#[derive(Clone, Deserialize, Debug)]
pub struct ConfigData {
    pub cleanup: CleanupConfig,
    pub retry: Option<RetryConfig>,
    pub qbittorrent: QBittorrentConfig,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
}
