use std::{
    collections::HashSet, fmt::Debug, os::linux::fs::MetadataExt, path::Path, sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use log::{debug, error, info, trace};
use qbit_rs::{
    Qbit,
    model::{Credential, GetTorrentListArg},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::fs;
use url::Url;

use crate::{
    apis::{radarr::RadarrAPI, sonarr::SonarrAPI},
    config::{
        CategoriesConfig, CleanupConfig, QBittorrentConfig, RadarrConfig, SonarrConfig,
        TrackerConfig, TrackerIgnore,
    },
};

static VIDEO_EXTENSIONS: [&str; 38] = [
    "webm", "mkv", "flv", "vob", "ogv", "ogg", "rrc", "gifv", "mng", "mov", "avi", "qt", "wmv",
    "yuv", "rm", "asf", "amv", "mp4", "m4p", "m4v", "mpg", "mp2", "mpeg", "mpe", "mpv", "m4v",
    "svi", "3gp", "3g2", "mxf", "roq", "nsv", "flv", "f4v", "f4p", "f4a", "f4b", "mod",
];

#[derive(Clone, Debug)]
struct Torrent {
    name: String,
    hash: String,
    total_size: i64,
    save_path: String,
    category: String,
    ratio: f64,
    seeding_time: u64,
    progress: f64,
    last_activity: Option<OffsetDateTime>,
    trackers: Vec<qbit_rs::model::Tracker>,
    contents: Vec<qbit_rs::model::TorrentContent>,
}

impl std::cmp::PartialEq for Torrent {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl std::cmp::Eq for Torrent {}

impl std::hash::Hash for Torrent {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

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
    ratio: Option<f64>,
}

impl RatioFilter {
    fn new(ratio: Option<f64>) -> Self {
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
            let ignore: bool = torrent.ratio < *ratio;
            if ignore {
                debug!(
                    "Ignoring torrent '{}' due to ratio {:.2} not reaching minimum required global ratio {:.2}",
                    torrent.name, torrent.ratio, ratio
                );
            }
            !ignore
        });
        Ok(torrents)
    }
}

struct CategoriesFilter {
    categories: Option<Vec<CategoriesConfig>>,
}

impl CategoriesFilter {
    fn new(categories: Option<Vec<CategoriesConfig>>) -> Self {
        Self { categories }
    }
}

#[async_trait]
impl TorrentFilter for CategoriesFilter {
    fn name(&self) -> String {
        "CategoriesFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let categories = match self.categories.as_ref() {
            Some(ignored_categories) => ignored_categories,
            None => return Ok(torrents),
        };

        let mut result = vec![];
        for torrent in torrents {
            if categories.iter().any(|category| {
                category.ignore.unwrap_or(false) && category.name == torrent.category
            }) {
                debug!(
                    "Ignoring torrent '{}' due to category '{}'",
                    torrent.name, torrent.category
                );
            } else {
                result.push(torrent);
            }
        }
        Ok(result)
    }
}

struct TrackerFilter {
    trackers: Option<Vec<TrackerConfig>>,
}

impl TrackerFilter {
    fn new(trackers: Option<Vec<TrackerConfig>>) -> Self {
        Self { trackers }
    }
}

#[async_trait]
impl TorrentFilter for TrackerFilter {
    fn name(&self) -> String {
        "TrackerFilter".to_string()
    }

