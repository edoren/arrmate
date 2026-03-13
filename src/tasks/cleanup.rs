use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    ops::AddAssign,
    os::linux::fs::MetadataExt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use log::{debug, error, info, trace};
use time::OffsetDateTime;
use tokio::fs;
use url::Url;

use crate::{
    apis::{
        QBittorrentAPIInterface, SonarrAndRadarrAPIInterface,
        types::{QueueResource, SystemStatus},
    },
    config::{CategoriesConfig, CleanupConfig, TrackerConfig, TrackerIgnore},
    tasks::Task,
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
    #[allow(unused)]
    total_size: i64,
    save_path: String,
    category: String,
    ratio: f64,
    seeding_time: Duration,
    progress: f64,
    #[allow(unused)]
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

struct TorrentFilterData {
    ignored: bool,
    messages: Vec<String>,
}

impl TorrentFilterData {
    fn pass() -> Self {
        Self {
            ignored: false,
            messages: Vec::new(),
        }
    }

    fn ignored(messages: Vec<String>) -> Self {
        if messages.is_empty() {
            Self::pass()
        } else {
            Self {
                ignored: true,
                messages,
            }
        }
    }

    fn ignored_single_message(message: String) -> Self {
        Self {
            ignored: true,
            messages: vec![message],
        }
    }
}

impl AddAssign for TorrentFilterData {
    fn add_assign(&mut self, rhs: Self) {
        self.ignored = self.ignored || rhs.ignored;
        self.messages.extend(rhs.messages);
    }
}

#[async_trait]
trait TorrentFilter: Send {
    fn name(&self) -> String;
    async fn filter(&mut self, torrent: &Torrent) -> Result<TorrentFilterData>;
}

impl Debug for dyn TorrentFilter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.name())
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

    async fn filter(&mut self, torrent: &Torrent) -> Result<TorrentFilterData> {
        let categories = match self.categories.as_ref() {
            Some(ignored_categories) => ignored_categories,
            None => return Ok(TorrentFilterData::pass()),
        };

        if categories
            .iter()
            .any(|category| category.ignore && category.name == torrent.category)
        {
            return Ok(TorrentFilterData::ignored_single_message(format!(
                "Ignoring torrent '{}' due to category '{}'",
                torrent.name, torrent.category
            )));
        } else {
            return Ok(TorrentFilterData::pass());
        }
    }
}

struct TrackerFilter {
    global_ratio: Option<f64>,
    trackers: Option<Vec<TrackerConfig>>,
}

impl TrackerFilter {
    fn new(global_ratio: Option<f64>, trackers: Option<Vec<TrackerConfig>>) -> Self {
        Self {
            global_ratio,
            trackers,
        }
    }
}

#[async_trait]
impl TorrentFilter for TrackerFilter {
    fn name(&self) -> String {
        "TrackerFilter".to_string()
    }

    async fn filter(&mut self, torrent: &Torrent) -> Result<TorrentFilterData> {
        let trackers = match self.trackers.as_ref() {
            Some(ignored_trackers) => ignored_trackers,
            None => return Ok(TorrentFilterData::pass()),
        };

        let mut ignored_reasons = vec![];

        let torrent_tracker_urls: Vec<Url> = torrent
            .trackers
            .iter()
            .filter_map(|t| Url::parse(&t.url).ok())
            .collect();

        let configured_trackers: Vec<&TrackerConfig> = trackers
            .iter()
            .filter(|tracker| {
                torrent_tracker_urls.iter().any(|url| {
                    url.domain()
                        .is_some_and(|v| tracker.domain.contains(&v.to_string()))
                })
            })
            .collect();

        if configured_trackers.is_empty()
            && let Some(global_ratio) = self.global_ratio
            && torrent.ratio < global_ratio
        {
            ignored_reasons.push(format!(
                    "Ignoring torrent '{}' due to ratio {:.2} not reaching minimum required global ratio {:.2}",
                    torrent.name, torrent.ratio, global_ratio
                ));
        }

        let percentage_multiple_linked =
            if torrent.progress == 1.0 && !configured_trackers.is_empty() {
                let mut progress_size = 0;
                let mut total_size = 0;
                for content in &torrent.contents {
                    let save_path = Path::new(&torrent.save_path).join(&content.name);
                    let is_video = VIDEO_EXTENSIONS
                        .iter()
                        .any(|ext| save_path.extension().map_or(false, |e| e == *ext));
                    if !is_video {
                        continue;
                    }
                    total_size += content.size;
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
                        progress_size += disk_size;
                    }
                }

                let result = (progress_size as f64 / total_size as f64) * 100.0;
                trace!(
                    "Torrent '{}' has {:.0}% multiple hard linked files",
                    torrent.name, result
                );
                Some(result)
            } else {
                None
            };

