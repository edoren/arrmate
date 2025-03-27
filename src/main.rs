use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use log::{debug, info};
use qbit_rs::{
    Qbit,
    model::{Credential, GetTorrentListArg, Torrent},
};

use regex::Regex;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::fs;
use url::Url;

mod config;
mod radarr_api;
mod sonarr_api;
use config::{
    CleanupConfig, ConfigData, QBittorrentConfig, RadarrConfig, RetryConfig, SonarrConfig,
};
use radarr_api::RadarrAPI;
use sonarr_api::SonarrAPI;

#[async_trait]
trait TorrentFilter {
    async fn filter(&self, api: &Qbit, torrents: Vec<Torrent>) -> Result<Vec<Torrent>>;
}

enum RatioQualifier {
    Less,
    LessOrEqual,
    Higher,
    HigherOrEqual,
}

struct RatioFilter {
    ratio: Option<f64>,
    ratio_qualifier: RatioQualifier,
}

impl RatioFilter {
    fn new(ratio: Option<String>) -> Result<Self> {
        let ratio = match ratio {
            Some(val) => val,
            None => {
                return Ok(Self {
                    ratio: None,
                    ratio_qualifier: RatioQualifier::Less,
                });
            }
        };

        let re = Regex::new(r"^(>|<|<=|>=|=)((?:[0-9]*[.])?[0-9]+)$")?;
        let caps = re.captures(&ratio).ok_or(anyhow!("Invalid ratio format"))?;

        let ratio_qualifier_str = caps
            .get(1)
            .map(|val| val.as_str())
            .ok_or(anyhow!("Missing ratio qualifier"))?
            .to_string();

        let ratio_qualifier = match ratio_qualifier_str.as_str() {
            ">" => RatioQualifier::Higher,
            "<" => RatioQualifier::Less,
            ">=" => RatioQualifier::HigherOrEqual,
            "<=" => RatioQualifier::LessOrEqual,
            _ => return Err(anyhow!("Invalid ratio qualifier")),
        };

        let ratio = Some(
            caps.get(2)
                .map(|val| val.as_str().parse::<f64>().unwrap())
                .ok_or(anyhow!("Missing ratio"))?,
        );

        Ok(Self {
            ratio,
            ratio_qualifier,
        })
    }
}

#[async_trait]
impl TorrentFilter for RatioFilter {
    async fn filter(&self, _api: &Qbit, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let ratio = match self.ratio {
            Some(val) => val,
            None => return Ok(torrents),
        };

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            torrent
                .ratio
                .is_some_and(|torrent_ratio| match self.ratio_qualifier {
                    RatioQualifier::Higher => torrent_ratio > ratio,
                    RatioQualifier::Less => torrent_ratio < ratio,
                    RatioQualifier::HigherOrEqual => torrent_ratio >= ratio,
                    RatioQualifier::LessOrEqual => torrent_ratio <= ratio,
                })
        });
        Ok(torrents)
    }
}

struct TrackerFilter {
    ignored_trackers: Option<Vec<String>>,
}

impl TrackerFilter {
    fn new(ignored_trackers: Option<Vec<String>>) -> Self {
        Self { ignored_trackers }
    }
}

#[async_trait]
impl TorrentFilter for TrackerFilter {
    async fn filter(&self, api: &Qbit, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let ignored_trackers = match self.ignored_trackers.as_ref() {
            Some(ignored_trackers) => ignored_trackers,
            None => return Ok(torrents),
        };

        let mut result = vec![];
        for torrent in torrents {
            let trackers = if let Some(hash) = &torrent.hash {
                api.get_torrent_trackers(hash).await?
            } else {
                vec![]
            };

            let ignored = trackers
                .iter()
                .map(|t| Url::parse(&t.url))
                .any(|url_result| {
                    url_result.is_ok_and(|url| {
                        url.domain()
                            .is_some_and(|val| ignored_trackers.contains(&val.to_string()))
                    })
                });

            if !ignored {
                result.push(torrent);
            }
        }
        Ok(result)
    }
}

struct SonarrFilter {
    sonarr_config: Option<SonarrConfig>,
}

impl SonarrFilter {
    fn new(sonarr_config: Option<SonarrConfig>) -> Self {
        SonarrFilter { sonarr_config }
    }
}

#[async_trait]
impl TorrentFilter for SonarrFilter {
    async fn filter(&self, _api: &Qbit, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let api = match self.sonarr_config.as_ref() {
            Some(config) => SonarrAPI::new(config)?,
            None => return Ok(torrents),
        };

        let queue_items = api
            .get_queue()
            .await?
            .records
            .unwrap_or_default()
            .unwrap_or_default();
        debug!("Sonarr Queue: {}", queue_items.len());

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            if let Some(hash) = &torrent.hash {
                for queue_item in &queue_items {
                    let download_id = queue_item
                        .download_id
                        .clone()
                        .unwrap_or_default()
                        .unwrap_or_default()
                        .to_lowercase();
                    if download_id == hash.to_lowercase() {
                        debug!("- {:?} -> Ignored", queue_item.title);
                        return false;
                    }
                }
            }
            return true;
        });

        Ok(torrents)
    }
}