    async fn filter(&mut self, torrents: Vec<Torrent>) -> Result<Vec<Torrent>> {
        let trackers = match self.trackers.as_ref() {
            Some(ignored_trackers) => ignored_trackers,
            None => return Ok(torrents),
        };

        let mut result = vec![];
        for torrent in torrents {
            let torrent_tracker_urls = torrent
                .trackers
                .iter()
                .filter_map(|t| Url::parse(&t.url).ok())
                .collect::<Vec<_>>();

            let percentage_multiple_linked = if torrent.progress == 1.0 {
                let mut result = 0.0;
                for file in &torrent.contents {
                    let save_path = Path::new(&torrent.save_path).join(&file.name);
                    let metadata = match fs::metadata(&save_path).await {
                        Ok(meta) => meta,
                        Err(e) => {
                            debug!(
                                "Failed to get metadata for file '{}': {}",
                                save_path.to_str().unwrap_or("unknown"),
                                e
                            );
                            continue;
                        }
                    };
                    let num_links = metadata.st_nlink();
                    let disk_size = metadata.len();

                    if num_links > 1 {
                        result += (disk_size as f64 / torrent.total_size as f64) * 100.0;
                    }
                }

                trace!(
                    "Torrent '{}' has {:.0}% multiple hard linked files",
                    torrent.name, result
                );
                Some(result)
            } else {
                None
            };

            let mut ignored = false;

            for tracker in trackers {
                if !torrent_tracker_urls.iter().any(|url| {
                    url.domain()
                        .is_some_and(|v| tracker.domains.contains(&v.to_string()))
                }) {
                    continue;
                }

                if tracker.ignore.clone().unwrap_or_default() == TrackerIgnore::Always {
                    ignored = true;
                    debug!(
                        "Ignoring torrent '{}' due to tracker '{}' with ignore enabled",
                        torrent.name, tracker.name
                    );
                    break;
                }

                let ratio_reached = tracker.ratio.map(|ratio| torrent.ratio > ratio);

                let seeding_time_reached = tracker
                    .seeding_time
                    .map(|seeding_time| torrent.seeding_time > seeding_time);

                if tracker.require_ratio_and_seeding_time
                    && ratio_reached.is_some_and(|v| v)
                    && seeding_time_reached.is_some_and(|v| v)
                {
                    debug!(
                        "Ignoring torrent '{}' due to ratio {:.2} and seeding time {} not reaching minimum required ratio {:.2} and time {} for tracker '{}'",
                        torrent.name,
                        torrent.ratio,
                        torrent.seeding_time,
                        tracker.ratio.unwrap_or(0.0),
                        tracker.seeding_time.unwrap_or(0),
                        tracker.name
                    );
                    ignored = true;
                    break;
                } else if ratio_reached.is_some_and(|v| v) {
                    debug!(
                        "Ignoring torrent '{}' due to ratio {:.2} not reaching minimum required ratio {:.2} for tracker '{}'",
                        torrent.name,
                        torrent.ratio,
                        tracker.ratio.unwrap_or(0.0),
                        tracker.name
                    );
                    ignored = true;
                    break;
                } else if seeding_time_reached.is_some_and(|v| v) {
                    debug!(
                        "Ignoring torrent '{}' due to seeding time {:.2} not reaching minimum required time {:.2} for tracker '{}'",
                        torrent.name,
                        torrent.seeding_time,
                        tracker.seeding_time.unwrap_or(0),
                        tracker.name
                    );
                    ignored = true;
                    break;
                }

                if tracker.ignore.clone().unwrap_or_default() == TrackerIgnore::HardLinks
                    && let Some(percentage) = percentage_multiple_linked
                    && percentage >= tracker.hard_links_percentage as f64
                {
                    ignored = true;
                    debug!(
                        "Ignoring torrent '{}' due to tracker '{}' with {}% multiple hard linked files",
                        torrent.name, tracker.name, percentage
                    );
                    break;
                }
            }

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

        let queue_items = api
            .get_queue()
            .await
            .context("Could not retrieve Sonarr queue")?;
        trace!("Sonarr Queue: {}", queue_items.len());

        // Ignore cleanup if the Sonarr has started recently
        if queue_items.len() == 0
            && let Some(start_time) = api
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

        let queue_download_ids = queue_items
            .into_iter()
            .filter_map(|item| item.download_id.and_then(|id| id))
            .map(|id| id.to_lowercase())
            .collect::<HashSet<String>>();

        trace!(
            "Sonarr Torrents: {:?}",
            torrents.iter().map(|t| &t.name).collect::<Vec<&String>>()
        );

        trace!("Sonarr Download Ids: {:?}", queue_download_ids);

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            for download_id in &queue_download_ids {
                if download_id == &torrent.hash.to_lowercase() {
                    debug!(
                        "Ignoring torrent '{}' due to still present on Sonarr queue",
                        torrent.name,
                    );
                    return false;
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

        let queue_items = api
            .get_queue()
            .await
            .context("Could not retrieve Radarr queue")?;
        trace!("Radarr Queue: {}", queue_items.len());

        // Ignore cleanup if the Radarr has started recently
        if queue_items.len() == 0
            && let Some(start_time) = api
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

        let mut torrents = torrents;
        torrents.retain(|torrent| {
            for queue_item in &queue_items {
                let download_id = queue_item
                    .download_id
                    .clone()
                    .unwrap_or_default()
                    .unwrap_or_default()
                    .to_lowercase();

                if download_id == torrent.hash.to_lowercase() {
                    debug!(
                        "Ignoring torrent '{}' due to still present on Radarr queue",
                        torrent.name,
                    );
                    return false;
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

async fn process_torrent(qbit_api: &Qbit, torrent: qbit_rs::model::Torrent) -> Result<Torrent> {
    let name = torrent.name.context("Torrent missing name")?;
    let hash = torrent.hash.context("Torrent missing hash")?;
    let total_size = torrent.total_size.context("Torrent missing total_size")?;
    let save_path = torrent.save_path.context("Torrent missing save_path")?;
    let contents = qbit_api
        .get_torrent_contents(&hash, None)
        .await
        .map_err(|e| anyhow!("Could not retrieve contents for torrent '{}': {e}", name))?;
    let trackers = qbit_api
        .get_torrent_trackers(&hash)
        .await
        .map_err(|e| anyhow!("Could not retrieve trackers for torrent '{}': {e}", name))?;

    Ok(Torrent {
        name,
        hash,
        total_size,
        save_path,
        category: torrent.category.unwrap_or_default(),
        ratio: torrent.ratio.unwrap_or(0.0),
        seeding_time: torrent.seeding_time.unwrap_or(0).try_into().unwrap_or(0),
        progress: torrent.progress.unwrap_or(0.0),
        last_activity: torrent
            .last_activity
            .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok()),
        trackers: trackers,
        contents: contents,
    })
}

async fn get_torrents(qbit_api: &Qbit) -> Result<Vec<Torrent>> {
    let mut results = Vec::new();

    for torrent in qbit_api
        .get_torrent_list(GetTorrentListArg::default())
        .await?
    {
        match process_torrent(qbit_api, torrent.clone()).await {
            Ok(torrent) => {
                results.push(torrent);
            }
            Err(e) => {
                error!(
                    "Failed to process torrent: {}: {e}",
                    torrent.name.unwrap_or("unknown".into())
                );
                break;
            }
        }
    }

    Ok(results)
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
        let mut torrents = get_torrents(&self.qbit_api).await?;

        // let contents = {
        //     let mut contents = HashMap::new();
        //     for torrent in &torrents {
        //         if let Ok(files) = self
        //             .qbit_api
        //             .get_torrent_contents(&torrent.hash, None)
        //             .await
        //         {
        //             contents.insert(torrent.clone(), files);
        //         } else {
        //             debug!("Failed to get contents for torrent: {}", torrent.name);
        //         }
        //     }
        //     contents
        // };

        let mut filters: Vec<Box<dyn TorrentFilter>> = Vec::new();
        filters.push(Box::new(RatioFilter::new(
            self.cleanup_config.ratio.clone(),
        )));
        filters.push(Box::new(CategoriesFilter::new(
            self.cleanup_config.categories.clone(),
        )));
        filters.push(Box::new(TrackerFilter::new(
            self.cleanup_config.trackers.clone(),
        )));
        filters.push(Box::new(SonarrFilter::new(self.sonarr_api.clone())));
        filters.push(Box::new(RadarrFilter::new(self.radarr_api.clone())));

        for mut filter in filters {
            debug!("Applying {filter:?} ");
            torrents = filter.filter(torrents).await?;
            debug!("Torrents after {filter:?}: {}", torrents.len());
        }

        if torrents.is_empty() {
            trace!("No torrents to delete");
            return Ok(());
        }

        info!("The following torrents will be deleted:");
        for torrent in &torrents {
            info!("- {}", torrent.name);
        }

        let torrent_hashes = torrents
            .iter()
            .map(|t| t.hash.clone())
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