        for tracker in configured_trackers {
            match tracker
                .ignore
                .as_ref()
                .unwrap_or(&TrackerIgnore::WhenHardLinked)
            {
                TrackerIgnore::Always => {
                    ignored_reasons.push(format!(
                        "Ignoring torrent '{}' due to tracker '{}' with ignore enabled",
                        torrent.name, tracker.name
                    ));
                }
                TrackerIgnore::WhenHardLinked => {
                    if let Some(percentage) = percentage_multiple_linked
                        && percentage >= tracker.hard_links_percentage as f64
                    {
                        ignored_reasons.push(format!(
                                "Ignoring torrent '{}' due to tracker '{}' with {:.0}% multiple hard linked files",
                                torrent.name, tracker.name, percentage
                            ));
                    }
                }
                TrackerIgnore::Never => {}
            }

            let ratio_reached_opt = tracker.ratio.map(|ratio| torrent.ratio >= ratio);

            let seeding_time_reached_opt = tracker
                .seeding_time
                .map(|seeding_time| torrent.seeding_time >= seeding_time);

            if let Some(ratio_reached) = ratio_reached_opt
                && let Some(seeding_time_reached) = seeding_time_reached_opt
            {
                if tracker.require_both && (!ratio_reached || !seeding_time_reached) {
                    ignored_reasons.push(format!(
                        "Ignoring torrent '{}' due to ratio {:.2} or seeding time {} not reaching minimum required ratio {:.2} or time {} for tracker '{}'",
                        torrent.name,
                        torrent.ratio,
                        humantime::format_duration(torrent.seeding_time),
                        tracker.ratio.unwrap_or(0.0),
                        humantime::format_duration(
                            tracker.seeding_time.unwrap_or(Duration::from_secs(0))
                        ),
                        tracker.name
                    ));
                } else if !ratio_reached && !seeding_time_reached {
                    ignored_reasons.push(format!(
                        "Ignoring torrent '{}' due to ratio {:.2} and seeding time {} not reaching minimum required ratio {:.2} and time {} for tracker '{}'",
                        torrent.name,
                        torrent.ratio,
                        humantime::format_duration(torrent.seeding_time),
                        tracker.ratio.unwrap_or(0.0),
                        humantime::format_duration(
                            tracker.seeding_time.unwrap_or(Duration::from_secs(0))
                        ),
                        tracker.name
                    ));
                }
            } else if ratio_reached_opt.is_some_and(|ratio_reached| !ratio_reached) {
                ignored_reasons.push(format!(
                    "Ignoring torrent '{}' due to ratio {:.2} not reaching minimum required ratio {:.2} for tracker '{}'",
                    torrent.name,
                    torrent.ratio,
                    tracker.ratio.unwrap_or(0.0),
                    tracker.name
                ));
            } else if seeding_time_reached_opt
                .is_some_and(|seeding_time_reached| !seeding_time_reached)
            {
                ignored_reasons.push(format!(
                    "Ignoring torrent '{}' due to seeding time {} not reaching minimum required time {} for tracker '{}'",
                    torrent.name,
                    humantime::format_duration(torrent.seeding_time),
                    humantime::format_duration(
                        tracker.seeding_time.unwrap_or(Duration::from_secs(0))
                    ),
                    tracker.name
                ));
            }
        }

        Ok(TorrentFilterData::ignored(ignored_reasons))
    }
}

struct SonarrFilter {
    sonarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
    cached_queue: Option<(Instant, Vec<QueueResource>)>,
    cached_system_status: Option<(Instant, SystemStatus)>,
}

impl SonarrFilter {
    fn new(sonarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>) -> Self {
        Self {
            sonarr,
            cached_queue: None,
            cached_system_status: None,
        }
    }

    async fn get_queue(&mut self) -> Result<Vec<QueueResource>> {
        if let Some((fetched_at, ref queue)) = self.cached_queue {
            if fetched_at.elapsed() < Duration::from_secs(60) {
                return Ok(queue.clone());
            }
        }
        let queue = match &self.sonarr {
            Some(api) => api
                .get_queue()
                .await
                .context("Could not retrieve Sonarr queue")?,
            None => bail!("No Sonarr API available"),
        };
        self.cached_queue = Some((Instant::now(), queue.clone()));
        Ok(queue)
    }

