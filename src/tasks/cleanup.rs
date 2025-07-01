use std::{collections::HashSet, fmt::Debug, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info, trace};
use qbit_rs::{
    Qbit,
    model::{Credential, GetTorrentListArg, Torrent},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

use crate::{
    apis::{radarr::RadarrAPI, sonarr::SonarrAPI},
    config::{
        CleanupConfig, QBittorrentConfig, RadarrConfig, RatioConfig, RatioQualifier, SonarrConfig,
    },
};

#[async_trait]
trait TorrentFilter {
    fn name(&self) -> String;
    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>>;
}

impl Debug for dyn TorrentFilter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.name())
    }
}

struct RatioFilter {
    ratio: Option<RatioConfig>,
}

impl RatioFilter {
    fn new(ratio: Option<RatioConfig>) -> Self {
        Self { ratio }
    }
}

#[async_trait]
impl TorrentFilter for RatioFilter {
    fn name(&self) -> String {
        "RatioFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let ratio = match self.ratio.as_ref() {
            Some(val) => val,
            None => return Ok(torrents),
        };

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            torrent
                .ratio
                .is_some_and(|torrent_ratio| match ratio.qualifier {
                    RatioQualifier::Higher => torrent_ratio > ratio.value,
                    RatioQualifier::Less => torrent_ratio < ratio.value,
                    RatioQualifier::HigherOrEqual => torrent_ratio >= ratio.value,
                    RatioQualifier::LessOrEqual => torrent_ratio <= ratio.value,
                })
        });
        Ok(torrents)
    }
}

struct TrackerFilter {
    api: Arc<Qbit>,
    ignored_trackers: Option<Vec<String>>,
}

impl TrackerFilter {
    fn new(api: Arc<Qbit>, ignored_trackers: Option<Vec<String>>) -> Self {
        Self {
            api,
            ignored_trackers,
        }
    }
}

#[async_trait]
impl TorrentFilter for TrackerFilter {
    fn name(&self) -> String {
        "TrackerFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let ignored_trackers = match self.ignored_trackers.as_ref() {
            Some(ignored_trackers) => ignored_trackers,
            None => return Ok(torrents),
        };

        let mut result = vec![];
        for torrent in torrents {
            let trackers = if let Some(hash) = &torrent.hash {
                self.api.get_torrent_trackers(hash).await?
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

struct CategoriesFilter {
    ignored_categories: Option<Vec<String>>,
}

impl CategoriesFilter {
    fn new(ignored_categories: Option<Vec<String>>) -> Self {
        Self { ignored_categories }
    }
}

#[async_trait]
impl TorrentFilter for CategoriesFilter {
    fn name(&self) -> String {
        "CategoriesFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let ignored_categories = match self.ignored_categories.as_ref() {
            Some(ignored_categories) => ignored_categories,
            None => return Ok(torrents),
        };

        let mut result = vec![];
        for torrent in torrents {
            let ignored = torrent
                .category
                .as_ref()
                .is_some_and(|category| ignored_categories.contains(category));

            if !ignored {
                result.push(torrent);
            }
        }
        Ok(result)
    }
}

struct SonarrFilter {
    sonarr_api: Arc<Option<SonarrAPI>>,
}

impl SonarrFilter {
    fn new(sonarr_api: Arc<Option<SonarrAPI>>) -> Self {
        Self { sonarr_api }
    }
}

#[async_trait]
impl TorrentFilter for SonarrFilter {
    fn name(&self) -> String {
        "SonarrFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let api = match self.sonarr_api.as_ref() {
            Some(api) => api,
            None => return Ok(torrents),
        };

        let queue_items = api.get_queue().await?;
        trace!("Sonarr Queue: {}", queue_items.len());

        // Ignore cleanup if the Sonarr has started recently
        if queue_items.len() == 0 {
            if let Some(start_time) = api
                .get_system_status()
                .await?
                .start_time
                .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
            {
                let mins = 2;
                if OffsetDateTime::now_utc() < start_time + Duration::from_secs(60 * mins) {
                    return Ok(Vec::new());
                }
            }
        }

        let queue_download_ids = queue_items
            .into_iter()
            .filter_map(|item| item.download_id.and_then(|id| id))
            .map(|id| id.to_lowercase())
            .collect::<HashSet<String>>();

        trace!(
            "torrents: {:?}",
            torrents
                .iter()
                .filter_map(|t| t.name.clone())
                .collect::<Vec<String>>()
        );

        trace!("Sonarr Download Ids: {:?}", queue_download_ids);

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            if let Some(hash) = &torrent.hash {
                for download_id in &queue_download_ids {
                    if download_id == &hash.to_lowercase() {
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
    radarr_api: Arc<Option<RadarrAPI>>,
}

impl RadarrFilter {
    fn new(radarr_api: Arc<Option<RadarrAPI>>) -> Self {
        Self { radarr_api }
    }
}

#[async_trait]
impl TorrentFilter for RadarrFilter {
    fn name(&self) -> String {
        "RadarrFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let api = match self.radarr_api.as_ref() {
            Some(api) => api,
            None => return Ok(torrents),
        };

        let queue_items = api.get_queue().await?;
        trace!("Radarr Queue: {}", queue_items.len());

        // Ignore cleanup if the Radarr has started recently
        if queue_items.len() == 0 {
            if let Some(start_time) = api
                .get_system_status()
                .await?
                .start_time
                .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
            {
                let mins = 2;
                if start_time + Duration::from_secs(60 * mins) > OffsetDateTime::now_utc() {
                    return Ok(Vec::new());
                }
            }
        }

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
                        return false;
                    }
                }
            }
            return true;
        });

