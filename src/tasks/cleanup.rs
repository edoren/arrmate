use std::{
    collections::{HashMap, HashSet},
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
        qbittorrent::Torrent,
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

    async fn delete_torrents(&self, torrents: Vec<&Torrent>) -> Result<usize> {
        if torrents.is_empty() {
            return Ok(0);
        }

        if self.cleanup_config.dry_run.unwrap_or(false) {
            return Ok(0);
        }

        let torrents_size: usize = torrents.len();
        self.qbittorrent
            .delete_torrents(torrents, Some(true))
            .await?;

        Ok(torrents_size)
    }

    async fn process_with_filters(
        &self,
        torrents: Vec<Torrent>,
        mut filters: Vec<Box<dyn TorrentFilter>>,
    ) -> Result<HashMap<Torrent, TorrentFilterData>> {
        let mut processed_torrents = HashMap::new();

        for torrent in torrents {
            processed_torrents.insert(torrent, TorrentFilterData::pass());
        }

        for filter in &mut filters {
            debug!("Applying filter {}", filter.name());
            for (torrent, filter_data) in &mut processed_torrents {
                debug!("Evaluating torrent '{}'", torrent.name);
                if let Ok(data) = filter.filter(&torrent).await {
                    *filter_data += data;
                } else {
                    error!(
                        "Filter '{}' failed for torrent '{}', ignoring",
                        filter.name(),
                        torrent.name
                    );
                }
            }
        }

        return Ok(processed_torrents);
    }

    async fn run(&mut self) -> Result<()> {
        let torrents = self.qbittorrent.get_torrent_list().await?;

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

        let processed_torrents = self.process_with_filters(torrents, filters).await?;

        let mut torrents_to_delete = Vec::new();
        let mut torrents_ignored = HashMap::new();
        for (torrent, filter_data) in processed_torrents {
            if filter_data.ignored {
                torrents_ignored.insert(torrent, filter_data.messages);
            } else {
                torrents_to_delete.push(torrent);
            }
        }

        let deleted_count = self
            .delete_torrents(torrents_to_delete.iter().collect())
            .await
            .map_err(|e| anyhow!("Failed to delete torrents: {}", e))?;

        info!("Deleted {} torrents", deleted_count);

        info!("The following torrents were deleted:");
        for torrent in &torrents_to_delete {
            info!("- {}", torrent.name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apis::types::{
        QueueResource, QueueStatus, SystemStatus, TrackedDownloadState, TrackedDownloadStatus,
    };
    use qbit_rs::model::{Tracker, TrackerStatus};
    use time::OffsetDateTime;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_torrent(name: &str, hash: &str) -> Torrent {
        Torrent {
            name: name.to_string(),
            hash: hash.to_string(),
            total_size: 1_000_000,
            save_path: "/downloads".to_string(),
            category: String::new(),
            ratio: 0.0,
            seeding_time: Duration::from_secs(0),
            progress: 0.0,
            last_activity: None,
            trackers: vec![],
            contents: vec![],
        }
    }

    fn make_tracker_url(url: &str) -> Tracker {
        Tracker {
            url: url.to_string(),
            status: TrackerStatus::Working,
            tier: 0,
            num_peers: 0,
            num_seeds: 0,
            num_leeches: 0,
            num_downloaded: 0,
            msg: String::new(),
        }
    }

    fn make_queue_resource(download_id: Option<&str>) -> QueueResource {
        QueueResource {
            id: 1,
            added: None,
            size: 1_000_000,
            title: Some("title".to_string()),
            download_id: download_id.map(|s| s.to_string()),
            status: QueueStatus::Downloading,
            tracked_download_status: TrackedDownloadStatus::Ok,
            tracked_download_state: TrackedDownloadState::Downloading,
            status_messages: vec![],
            sizeleft: 0,
            error_message: None,
        }
    }

    fn make_tracker_config(
        domain: &str,
        ratio: Option<f64>,
        seeding_time: Option<Duration>,
        require_both: bool,
        ignore: Option<TrackerIgnore>,
    ) -> TrackerConfig {
        TrackerConfig {
            name: domain.to_string(),
            domain: vec![domain.to_string()],
            ratio,
            seeding_time,
            require_both,
            hard_links_percentage: 100,
            ignore,
        }
    }

    fn torrent_with_tracker(url: &str) -> Torrent {
        let mut t = make_torrent("t", "abc");
        t.trackers = vec![make_tracker_url(url)];
        t
    }

    fn make_controller(
        qbit: Arc<dyn QBittorrentAPIInterface>,
        dry_run: Option<bool>,
    ) -> CleanupController {
        CleanupController {
            cleanup_config: CleanupConfig {
                ratio: None,
                trackers: None,
                categories: None,
                dry_run,
            },
            qbittorrent: qbit,
            sonarr: None,
            radarr: None,
        }
    }

    fn make_run_controller(mock: Arc<MockQBitApi>, dry_run: Option<bool>) -> CleanupController {
        CleanupController {
            cleanup_config: CleanupConfig {
                ratio: None,
                trackers: None,
                categories: None,
                dry_run,
            },
            qbittorrent: mock,
            sonarr: None,
            radarr: None,
        }
    }

    fn make_content(name: &str, size: u64) -> qbit_rs::model::TorrentContent {
        qbit_rs::model::TorrentContent {
            index: 0,
            name: name.to_string(),
            size,
            progress: 1.0,
            priority: qbit_rs::model::Priority::Normal,
            is_seed: Some(true),
            piece_range: vec![],
            availability: 1.0,
        }
    }

    // ── mocks ────────────────────────────────────────────────────────────────

    struct AlwaysPassFilter;
    struct AlwaysIgnoreFilter;
    struct AlwaysErrFilter;

    #[async_trait]
    impl TorrentFilter for AlwaysPassFilter {
        fn name(&self) -> String {
            "AlwaysPassFilter".to_string()
        }
        async fn filter(&mut self, _torrent: &Torrent) -> Result<TorrentFilterData> {
            Ok(TorrentFilterData::pass())
        }
    }

    #[async_trait]
    impl TorrentFilter for AlwaysIgnoreFilter {
        fn name(&self) -> String {
            "AlwaysIgnoreFilter".to_string()
        }
        async fn filter(&mut self, torrent: &Torrent) -> Result<TorrentFilterData> {
            Ok(TorrentFilterData::ignored_single_message(format!(
                "ignoring '{}'",
                torrent.name
            )))
        }
    }

    #[async_trait]
    impl TorrentFilter for AlwaysErrFilter {
        fn name(&self) -> String {
            "AlwaysErrFilter".to_string()
        }
        async fn filter(&mut self, _torrent: &Torrent) -> Result<TorrentFilterData> {
            Err(anyhow::anyhow!("filter error"))
        }
    }

    struct MockArrApi {
        queue: Vec<QueueResource>,
        system_status: SystemStatus,
    }

    impl MockArrApi {
        fn with_queue(queue: Vec<QueueResource>) -> Self {
            Self {
                queue,
                system_status: SystemStatus {
                    start_time: OffsetDateTime::now_utc() - Duration::from_secs(3600),
                },
            }
        }

        fn started_recently(queue: Vec<QueueResource>) -> Self {
            Self {
                queue,
                system_status: SystemStatus {
                    start_time: OffsetDateTime::now_utc(),
                },
            }
        }
    }

    #[async_trait]
    impl SonarrAndRadarrAPIInterface for MockArrApi {
        async fn get_system_status(&self) -> Result<SystemStatus> {
            Ok(self.system_status.clone())
        }

        async fn get_queue(&self) -> Result<Vec<QueueResource>> {
            Ok(self.queue.clone())
        }

        async fn queue_bulk_delete(
            &self,
            _ids: Vec<i32>,
            _remove_from_client: Option<bool>,
            _blocklist: Option<bool>,
            _skip_redownload: Option<bool>,
            _change_category: Option<bool>,
        ) -> Result<()> {
            Ok(())
        }
    }

    struct MockQBitApi {
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
        delete_result: bool,
        torrent_list: Vec<Torrent>,
    }

    impl MockQBitApi {
        fn new() -> Self {
            Self {
                deleted: Arc::new(std::sync::Mutex::new(vec![])),
                delete_result: true,
                torrent_list: vec![],
            }
        }

        fn failing() -> Self {
            Self {
                deleted: Arc::new(std::sync::Mutex::new(vec![])),
                delete_result: false,
                torrent_list: vec![],
            }
        }

        fn with_torrents(torrents: Vec<Torrent>) -> Self {
            Self {
                deleted: Arc::new(std::sync::Mutex::new(vec![])),
                delete_result: true,
                torrent_list: torrents,
            }
        }

        fn deleted_hashes(&self) -> Vec<String> {
            self.deleted.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl QBittorrentAPIInterface for MockQBitApi {
        async fn get_torrent_list(&self) -> Result<Vec<Torrent>> {
            Ok(self.torrent_list.clone())
        }

        async fn delete_torrents(
            &self,
            torrents: Vec<&Torrent>,
            _delete_files: Option<bool>,
        ) -> Result<()> {
            let mut deleted = self.deleted.lock().unwrap();
            deleted.extend(torrents.iter().map(|t| t.hash.clone()));
            if self.delete_result {
                Ok(())
            } else {
                Err(anyhow::anyhow!("delete failed"))
            }
        }
    }

    struct MockArrApiCounted {
        queue: Vec<QueueResource>,
        system_status: SystemStatus,
        queue_calls: Arc<std::sync::Mutex<usize>>,
        status_calls: Arc<std::sync::Mutex<usize>>,
    }

    impl MockArrApiCounted {
        fn new(queue: Vec<QueueResource>, system_status: SystemStatus) -> Self {
            Self {
                queue,
                system_status,
                queue_calls: Arc::new(std::sync::Mutex::new(0)),
                status_calls: Arc::new(std::sync::Mutex::new(0)),
            }
        }

        fn queue_call_count(&self) -> usize {
            *self.queue_calls.lock().unwrap()
        }

        fn status_call_count(&self) -> usize {
            *self.status_calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl SonarrAndRadarrAPIInterface for MockArrApiCounted {
        async fn get_system_status(&self) -> Result<SystemStatus> {
            *self.status_calls.lock().unwrap() += 1;
            Ok(self.system_status.clone())
        }

        async fn get_queue(&self) -> Result<Vec<QueueResource>> {
            *self.queue_calls.lock().unwrap() += 1;
            Ok(self.queue.clone())
        }

        async fn queue_bulk_delete(
            &self,
            _ids: Vec<i32>,
            _remove_from_client: Option<bool>,
            _blocklist: Option<bool>,
            _skip_redownload: Option<bool>,
            _change_category: Option<bool>,
        ) -> Result<()> {
            Ok(())
        }
    }

    // ── CategoriesFilter ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn categories_filter_no_config_passes() {
        let mut f = CategoriesFilter::new(None);
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn categories_filter_matching_ignored_category_is_ignored() {
        let mut f = CategoriesFilter::new(Some(vec![CategoriesConfig {
            name: "movies".to_string(),
            ignore: true,
        }]));
        let mut t = make_torrent("t", "abc");
        t.category = "movies".to_string();
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
        assert!(!result.messages.is_empty());
    }

    #[tokio::test]
    async fn categories_filter_non_ignored_category_passes() {
        let mut f = CategoriesFilter::new(Some(vec![CategoriesConfig {
            name: "movies".to_string(),
            ignore: false,
        }]));
        let mut t = make_torrent("t", "abc");
        t.category = "movies".to_string();
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn categories_filter_non_matching_category_passes() {
        let mut f = CategoriesFilter::new(Some(vec![CategoriesConfig {
            name: "movies".to_string(),
            ignore: true,
        }]));
        let mut t = make_torrent("t", "abc");
        t.category = "tv".to_string();
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[test]
    fn categories_filter_name() {
        assert_eq!(CategoriesFilter::new(None).name(), "CategoriesFilter");
    }

    // ── TrackerFilter ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tracker_filter_no_config_passes() {
        let mut f = TrackerFilter::new(None, None);
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_global_ratio_not_met_is_ignored() {
        // Global ratio only checked when trackers are configured but none match
        let cfg = make_tracker_config(
            "other.example.com",
            None,
            None,
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(Some(2.0), Some(vec![cfg]));
        let mut t = make_torrent("t", "abc");
        t.ratio = 1.0;
        // No trackers on the torrent → configured_trackers is empty → global ratio applies
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_global_ratio_met_equal_passes() {
        let cfg = make_tracker_config(
            "other.example.com",
            None,
            None,
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(Some(2.0), Some(vec![cfg]));
        let mut t = make_torrent("t", "abc");
        t.ratio = 2.5;
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_global_ratio_met_higher_passes() {
        let cfg = make_tracker_config(
            "other.example.com",
            None,
            None,
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(Some(2.0), Some(vec![cfg]));
        let mut t = make_torrent("t", "abc");
        t.ratio = 2.5;
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_ignore_always_is_ignored() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            None,
            None,
            false,
            Some(TrackerIgnore::Always),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let t = torrent_with_tracker("https://tracker.example.com/announce");
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_ignore_never_passes() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            None,
            None,
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let t = torrent_with_tracker("https://tracker.example.com/announce");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_ratio_not_met_is_ignored() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            Some(2.0),
            None,
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.ratio = 1.0;
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_ratio_met_passes() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            Some(2.0),
            None,
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.ratio = 3.0;
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_seeding_time_not_met_is_ignored() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            None,
            Some(Duration::from_secs(3600)),
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.seeding_time = Duration::from_secs(60);
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_seeding_time_met_passes() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            None,
            Some(Duration::from_secs(3600)),
            false,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.seeding_time = Duration::from_secs(7200);
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_require_both_only_ratio_met_is_ignored() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            Some(2.0),
            Some(Duration::from_secs(3600)),
            true,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.ratio = 3.0;
        t.seeding_time = Duration::from_secs(60);
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_require_both_both_met_passes() {
        let cfg = make_tracker_config(
            "tracker.example.com",
            Some(2.0),
            Some(Duration::from_secs(3600)),
            true,
            Some(TrackerIgnore::Never),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.ratio = 3.0;
        t.seeding_time = Duration::from_secs(7200);
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_no_matching_tracker_passes() {
        let cfg = make_tracker_config(
            "other.example.com",
            None,
            None,
            false,
            Some(TrackerIgnore::Always),
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let t = torrent_with_tracker("https://tracker.example.com/announce");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_when_hard_linked_ignored_when_percentage_met() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orig = dir.path().join("movie.mkv");
        std::fs::write(&orig, b"data").expect("write");
        let link = dir.path().join("movie_link.mkv");
        std::fs::hard_link(&orig, &link).expect("hard_link");

        let save_path = dir.path().to_str().unwrap().to_string();
        let cfg = make_tracker_config(
            "tracker.example.com",
            None,
            None,
            false,
            None, // defaults to WhenHardLinked
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.progress = 1.0;
        t.save_path = save_path;
        t.contents = vec![make_content("movie.mkv", 4)];

        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_when_hard_linked_passes_when_no_hard_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("movie.mkv");
        std::fs::write(&file, b"data").expect("write");

        let save_path = dir.path().to_str().unwrap().to_string();
        let cfg = make_tracker_config(
            "tracker.example.com",
            None,
            None,
            false,
            None, // defaults to WhenHardLinked
        );
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.progress = 1.0;
        t.save_path = save_path;
        t.contents = vec![make_content("movie.mkv", 4)];

        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn tracker_filter_missing_content_file_skipped_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let save_path = dir.path().to_str().unwrap().to_string();
        let cfg = make_tracker_config("tracker.example.com", None, None, false, None);
        let mut f = TrackerFilter::new(None, Some(vec![cfg]));
        let mut t = torrent_with_tracker("https://tracker.example.com/announce");
        t.progress = 1.0;
        t.save_path = save_path;
        // file does not exist → metadata Err → continue; 0% hard-linked → not ignored
        t.contents = vec![make_content("nonexistent.mkv", 1000)];
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[test]
    fn tracker_filter_name() {
        assert_eq!(TrackerFilter::new(None, None).name(), "TrackerFilter");
    }

    // ── SonarrFilter ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sonarr_filter_no_api_passes() {
        let mut f = SonarrFilter::new(None);
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn sonarr_filter_torrent_in_queue_is_ignored() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> =
            Arc::new(MockArrApi::with_queue(vec![make_queue_resource(Some(
                "ABC123",
            ))]));
        let mut f = SonarrFilter::new(Some(api));
        let mut t = make_torrent("t", "ABC123");
        t.hash = "ABC123".to_string();
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn sonarr_filter_torrent_not_in_queue_passes() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> =
            Arc::new(MockArrApi::with_queue(vec![make_queue_resource(Some(
                "OTHER",
            ))]));
        let mut f = SonarrFilter::new(Some(api));
        let t = make_torrent("t", "ABC123");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn sonarr_filter_empty_queue_recently_started_bails() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> =
            Arc::new(MockArrApi::started_recently(vec![]));
        let mut f = SonarrFilter::new(Some(api));
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sonarr_filter_empty_queue_started_long_ago_passes() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> = Arc::new(MockArrApi::with_queue(vec![]));
        let mut f = SonarrFilter::new(Some(api));
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn sonarr_filter_queue_cached_on_second_call() {
        let api = Arc::new(MockArrApiCounted::new(
            vec![make_queue_resource(Some("ABC"))],
            SystemStatus {
                start_time: OffsetDateTime::now_utc() - Duration::from_secs(3600),
            },
        ));
        let api_dyn = api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>;
        let mut f = SonarrFilter::new(Some(api_dyn));
        let t = make_torrent("t", "xyz");
        let _ = f.filter(&t).await;
        let _ = f.filter(&t).await;
        assert_eq!(api.queue_call_count(), 1);
    }

    #[tokio::test]
    async fn sonarr_filter_system_status_cached_on_second_call() {
        // Empty queue triggers system_status fetch; started recently → Err
        let api = Arc::new(MockArrApiCounted::new(
            vec![],
            SystemStatus {
                start_time: OffsetDateTime::now_utc(),
            },
        ));
        let api_dyn = api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>;
        let mut f = SonarrFilter::new(Some(api_dyn));
        let t = make_torrent("t", "xyz");
        let _ = f.filter(&t).await; // fetches queue + status
        let _ = f.filter(&t).await; // both from cache
        assert_eq!(api.queue_call_count(), 1);
        assert_eq!(api.status_call_count(), 1);
    }

    #[test]
    fn sonarr_filter_name() {
        assert_eq!(SonarrFilter::new(None).name(), "SonarrFilter");
    }

    // These branches are structurally unreachable via filter() (which guards
    // sonarr.is_none() first), but reachable by calling the private methods
    // directly from within this module.
    #[tokio::test]
    async fn sonarr_filter_get_queue_no_api_bails() {
        let mut f = SonarrFilter {
            sonarr: None,
            cached_queue: None,
            cached_system_status: None,
        };
        assert!(f.get_queue().await.is_err());
    }

    #[tokio::test]
    async fn sonarr_filter_get_system_status_no_api_bails() {
        let mut f = SonarrFilter {
            sonarr: None,
            cached_queue: None,
            cached_system_status: None,
        };
        assert!(f.get_system_status().await.is_err());
    }

    // ── RadarrFilter ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn radarr_filter_no_api_passes() {
        let mut f = RadarrFilter::new(None);
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn radarr_filter_torrent_in_queue_is_ignored() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> =
            Arc::new(MockArrApi::with_queue(vec![make_queue_resource(Some(
                "ABC123",
            ))]));
        let mut f = RadarrFilter::new(Some(api));
        let mut t = make_torrent("t", "ABC123");
        t.hash = "ABC123".to_string();
        let result = f.filter(&t).await.unwrap();
        assert!(result.ignored);
    }

    #[tokio::test]
    async fn radarr_filter_torrent_not_in_queue_passes() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> =
            Arc::new(MockArrApi::with_queue(vec![make_queue_resource(Some(
                "OTHER",
            ))]));
        let mut f = RadarrFilter::new(Some(api));
        let t = make_torrent("t", "ABC123");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn radarr_filter_empty_queue_recently_started_bails() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> =
            Arc::new(MockArrApi::started_recently(vec![]));
        let mut f = RadarrFilter::new(Some(api));
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn radarr_filter_empty_queue_started_long_ago_passes() {
        let api: Arc<dyn SonarrAndRadarrAPIInterface> = Arc::new(MockArrApi::with_queue(vec![]));
        let mut f = RadarrFilter::new(Some(api));
        let t = make_torrent("t", "abc");
        let result = f.filter(&t).await.unwrap();
        assert!(!result.ignored);
    }

    #[tokio::test]
    async fn radarr_filter_queue_cached_on_second_call() {
        let api = Arc::new(MockArrApiCounted::new(
            vec![make_queue_resource(Some("ABC"))],
            SystemStatus {
                start_time: OffsetDateTime::now_utc() - Duration::from_secs(3600),
            },
        ));
        let api_dyn = api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>;
        let mut f = RadarrFilter::new(Some(api_dyn));
        let t = make_torrent("t", "xyz");
        let _ = f.filter(&t).await;
        let _ = f.filter(&t).await;
        assert_eq!(api.queue_call_count(), 1);
    }

    #[tokio::test]
    async fn radarr_filter_system_status_cached_on_second_call() {
        let api = Arc::new(MockArrApiCounted::new(
            vec![],
            SystemStatus {
                start_time: OffsetDateTime::now_utc(),
            },
        ));
        let api_dyn = api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>;
        let mut f = RadarrFilter::new(Some(api_dyn));
        let t = make_torrent("t", "xyz");
        let _ = f.filter(&t).await;
        let _ = f.filter(&t).await;
        assert_eq!(api.queue_call_count(), 1);
        assert_eq!(api.status_call_count(), 1);
    }

    #[test]
    fn radarr_filter_name() {
        assert_eq!(RadarrFilter::new(None).name(), "RadarrFilter");
    }

    // These branches are structurally unreachable via filter() (which guards
    // radarr.is_none() first), but reachable by calling the private methods
    // directly from within this module.
    #[tokio::test]
    async fn radarr_filter_get_queue_no_api_bails() {
        let mut f = RadarrFilter {
            radarr: None,
            cached_queue: None,
            cached_system_status: None,
        };
        assert!(f.get_queue().await.is_err());
    }

    #[tokio::test]
    async fn radarr_filter_get_system_status_no_api_bails() {
        let mut f = RadarrFilter {
            radarr: None,
            cached_queue: None,
            cached_system_status: None,
        };
        assert!(f.get_system_status().await.is_err());
    }

    // ── delete_torrents ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_torrents_empty_returns_zero_without_calling_api() {
        let mock = Arc::new(MockQBitApi::new());
        let ctrl = make_controller(mock.clone(), None);
        let count = ctrl.delete_torrents(vec![]).await.unwrap();
        assert_eq!(count, 0);
        assert!(mock.deleted_hashes().is_empty());
    }

    #[tokio::test]
    async fn delete_torrents_dry_run_returns_zero_without_calling_api() {
        let mock = Arc::new(MockQBitApi::new());
        let ctrl = make_controller(mock.clone(), Some(true));
        let t = make_torrent("t", "abc");
        let count = ctrl.delete_torrents(vec![&t]).await.unwrap();
        assert_eq!(count, 0);
        assert!(mock.deleted_hashes().is_empty());
    }

    #[tokio::test]
    async fn delete_torrents_calls_api_and_returns_count() {
        let mock = Arc::new(MockQBitApi::new());
        let ctrl = make_controller(mock.clone(), None);
        let t1 = make_torrent("a", "hash1");
        let t2 = make_torrent("b", "hash2");
        let count = ctrl.delete_torrents(vec![&t1, &t2]).await.unwrap();
        assert_eq!(count, 2);
        let deleted = mock.deleted_hashes();
        assert!(deleted.contains(&"hash1".to_string()));
        assert!(deleted.contains(&"hash2".to_string()));
    }

    #[tokio::test]
    async fn delete_torrents_api_error_propagates() {
        let mock = Arc::new(MockQBitApi::failing());
        let ctrl = make_controller(mock.clone(), None);
        let t = make_torrent("t", "abc");
        assert!(ctrl.delete_torrents(vec![&t]).await.is_err());
    }

    // ── process_with_filters ──────────────────────────────────────────────────

    #[tokio::test]
    async fn process_with_filters_no_filters_all_pass() {
        let ctrl = make_controller(Arc::new(MockQBitApi::new()), None);
        let torrents = vec![make_torrent("a", "h1"), make_torrent("b", "h2")];
        let result = ctrl.process_with_filters(torrents, vec![]).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.values().all(|d| !d.ignored));
    }

    #[tokio::test]
    async fn process_with_filters_ignore_filter_marks_all_ignored() {
        let ctrl = make_controller(Arc::new(MockQBitApi::new()), None);
        let torrents = vec![make_torrent("a", "h1"), make_torrent("b", "h2")];
        let filters: Vec<Box<dyn TorrentFilter>> = vec![Box::new(AlwaysIgnoreFilter)];
        let result = ctrl.process_with_filters(torrents, filters).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.values().all(|d| d.ignored));
    }

    #[tokio::test]
    async fn process_with_filters_pass_filter_leaves_none_ignored() {
        let ctrl = make_controller(Arc::new(MockQBitApi::new()), None);
        let torrents = vec![make_torrent("a", "h1")];
        let filters: Vec<Box<dyn TorrentFilter>> = vec![Box::new(AlwaysPassFilter)];
        let result = ctrl.process_with_filters(torrents, filters).await.unwrap();
        assert!(!result.values().next().unwrap().ignored);
    }

    #[tokio::test]
    async fn process_with_filters_failing_filter_does_not_ignore() {
        let ctrl = make_controller(Arc::new(MockQBitApi::new()), None);
        let torrents = vec![make_torrent("a", "h1")];
        let filters: Vec<Box<dyn TorrentFilter>> = vec![Box::new(AlwaysErrFilter)];
        let result = ctrl.process_with_filters(torrents, filters).await.unwrap();
        // Error from filter is swallowed; torrent should not be marked ignored
        assert!(!result.values().next().unwrap().ignored);
    }

    #[tokio::test]
    async fn process_with_filters_multiple_filters_combined() {
        let ctrl = make_controller(Arc::new(MockQBitApi::new()), None);
        let torrents = vec![make_torrent("a", "h1")];
        // Pass then ignore: final state should be ignored
        let filters: Vec<Box<dyn TorrentFilter>> =
            vec![Box::new(AlwaysPassFilter), Box::new(AlwaysIgnoreFilter)];
        let result = ctrl.process_with_filters(torrents, filters).await.unwrap();
        let data = result.values().next().unwrap();
        assert!(data.ignored);
        assert!(!data.messages.is_empty());
    }

    // ── CleanupController::new ────────────────────────────────────────────────

    #[test]
    fn cleanup_controller_new_with_qbit_succeeds() {
        let qbit: Arc<dyn QBittorrentAPIInterface> = Arc::new(MockQBitApi::new());
        let config = CleanupConfig {
            ratio: None,
            trackers: None,
            categories: None,
            dry_run: None,
        };
        assert!(CleanupController::new(config, Some(qbit), None, None).is_ok());
    }

    #[test]
    fn cleanup_controller_new_without_qbit_fails() {
        let config = CleanupConfig {
            ratio: None,
            trackers: None,
            categories: None,
            dry_run: None,
        };
        assert!(CleanupController::new(config, None, None, None).is_err());
    }

    // ── CleanupController::run ────────────────────────────────────────────────

    #[tokio::test]
    async fn run_no_torrents_nothing_deleted() {
        let mock = Arc::new(MockQBitApi::new());
        let mut ctrl = make_run_controller(mock.clone(), None);
        ctrl.run().await.unwrap();
        assert!(mock.deleted_hashes().is_empty());
    }

    #[tokio::test]
    async fn run_torrents_with_no_filters_deletes_all() {
        let mock = Arc::new(MockQBitApi::with_torrents(vec![
            make_torrent("a", "hash1"),
            make_torrent("b", "hash2"),
        ]));
        let mut ctrl = make_run_controller(mock.clone(), None);
        ctrl.run().await.unwrap();
        let deleted = mock.deleted_hashes();
        assert!(deleted.contains(&"hash1".to_string()));
        assert!(deleted.contains(&"hash2".to_string()));
    }

    #[tokio::test]
    async fn run_dry_run_deletes_nothing() {
        let mock = Arc::new(MockQBitApi::with_torrents(vec![
            make_torrent("a", "hash1"),
            make_torrent("b", "hash2"),
        ]));
        let mut ctrl = make_run_controller(mock.clone(), Some(true));
        ctrl.run().await.unwrap();
        assert!(mock.deleted_hashes().is_empty());
    }

    #[tokio::test]
    async fn run_sonarr_filter_keeps_queued_torrent() {
        let sonarr_api: Arc<dyn SonarrAndRadarrAPIInterface> = Arc::new(MockArrApi::with_queue(
            vec![make_queue_resource(Some("hash1"))],
        ));
        let mock = Arc::new(MockQBitApi::with_torrents(vec![
            make_torrent("a", "hash1"), // in Sonarr queue → ignored
            make_torrent("b", "hash2"), // not in queue → deleted
        ]));
        let mut ctrl = CleanupController {
            cleanup_config: CleanupConfig {
                ratio: None,
                trackers: None,
                categories: None,
                dry_run: None,
            },
            qbittorrent: mock.clone(),
            sonarr: Some(sonarr_api),
            radarr: None,
        };
        ctrl.run().await.unwrap();
        let deleted = mock.deleted_hashes();
        assert!(!deleted.contains(&"hash1".to_string()));
        assert!(deleted.contains(&"hash2".to_string()));
    }

    #[tokio::test]
    async fn run_delete_failure_propagates_error() {
        let mock = Arc::new(MockQBitApi {
            deleted: Arc::new(std::sync::Mutex::new(vec![])),
            delete_result: false,
            torrent_list: vec![make_torrent("a", "hash1")],
        });
        let mut ctrl = make_run_controller(mock, None);
        assert!(ctrl.run().await.is_err());
    }

    // ── Task ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_execute_delegates_to_run() {
        let mock = Arc::new(MockQBitApi::with_torrents(vec![make_torrent("a", "h1")]));
        let mut ctrl = make_run_controller(mock.clone(), None);
        ctrl.execute().await.unwrap();
        assert!(mock.deleted_hashes().contains(&"h1".to_string()));
    }

    #[test]
    fn task_name_is_cleanup() {
        let ctrl = make_controller(Arc::new(MockQBitApi::new()), None);
        assert_eq!(ctrl.name(), "cleanup");
    }
}