    async fn get_system_status(&mut self) -> Result<SystemStatus> {
        if let Some((fetched_at, ref status)) = self.cached_system_status {
            if fetched_at.elapsed() < Duration::from_secs(60) {
                return Ok(status.clone());
            }
        }
        let status = match &self.sonarr {
            Some(api) => api
                .get_system_status()
                .await
                .context("Could not retrieve Sonarr system status")?,
            None => bail!("No Sonarr API available"),
        };
        self.cached_system_status = Some((Instant::now(), status.clone()));
        Ok(status)
    }
}

#[async_trait]
impl TorrentFilter for SonarrFilter {
    fn name(&self) -> String {
        "SonarrFilter".to_string()
    }

    async fn filter(&mut self, torrent: &Torrent) -> Result<TorrentFilterData> {
        if self.sonarr.is_none() {
            return Ok(TorrentFilterData::pass());
        }

        let queue_items = self.get_queue().await?;
        // trace!("Sonarr Queue: {}", queue_items.len());

        // Ignore cleanup if Sonarr has started recently
        if queue_items.is_empty() {
            let system_status = self.get_system_status().await?;
            let mins = 2;
            if OffsetDateTime::now_utc() < system_status.start_time + Duration::from_secs(60 * mins)
            {
                bail!("Skipping due to recent Sonarr startup");
            }
        }

        let queue_download_ids = queue_items
            .into_iter()
            .filter_map(|v| v.download_id)
            .map(|id| id.to_lowercase())
            .collect::<HashSet<String>>();

        // trace!(
        //     "Sonarr Torrents: {:?}",
        //     torrents.iter().map(|t| &t.name).collect::<Vec<&String>>()
        // );
        // trace!("Sonarr Download Ids: {:?}", queue_download_ids);

        if queue_download_ids.contains(&torrent.hash.to_lowercase()) {
            return Ok(TorrentFilterData::ignored_single_message(format!(
                "Ignoring torrent '{}' due to still present on Sonarr queue",
                torrent.name,
            )));
        }

        Ok(TorrentFilterData::pass())
    }
}

struct RadarrFilter {
    radarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
    cached_queue: Option<(Instant, Vec<QueueResource>)>,
    cached_system_status: Option<(Instant, SystemStatus)>,
}

impl RadarrFilter {
    fn new(radarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>) -> Self {
        Self {
            radarr,
            cached_queue: None,
            cached_system_status: None,
        }
    }

    async fn get_queue(&mut self) -> Result<Vec<QueueResource>> {
        if let Some((fetched_at, ref queue)) = self.cached_queue {
            if fetched_at.elapsed() < Duration::from_secs(60) {
                return Ok(queue.clone());
            }
        }
        let queue = match &self.radarr {
            Some(api) => api
                .get_queue()
                .await
                .context("Could not retrieve Radarr queue")?,
            None => bail!("No Radarr API available"),
        };
        self.cached_queue = Some((Instant::now(), queue.clone()));
        Ok(queue)
    }

    async fn get_system_status(&mut self) -> Result<SystemStatus> {
        if let Some((fetched_at, ref status)) = self.cached_system_status {
            if fetched_at.elapsed() < Duration::from_secs(60) {
                return Ok(status.clone());
            }
        }
        let status = match &self.radarr {
            Some(api) => api
                .get_system_status()
                .await
                .context("Could not retrieve Radarr system status")?,
            None => bail!("No Radarr API available"),
        };
        self.cached_system_status = Some((Instant::now(), status.clone()));
        Ok(status)
    }
}

#[async_trait]
impl TorrentFilter for RadarrFilter {
    fn name(&self) -> String {
        "RadarrFilter".to_string()
    }

    async fn filter(&mut self, torrent: &Torrent) -> Result<TorrentFilterData> {
        if self.radarr.is_none() {
            return Ok(TorrentFilterData::pass());
        }

        let queue_items = self.get_queue().await?;

        // Ignore cleanup if Radarr has started recently
        if queue_items.is_empty() {
            let system_status = self.get_system_status().await?;
            let mins = 2;
            if OffsetDateTime::now_utc() < system_status.start_time + Duration::from_secs(60 * mins)
            {
                bail!("Skipping due to recent Radarr startup");
            }
        }

        let queue_download_ids = queue_items
            .into_iter()
            .filter_map(|v| v.download_id)
            .map(|id| id.to_lowercase())
            .collect::<HashSet<String>>();

        if queue_download_ids.contains(&torrent.hash.to_lowercase()) {
            return Ok(TorrentFilterData::ignored_single_message(format!(
                "Ignoring torrent '{}' due to still present on Radarr queue",
                torrent.name,
            )));
        }

        Ok(TorrentFilterData::pass())
    }
}

pub struct CleanupController {
    cleanup_config: CleanupConfig,
    qbittorrent: Arc<dyn QBittorrentAPIInterface>,
    sonarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
    radarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
}