        Ok(torrents)
    }
}

pub struct CleanupController {
    cleanup_config: CleanupConfig,
    qbit_api: Arc<Qbit>,
    sonarr_api: Arc<Option<SonarrAPI>>,
    radarr_api: Arc<Option<RadarrAPI>>,
}

impl CleanupController {
    pub fn new(
        cleanup_config: CleanupConfig,
        qbittorrent_config: QBittorrentConfig,
        sonarr_config: Option<SonarrConfig>,
        radarr_config: Option<RadarrConfig>,
    ) -> Result<Self> {
        Ok(Self {
            cleanup_config,
            qbit_api: Arc::new(Qbit::new(
                qbittorrent_config.host,
                Credential::new(qbittorrent_config.username, qbittorrent_config.password),
            )),
            sonarr_api: Arc::new(sonarr_config.as_ref().and_then(|c| SonarrAPI::new(c).ok())),
            radarr_api: Arc::new(radarr_config.as_ref().and_then(|c| RadarrAPI::new(c).ok())),
        })
    }

    pub async fn execute(&mut self) -> Result<()> {
        let mut torrents = self
            .qbit_api
            .get_torrent_list(GetTorrentListArg::default())
            .await?;

        let mut filters: Vec<Box<dyn TorrentFilter>> = Vec::new();
        filters.push(Box::new(RatioFilter::new(
            self.cleanup_config.ratio.clone(),
        )));
        filters.push(Box::new(TrackerFilter::new(
            self.qbit_api.clone(),
            self.cleanup_config.ignored.clone().and_then(|i| i.trackers),
        )));
        filters.push(Box::new(CategoriesFilter::new(
            self.cleanup_config
                .ignored
                .clone()
                .and_then(|i| i.categories),
        )));
        filters.push(Box::new(SonarrFilter::new(self.sonarr_api.clone())));
        filters.push(Box::new(RadarrFilter::new(self.radarr_api.clone())));

        for mut filter in filters {
            debug!("Applying filter: {:?} ", filter);
            torrents = filter.filter(torrents).await?;
            debug!("Torrents after filter: {}", torrents.len());
        }

        if torrents.is_empty() {
            trace!("No torrents to delete");
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

        if self.cleanup_config.dry_run.unwrap_or(false) {
            info!("Dry run enabled, not deleting torrents");
            return Ok(());
        }

        match self
            .qbit_api
            .delete_torrents(torrent_hashes, Some(true))
            .await
        {
            Ok(_) => info!("Torrents deleted"),
            Err(_) => info!("Failed to delete torrents"),
        }

        Ok(())
    }
}