struct RadarrFilter {
    radarr_config: Option<RadarrConfig>,
}

impl RadarrFilter {
    fn new(radarr_config: Option<RadarrConfig>) -> Self {
        RadarrFilter { radarr_config }
    }
}

#[async_trait]
impl TorrentFilter for RadarrFilter {
    async fn filter(&self, _api: &Qbit, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let api = match self.radarr_config.as_ref() {
            Some(config) => RadarrAPI::new(config)?,
            None => return Ok(torrents),
        };

        let queue_items = api
            .get_queue()
            .await?
            .records
            .unwrap_or_default()
            .unwrap_or_default();
        debug!("Radarr Queue: {}", queue_items.len());

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            if let Some(hash) = &torrent.hash {
                for queue_item in &queue_items {
                    let download_id = queue_item
                        .download_id
                        .clone()
                        .unwrap_or_default()
                        .unwrap_or_default()
                        .to_lowercase();
                    if download_id == hash.to_lowercase() {
                        info!("- {:?} -> Ignored", queue_item.title);
                        return false;
                    }
                }
            }
            return true;
        });

        Ok(torrents)
    }
}

async fn execute_cleanup(
    cleanup_config: CleanupConfig,
    qbittorrent_config: QBittorrentConfig,
    sonarr_config: Option<SonarrConfig>,
    radarr_config: Option<RadarrConfig>,
) -> Result<()> {
    let credential = Credential::new(qbittorrent_config.username, qbittorrent_config.password);
    let api = Qbit::new(qbittorrent_config.host, credential);

    let mut torrents = api.get_torrent_list(GetTorrentListArg::default()).await?;

    let mut filters: Vec<Box<dyn TorrentFilter>> = Vec::new();
    filters.push(Box::new(RatioFilter::new(cleanup_config.ratio)?));
    filters.push(Box::new(TrackerFilter::new(
        cleanup_config.ignored.and_then(|i| i.trackers),
    )));
    filters.push(Box::new(SonarrFilter::new(sonarr_config)));
    filters.push(Box::new(RadarrFilter::new(radarr_config)));

    for filter in filters {
        torrents = filter.filter(&api, torrents).await?;
    }

    if torrents.is_empty() {
        info!("No torrents to delete");
        return Ok(());
    }

    info!("The following torrents will be deleted:");
    for torrent in &torrents {
        if let Some(name) = &torrent.name {
            info!("- {name}");
        }
    }

    let torrent_hashes = torrents
        .iter()
        .filter_map(|t| t.hash.clone())
        .collect::<Vec<String>>();

    match api.delete_torrents(torrent_hashes, Some(true)).await {
        Ok(_) => info!("Torrents deleted"),
        Err(_) => info!("Failed to delete torrents"),
    }

    Ok(())
}