async fn process_torrent(
    qbit_api: &Arc<dyn QBittorrentAPIInterface>,
    torrent: qbit_rs::model::Torrent,
) -> Result<Torrent> {
    let name = torrent.name.context("Torrent missing name")?;
    let hash = torrent.hash.context("Torrent missing hash")?;
    let total_size = torrent.total_size.context("Torrent missing total_size")?;
    let save_path = torrent.save_path.context("Torrent missing save_path")?;
    let contents = qbit_api
        .get_torrent_contents(&hash)
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
        seeding_time: Duration::from_secs(
            torrent.seeding_time.unwrap_or(0).try_into().unwrap_or(0),
        ),
        progress: torrent.progress.unwrap_or(0.0),
        last_activity: torrent
            .last_activity
            .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok()),
        trackers,
        contents,
    })
}

async fn get_torrents(qbit_api: Arc<dyn QBittorrentAPIInterface>) -> Result<Vec<Torrent>> {
    let torrent_list = qbit_api.get_torrent_list().await?;

    let mut set = tokio::task::JoinSet::new();
    for torrent in torrent_list {
        let qbit_api = qbit_api.clone();
        set.spawn(async move { process_torrent(&qbit_api, torrent).await });
    }

    let mut results = Vec::new();
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(Ok(torrent)) => results.push(torrent),
            Ok(Err(e)) => error!("Failed to process torrent: {e}"),
            Err(e) => error!("Torrent processing task panicked: {e}"),
        }
    }
    Ok(results)
}

impl CleanupController {
    pub fn new(
        cleanup_config: CleanupConfig,
        qbittorrent: Option<Arc<dyn QBittorrentAPIInterface>>,
        sonarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
        radarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
    ) -> Result<Self> {
        qbittorrent
            .map(|qbittorrent| Self {
                cleanup_config,
                qbittorrent,
                sonarr,
                radarr,
            })
            .context("Could not initialize cleanup task")
    }

    async fn run(&mut self) -> Result<()> {
        let torrents = get_torrents(self.qbittorrent.clone()).await?;

        let mut filters: Vec<Box<dyn TorrentFilter>> = Vec::new();
        filters.push(Box::new(CategoriesFilter::new(
            self.cleanup_config.categories.clone(),
        )));
        filters.push(Box::new(TrackerFilter::new(
            self.cleanup_config.ratio,
            self.cleanup_config.trackers.clone(),
        )));
        filters.push(Box::new(SonarrFilter::new(self.sonarr.clone())));
        filters.push(Box::new(RadarrFilter::new(self.radarr.clone())));

        let mut processed_torrents = HashMap::new();
        let mut torrents_to_delete = HashMap::new();
        let mut torrents_ignored = HashMap::new();
        for filter in &mut filters {
            debug!("Applying filter {}", filter.name());
            for torrent in &torrents {
                debug!("Evaluating torrent '{}'", torrent.name);
                let torrent_entry = processed_torrents
                    .entry(torrent)
                    .or_insert_with(TorrentFilterData::pass);
                if let Ok(data) = filter.filter(&torrent).await {
                    *torrent_entry += data;
                } else {
                    error!(
                        "Filter '{}' failed for torrent '{}', ignoring filter result: {}",
                        filter.name(),
                        torrent.name,
                        anyhow!("Filter error").to_string()
                    );
                }
            }
        }

        for (torrent, filter_data) in processed_torrents {
            if filter_data.ignored {
                torrents_ignored.insert(torrent, filter_data.messages);
            } else {
                torrents_to_delete.insert(torrent, filter_data.messages);
            }
        }

        if torrents_to_delete.is_empty() {
            trace!("No torrents to delete");
            return Ok(());
        }

        info!("The following torrents will be deleted:");
        for (torrent, _) in &torrents_to_delete {
            info!("- {}", torrent.name);
        }

        let torrent_hashes = torrents_to_delete
            .iter()
            .map(|(torrent, _)| torrent.hash.clone())
            .collect::<Vec<String>>();

        if self.cleanup_config.dry_run.unwrap_or(false) {
            info!("Dry run enabled, not deleting torrents");
            return Ok(());
        }

        match self
            .qbittorrent
            .delete_torrents(torrent_hashes, Some(true))
            .await
        {
            Ok(_) => info!("Torrents deleted"),
            Err(_) => info!("Failed to delete torrents"),
        }

        Ok(())
    }
}

#[async_trait]
impl Task for CleanupController {
    fn name(&self) -> &str {
        "cleanup"
    }

    async fn execute(&mut self) -> Result<()> {
        self.run().await
    }
}