async fn execute_retry(
    retry_config: Option<RetryConfig>,
    sonarr_config: Option<SonarrConfig>,
    radarr_config: Option<RadarrConfig>,
) -> Result<()> {
    let retry_config = match retry_config.as_ref() {
        Some(config) => config,
        None => return Ok(()),
    };
    let sonarr_api = match sonarr_config.as_ref() {
        Some(config) => SonarrAPI::new(config)?,
        None => return Ok(()),
    };
    let radarr_api = match radarr_config.as_ref() {
        Some(config) => RadarrAPI::new(config)?,
        None => return Ok(()),
    };

    let sonarr_queue_items = sonarr_api
        .get_queue()
        .await?
        .records
        .unwrap_or_default()
        .unwrap_or_default();

    let radarr_queue_items = radarr_api
        .get_queue()
        .await?
        .records
        .unwrap_or_default()
        .unwrap_or_default();

    let mut sonarr_ids_to_remove = Vec::new();
    let mut sonarr_ids_to_remove_and_blocklist = Vec::new();

    let mut radarr_ids_to_remove = Vec::new();
    let mut radarr_ids_to_remove_and_blocklist = Vec::new();

    for resource in sonarr_queue_items {
        if resource.id.is_none() {
            continue;
        }

        let mut add_to_remove = false;
        let mut add_to_blocklist = false;

        if resource.status == Some(sonarr::models::QueueStatus::Queued) {
            if let Some(added_time) = resource
                .added
                .clone()
                .unwrap_or_default()
                .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
            {
                let timeout_datetime = added_time + Duration::from_secs(retry_config.timeout);
                if OffsetDateTime::now_utc() > timeout_datetime {
                    add_to_remove = true;
                }
            }
        }

        if resource.status == Some(sonarr::models::QueueStatus::Completed)
            && resource.tracked_download_status
                == Some(sonarr::models::TrackedDownloadStatus::Warning)
        {
            if resource.tracked_download_state
                == Some(sonarr::models::TrackedDownloadState::ImportPending)
            {
                for sonarr::models::TrackedDownloadStatusMessage { title: _, messages } in resource
                    .status_messages
                    .clone()
                    .unwrap_or_default()
                    .unwrap_or_default()
                {
                    if messages
                        .unwrap_or_default()
                        .unwrap_or_default()
                        .iter()
                        .any(|msg| msg.contains("Found potentially dangerous file"))
                    {
                        add_to_remove = true;
                        add_to_blocklist = true;
                        break;
                    }
                }
            }

            // if let Some(completion_time) = resource
            //     .estimated_completion_time
            //     .clone()
            //     .unwrap_or_default()
            //     .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
            // {
            //     let timeout_datetime = completion_time + Duration::from_secs(retry_config.timeout);
            //     if OffsetDateTime::now_utc() > timeout_datetime {
            //         add_to_remove = true;
            //     }
            // }
        }

        if add_to_remove {
            if add_to_blocklist {
                sonarr_ids_to_remove_and_blocklist.push(resource);
            } else {
                sonarr_ids_to_remove.push(resource);
            }
        }
    }

    if !sonarr_ids_to_remove.is_empty() {
        let removed: Vec<&String> = sonarr_ids_to_remove
            .iter()
            .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
            .collect();
        info!("Following queue removed: {removed:?}");
        sonarr_api
            .queue_id_delete_bulk(
                sonarr_ids_to_remove
                    .into_iter()
                    .filter_map(|res| res.id)
                    .collect(),
                Some(true),
                Some(false),
                Some(false),
                Some(false),
            )
            .await?;
    }
    if !sonarr_ids_to_remove_and_blocklist.is_empty() {
        let removed: Vec<&String> = sonarr_ids_to_remove_and_blocklist
            .iter()
            .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
            .collect();
        info!("Following queue removed and blocked: {removed:?}");
        sonarr_api
            .queue_id_delete_bulk(
                sonarr_ids_to_remove_and_blocklist
                    .into_iter()
                    .filter_map(|res| res.id)
                    .collect(),
                Some(true),
                Some(true),
                Some(false),
                Some(false),
            )
            .await?;
    }

    for resource in radarr_queue_items {
        if resource.id.is_none() {
            continue;
        }

        let mut add_to_remove = false;
        let mut add_to_blocklist = false;

        if resource.tracked_download_status == Some(radarr::models::TrackedDownloadStatus::Warning)
        {
            if resource.tracked_download_state
                == Some(radarr::models::TrackedDownloadState::ImportPending)
            {
                for radarr::models::TrackedDownloadStatusMessage { title: _, messages } in resource
                    .status_messages
                    .clone()
                    .unwrap_or_default()
                    .unwrap_or_default()
                {
                    if messages
                        .unwrap_or_default()
                        .unwrap_or_default()
                        .iter()
                        .any(|msg| msg.contains("Found potentially dangerous file"))
                    {
                        add_to_remove = true;
                        add_to_blocklist = true;
                        break;
                    }
                }
            }

            if let Some(completion_time) = resource
                .estimated_completion_time
                .clone()
                .unwrap_or_default()
                .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
            {
                let timeout_datetime = completion_time + Duration::from_secs(retry_config.timeout);
                if OffsetDateTime::now_utc() > timeout_datetime {
                    add_to_remove = true;
                }
            }
        }

        if add_to_remove {
            if add_to_blocklist {
                radarr_ids_to_remove_and_blocklist.push(resource);
            } else {
                radarr_ids_to_remove.push(resource);
            }
        }
    }

    if !radarr_ids_to_remove.is_empty() {
        let removed: Vec<&String> = radarr_ids_to_remove
            .iter()
            .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
            .collect();
        info!("Following queue removed: {removed:?}");
        radarr_api
            .queue_id_delete_bulk(
                radarr_ids_to_remove
                    .into_iter()
                    .filter_map(|res| res.id)
                    .collect(),
                Some(true),
                Some(false),
                Some(false),
                Some(false),
            )
            .await?;
    }
    if !radarr_ids_to_remove_and_blocklist.is_empty() {
        let removed: Vec<&String> = radarr_ids_to_remove_and_blocklist
            .iter()
            .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
            .collect();
        info!("Following queue removed and blocked: {removed:?}");
        radarr_api
            .queue_id_delete_bulk(
                radarr_ids_to_remove_and_blocklist
                    .into_iter()
                    .filter_map(|res| res.id)
                    .collect(),
                Some(true),
                Some(true),
                Some(false),
                Some(false),
            )
            .await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    edolib::log::setup("arrmate").await?;

    let configs: Vec<ConfigData> = serde_yaml::from_str(
        match fs::read_to_string("config.yaml").await {
            Ok(data) => Ok(data),
            Err(_) => fs::read_to_string("config.yml").await,
        }
        .map_err(|e| anyhow!("Failed to read config file: {e}"))?
        .as_str(),
    )?;

    for ConfigData {
        qbittorrent: qbittorrent_config,
        cleanup: cleanup_config,
        retry: retry_config,
        sonarr: sonarr_config,
        radarr: radarr_config,
        ..
    } in configs
    {
        execute_cleanup(
            cleanup_config,
            qbittorrent_config,
            sonarr_config.clone(),
            radarr_config.clone(),
        )
        .await?;

        execute_retry(retry_config, sonarr_config, radarr_config).await?;
    }

    return Ok(());
}
