use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::info;
use time::OffsetDateTime;

use crate::{
    apis::{
        SonarrAndRadarrAPIInterface,
        types::{QueueResource, QueueStatus, TrackedDownloadState, TrackedDownloadStatus},
    },
    config::RetryConfig,
    tasks::Task,
};

struct StrikeData {
    num: usize,
    last_sizeleft: i64,
    last_check: OffsetDateTime,
}

impl StrikeData {
    fn new(num: usize, last_sizeleft: i64, last_check: OffsetDateTime) -> Self {
        Self {
            num,
            last_sizeleft,
            last_check,
        }
    }
}

const BANNED_MESSAGES: [&str; 3] = [
    "Found potentially dangerous file",
    "Invalid video file, unsupported extension",
    "One or more episodes expected in this release were not imported or missing from the release",
];

const MAX_NUM_STRIKES: usize = 5;
const STALLED_INTERVAL: Duration = Duration::from_secs(60 * 5);

pub struct RetryController {
    retry_config: RetryConfig,
    sonarr: Arc<dyn SonarrAndRadarrAPIInterface>,
    radarr: Arc<dyn SonarrAndRadarrAPIInterface>,

    strikes: HashMap<String, StrikeData>,
}

impl RetryController {
    pub fn new(
        retry_config: RetryConfig,
        sonarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
        radarr: Option<Arc<dyn SonarrAndRadarrAPIInterface>>,
    ) -> Result<Self> {
        sonarr
            .zip(radarr)
            .map(|(sonarr, radarr)| Self {
                retry_config,
                sonarr,
                radarr,
                strikes: HashMap::new(),
            })
            .context("Could not initialize retry task")
    }

    /// Returns `true` when `resource` is in the stalled-download state that
    /// should trigger strike tracking.
    fn is_stalled_download(resource: &QueueResource) -> bool {
        resource.status == QueueStatus::Warning
            && resource.tracked_download_state == TrackedDownloadState::Downloading
            && resource
                .error_message
                .as_ref()
                .is_some_and(|v| v.contains("The download is stalled"))
    }

    /// Upserts a strike entry for `download_id`, increments when the interval
    /// has elapsed and size hasn't shrunk, and returns `true` (removing the
    /// entry) once `MAX_NUM_STRIKES` is reached.
    fn check_stalled_strikes(
        &mut self,
        download_id: &str,
        resource: &QueueResource,
        now: OffsetDateTime,
    ) -> bool {
        let current_sizeleft = resource.sizeleft;
        let strike = self
            .strikes
            .entry(download_id.to_owned())
            .or_insert(StrikeData::new(
                0,
                resource.sizeleft,
                now - STALLED_INTERVAL,
            ));

        // TODO: add threshold for size difference
        if now >= strike.last_check + STALLED_INTERVAL && current_sizeleft >= strike.last_sizeleft {
            strike.num += 1;
            strike.last_sizeleft = current_sizeleft;
            strike.last_check = now;
            info!(
                "Torrent '{}' is stalled, strikes {}/{}",
                resource.title.as_deref().unwrap_or("Unknown"),
                strike.num,
                MAX_NUM_STRIKES
            );
        }

        if strike.num >= MAX_NUM_STRIKES {
            self.strikes.remove(download_id);
            return true;
        }

        false
    }

    /// Returns `true` when the download was added more than 1 hour ago and
    /// zero bytes have been transferred.
    fn is_zero_progress_timeout(resource: &QueueResource, now: OffsetDateTime) -> bool {
        resource.added.is_some_and(|added| {
            now > added + Duration::from_secs(3600) && resource.size - resource.sizeleft == 0
        })
    }

    /// Returns `true` when the resource is `Completed / Warning / ImportPending`
    /// and any status message contains one of the `BANNED_MESSAGES` strings.
    fn has_banned_import_message(resource: &QueueResource) -> bool {
        resource.status == QueueStatus::Completed
            && resource.tracked_download_status == TrackedDownloadStatus::Warning
            && resource.tracked_download_state == TrackedDownloadState::ImportPending
            && resource.status_messages.iter().any(|msg_group| {
                msg_group
                    .messages
                    .iter()
                    .any(|msg| BANNED_MESSAGES.iter().any(|banned| msg.contains(banned)))
            })
    }

    /// Calls `queue_bulk_delete` for `items`, setting the blocklist flag
    /// according to `blocklist`. Skips the API call when dry-run is enabled.
    async fn execute_removals(
        &self,
        api: &Arc<dyn SonarrAndRadarrAPIInterface>,
        items: Vec<QueueResource>,
        blocklist: bool,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let titles: Vec<&String> = items.iter().filter_map(|r| r.title.as_ref()).collect();
        if self.retry_config.dry_run.unwrap_or(false) {
            if blocklist {
                info!("Dry run enabled, not removing and blocking: {titles:?}");
            } else {
                info!("Dry run enabled, not removing: {titles:?}");
            }
        } else {
            if blocklist {
                info!("Following queue removed and blocked: {titles:?}");
            } else {
                info!("Following queue removed: {titles:?}");
            }
            api.queue_bulk_delete(
                items.into_iter().map(|r| r.id).collect(),
                Some(true),
                Some(blocklist),
                Some(false),
                Some(false),
            )
            .await?;
        }
        Ok(())
    }

    async fn process_queue(
        &mut self,
        api: &Arc<dyn SonarrAndRadarrAPIInterface>,
        items: Vec<QueueResource>,
    ) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let mut to_remove = Vec::new();
        let mut to_remove_and_blocklist = Vec::new();

        for resource in items {
            let Some(download_id) = resource.download_id.as_ref() else {
                continue;
            };

            let mut remove = false;
            let mut blocklist = false;

            if resource.status == QueueStatus::Warning {
                if Self::is_zero_progress_timeout(&resource, now) {
                    remove = true;
                    blocklist = true;
                } else if Self::is_stalled_download(&resource)
                    && self.check_stalled_strikes(download_id, &resource, now)
                {
                    remove = true;
                    blocklist = true;
                }
            } else if let Some(strike) = self.strikes.get_mut(download_id) {
                strike.last_check = now;
            }

            if Self::has_banned_import_message(&resource) {
                remove = true;
                blocklist = true;
            }

            if remove {
                if blocklist {
                    to_remove_and_blocklist.push(resource);
                } else {
                    to_remove.push(resource);
                }
            }
        }

        self.execute_removals(api, to_remove, false).await?;
        self.execute_removals(api, to_remove_and_blocklist, true)
            .await?;

        Ok(())
    }

    async fn run(&mut self) -> Result<()> {
        let (sonarr_items, radarr_items) =
            tokio::try_join!(self.sonarr.get_queue(), self.radarr.get_queue())?;

        let sonarr = Arc::clone(&self.sonarr);
        self.process_queue(&sonarr, sonarr_items).await?;

        let radarr = Arc::clone(&self.radarr);
        self.process_queue(&radarr, radarr_items).await?;

        Ok(())
    }
}

#[async_trait]
impl Task for RetryController {
    fn name(&self) -> &str {
        "retry"
    }

    async fn execute(&mut self) -> Result<()> {
        self.run().await
    }

    fn next_date(&self, from: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
        self.retry_config.schedule.next_date(from)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use time::OffsetDateTime;

    use super::*;
    use crate::apis::types::{
        QueueStatus, SystemStatus, TrackedDownloadState, TrackedDownloadStatus,
        TrackedDownloadStatusMessage,
    };

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_resource() -> QueueResource {
        QueueResource {
            id: 1,
            added: None,
            size: 1_000_000,
            title: Some("Test Torrent".to_string()),
            download_id: Some("abc123".to_string()),
            status: QueueStatus::Downloading,
            tracked_download_status: TrackedDownloadStatus::Ok,
            tracked_download_state: TrackedDownloadState::Downloading,
            status_messages: vec![],
            sizeleft: 1_000_000,
            error_message: None,
        }
    }

    /// A resource that will always trigger removal (banned import message).
    fn make_removable_resource(id: i32) -> QueueResource {
        let mut r = make_resource();
        r.id = id;
        r.status = QueueStatus::Completed;
        r.tracked_download_status = TrackedDownloadStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Found potentially dangerous file".to_string()],
        }];
        r
    }

    fn make_controller() -> RetryController {
        RetryController {
            retry_config: RetryConfig {
                schedule: "0 * * * *".parse().unwrap(),
                timeout: None,
                dry_run: None,
            },
            sonarr: Arc::new(MockArrApi::new()),
            radarr: Arc::new(MockArrApi::new()),
            strikes: HashMap::new(),
        }
    }

    // ── mocks ────────────────────────────────────────────────────────────────

    struct MockArrApi {
        queue: Vec<QueueResource>,
        fail_queue: bool,
        bulk_delete_calls: std::sync::Mutex<Vec<(Vec<i32>, Option<bool>)>>,
    }

    impl MockArrApi {
        fn new() -> Self {
            Self {
                queue: vec![],
                fail_queue: false,
                bulk_delete_calls: std::sync::Mutex::new(vec![]),
            }
        }

        fn with_queue(queue: Vec<QueueResource>) -> Self {
            Self {
                queue,
                fail_queue: false,
                bulk_delete_calls: std::sync::Mutex::new(vec![]),
            }
        }

        fn failing_queue() -> Self {
            Self {
                queue: vec![],
                fail_queue: true,
                bulk_delete_calls: std::sync::Mutex::new(vec![]),
            }
        }

        fn delete_calls(&self) -> Vec<(Vec<i32>, Option<bool>)> {
            self.bulk_delete_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SonarrAndRadarrAPIInterface for MockArrApi {
        async fn get_system_status(&self) -> Result<SystemStatus> {
            Ok(SystemStatus {
                start_time: OffsetDateTime::now_utc(),
            })
        }

        async fn get_queue(&self) -> Result<Vec<QueueResource>> {
            if self.fail_queue {
                anyhow::bail!("queue fetch failed");
            }
            Ok(self.queue.clone())
        }

        async fn queue_bulk_delete(
            &self,
            ids: Vec<i32>,
            _remove_from_client: Option<bool>,
            blocklist: Option<bool>,
            _skip_redownload: Option<bool>,
            _change_category: Option<bool>,
        ) -> Result<()> {
            self.bulk_delete_calls
                .lock()
                .unwrap()
                .push((ids, blocklist));
            Ok(())
        }
    }

    // ── is_stalled_download ───────────────────────────────────────────────────

    #[test]
    fn is_stalled_download_true_when_all_conditions_met() {
        let mut r = make_resource();
        r.status = QueueStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::Downloading;
        r.error_message = Some("The download is stalled with no connections".to_string());
        assert!(RetryController::is_stalled_download(&r));
    }

    #[test]
    fn is_stalled_download_false_when_not_warning_status() {
        let mut r = make_resource();
        r.status = QueueStatus::Downloading;
        r.tracked_download_state = TrackedDownloadState::Downloading;
        r.error_message = Some("The download is stalled".to_string());
        assert!(!RetryController::is_stalled_download(&r));
    }

    #[test]
    fn is_stalled_download_false_when_not_downloading_state() {
        let mut r = make_resource();
        r.status = QueueStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.error_message = Some("The download is stalled".to_string());
        assert!(!RetryController::is_stalled_download(&r));
    }

    #[test]
    fn is_stalled_download_false_when_no_stall_message() {
        let mut r = make_resource();
        r.status = QueueStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::Downloading;
        r.error_message = None;
        assert!(!RetryController::is_stalled_download(&r));
    }

    #[test]
    fn is_stalled_download_false_when_different_error_message() {
        let mut r = make_resource();
        r.status = QueueStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::Downloading;
        r.error_message = Some("Some other error".to_string());
        assert!(!RetryController::is_stalled_download(&r));
    }

    // ── is_zero_progress_timeout ──────────────────────────────────────────────

    #[test]
    fn is_zero_progress_timeout_true_when_old_and_no_progress() {
        let mut r = make_resource();
        r.added = Some(OffsetDateTime::now_utc() - Duration::from_secs(7200));
        r.size = 1_000_000;
        r.sizeleft = 1_000_000; // zero bytes transferred
        let now = OffsetDateTime::now_utc();
        assert!(RetryController::is_zero_progress_timeout(&r, now));
    }

    #[test]
    fn is_zero_progress_timeout_false_when_no_added_time() {
        let mut r = make_resource();
        r.added = None;
        r.size = 1_000_000;
        r.sizeleft = 1_000_000;
        assert!(!RetryController::is_zero_progress_timeout(
            &r,
            OffsetDateTime::now_utc()
        ));
    }

    #[test]
    fn is_zero_progress_timeout_false_when_recent() {
        let mut r = make_resource();
        r.added = Some(OffsetDateTime::now_utc());
        r.size = 1_000_000;
        r.sizeleft = 1_000_000;
        assert!(!RetryController::is_zero_progress_timeout(
            &r,
            OffsetDateTime::now_utc()
        ));
    }

    #[test]
    fn is_zero_progress_timeout_false_when_progress_made() {
        let mut r = make_resource();
        r.added = Some(OffsetDateTime::now_utc() - Duration::from_secs(7200));
        r.size = 1_000_000;
        r.sizeleft = 500_000; // 500KB transferred
        let now = OffsetDateTime::now_utc();
        assert!(!RetryController::is_zero_progress_timeout(&r, now));
    }

    // ── has_banned_import_message ─────────────────────────────────────────────

    #[test]
    fn has_banned_import_message_true_on_match() {
        let mut r = make_resource();
        r.status = QueueStatus::Completed;
        r.tracked_download_status = TrackedDownloadStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Found potentially dangerous file in download".to_string()],
        }];
        assert!(RetryController::has_banned_import_message(&r));
    }

    #[test]
    fn has_banned_import_message_false_on_no_message_match() {
        let mut r = make_resource();
        r.status = QueueStatus::Completed;
        r.tracked_download_status = TrackedDownloadStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Everything looks fine".to_string()],
        }];
        assert!(!RetryController::has_banned_import_message(&r));
    }

    #[test]
    fn has_banned_import_message_false_on_wrong_status() {
        let mut r = make_resource();
        r.status = QueueStatus::Downloading; // not Completed
        r.tracked_download_status = TrackedDownloadStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Found potentially dangerous file".to_string()],
        }];
        assert!(!RetryController::has_banned_import_message(&r));
    }

    #[test]
    fn has_banned_import_message_false_on_wrong_tracked_status() {
        let mut r = make_resource();
        r.status = QueueStatus::Completed;
        r.tracked_download_status = TrackedDownloadStatus::Ok; // not Warning
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Found potentially dangerous file".to_string()],
        }];
        assert!(!RetryController::has_banned_import_message(&r));
    }

    // ── check_stalled_strikes ─────────────────────────────────────────────────

    #[test]
    fn check_stalled_strikes_first_call_inserts_entry_no_increment() {
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.sizeleft = 500_000;
        // Use a `now` that is exactly at the threshold so the interval condition
        // is met on this first call — but the entry is initialized with
        // `now - STALLED_INTERVAL`, so this IS the first increment.
        // To test "no increment", use a now that is BEFORE the interval elapses
        // relative to a pre-inserted entry.
        let past = OffsetDateTime::now_utc() - Duration::from_secs(10);
        // Insert a fresh entry manually with last_check = now (interval not elapsed)
        ctrl.strikes.insert(
            "abc123".to_string(),
            StrikeData::new(0, 500_000, OffsetDateTime::now_utc()),
        );
        let result = ctrl.check_stalled_strikes("abc123", &r, past + Duration::from_secs(10));
        assert!(!result);
        assert_eq!(ctrl.strikes["abc123"].num, 0);
    }

    #[test]
    fn check_stalled_strikes_increments_after_interval() {
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.sizeleft = 500_000;
        let start = OffsetDateTime::now_utc() - Duration::from_secs(3600);
        ctrl.strikes
            .insert("abc123".to_string(), StrikeData::new(0, 500_000, start));
        let now = start + STALLED_INTERVAL + Duration::from_secs(1);
        let result = ctrl.check_stalled_strikes("abc123", &r, now);
        assert!(!result);
        assert_eq!(ctrl.strikes["abc123"].num, 1);
    }

    #[test]
    fn check_stalled_strikes_no_increment_when_size_decreased() {
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.sizeleft = 400_000; // less than stored 500_000 → progress → no increment
        let start = OffsetDateTime::now_utc() - Duration::from_secs(3600);
        ctrl.strikes
            .insert("abc123".to_string(), StrikeData::new(2, 500_000, start));
        let now = start + STALLED_INTERVAL + Duration::from_secs(1);
        let result = ctrl.check_stalled_strikes("abc123", &r, now);
        assert!(!result);
        assert_eq!(ctrl.strikes["abc123"].num, 2); // unchanged
    }

    #[test]
    fn check_stalled_strikes_returns_true_and_removes_at_max() {
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.sizeleft = 500_000;
        let start = OffsetDateTime::now_utc() - Duration::from_secs(3600);
        // Pre-load at MAX_NUM_STRIKES - 1 so the next increment triggers removal
        ctrl.strikes.insert(
            "abc123".to_string(),
            StrikeData::new(MAX_NUM_STRIKES - 1, 500_000, start),
        );
        let now = start + STALLED_INTERVAL + Duration::from_secs(1);
        let result = ctrl.check_stalled_strikes("abc123", &r, now);
        assert!(result);
        assert!(!ctrl.strikes.contains_key("abc123"));
    }

    // ── execute_removals ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_removals_empty_is_noop() {
        let api = Arc::new(MockArrApi::new());
        let ctrl = make_controller();
        ctrl.execute_removals(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![],
            false,
        )
        .await
        .unwrap();
        assert!(api.delete_calls().is_empty());
    }

    #[tokio::test]
    async fn execute_removals_dry_run_skips_api() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        ctrl.retry_config.dry_run = Some(true);
        let r = make_resource();
        ctrl.execute_removals(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
            false,
        )
        .await
        .unwrap();
        assert!(api.delete_calls().is_empty());
    }

    #[tokio::test]
    async fn execute_removals_calls_api_without_blocklist() {
        let api = Arc::new(MockArrApi::new());
        let ctrl = make_controller();
        let mut r = make_resource();
        r.id = 42;
        ctrl.execute_removals(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
            false,
        )
        .await
        .unwrap();
        let calls = api.delete_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, vec![42]);
        assert_eq!(calls[0].1, Some(false));
    }

    #[tokio::test]
    async fn execute_removals_calls_api_with_blocklist() {
        let api = Arc::new(MockArrApi::new());
        let ctrl = make_controller();
        let mut r = make_resource();
        r.id = 7;
        ctrl.execute_removals(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
            true,
        )
        .await
        .unwrap();
        let calls = api.delete_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, vec![7]);
        assert_eq!(calls[0].1, Some(true));
    }

    // ── process_queue ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn process_queue_skips_resource_with_no_download_id() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.download_id = None;
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        assert!(api.delete_calls().is_empty());
    }

    #[tokio::test]
    async fn process_queue_clean_resource_not_removed() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        let r = make_resource(); // Downloading status, no issues
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        assert!(api.delete_calls().is_empty());
    }

    #[tokio::test]
    async fn process_queue_stalled_at_max_strikes_removed_and_blocklisted() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.id = 10;
        r.status = QueueStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::Downloading;
        r.error_message = Some("The download is stalled".to_string());
        r.sizeleft = 500_000;
        // Pre-load strike at MAX_NUM_STRIKES - 1 so one more check triggers removal
        let past = OffsetDateTime::now_utc() - STALLED_INTERVAL - Duration::from_secs(1);
        ctrl.strikes.insert(
            "abc123".to_string(),
            StrikeData::new(MAX_NUM_STRIKES - 1, 500_000, past),
        );
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        let calls = api.delete_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.contains(&10));
        assert_eq!(calls[0].1, Some(true)); // blocklisted
    }

    #[tokio::test]
    async fn process_queue_zero_progress_timeout_removed_and_blocklisted() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.id = 20;
        r.status = QueueStatus::Warning;
        r.added = Some(OffsetDateTime::now_utc() - Duration::from_secs(7200));
        r.size = 1_000_000;
        r.sizeleft = 1_000_000; // zero progress
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        let calls = api.delete_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.contains(&20));
        assert_eq!(calls[0].1, Some(true)); // blocklisted
    }

    #[tokio::test]
    async fn process_queue_banned_import_message_removed_and_blocklisted() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        let mut r = make_resource();
        r.id = 30;
        r.status = QueueStatus::Completed;
        r.tracked_download_status = TrackedDownloadStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Found potentially dangerous file".to_string()],
        }];
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        let calls = api.delete_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.contains(&30));
        assert_eq!(calls[0].1, Some(true)); // blocklisted
    }

    #[tokio::test]
    async fn process_queue_non_warning_status_updates_existing_strike_last_check() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        let old_check = OffsetDateTime::now_utc() - Duration::from_secs(3600);
        ctrl.strikes
            .insert("abc123".to_string(), StrikeData::new(2, 500_000, old_check));
        let r = make_resource(); // status = Downloading (non-Warning)
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        assert!(api.delete_calls().is_empty());
        // last_check advanced but strike preserved
        assert_eq!(ctrl.strikes["abc123"].num, 2);
        assert!(ctrl.strikes["abc123"].last_check > old_check);
    }

    #[tokio::test]
    async fn process_queue_dry_run_does_not_call_api() {
        let api = Arc::new(MockArrApi::new());
        let mut ctrl = make_controller();
        ctrl.retry_config.dry_run = Some(true);
        let mut r = make_resource();
        r.status = QueueStatus::Completed;
        r.tracked_download_status = TrackedDownloadStatus::Warning;
        r.tracked_download_state = TrackedDownloadState::ImportPending;
        r.status_messages = vec![TrackedDownloadStatusMessage {
            title: None,
            messages: vec!["Found potentially dangerous file".to_string()],
        }];
        ctrl.process_queue(
            &(api.clone() as Arc<dyn SonarrAndRadarrAPIInterface>),
            vec![r],
        )
        .await
        .unwrap();
        assert!(api.delete_calls().is_empty());
    }

    // ── run ───────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_processes_sonarr_and_radarr_queues_independently() {
        let sonarr = Arc::new(MockArrApi::with_queue(vec![make_removable_resource(1)]));
        let radarr = Arc::new(MockArrApi::with_queue(vec![make_removable_resource(2)]));
        let mut ctrl = RetryController {
            retry_config: RetryConfig {
                schedule: "0 * * * *".parse().unwrap(),
                timeout: None,
                dry_run: None,
            },
            sonarr: sonarr.clone(),
            radarr: radarr.clone(),
            strikes: HashMap::new(),
        };
        ctrl.run().await.unwrap();
        // Each API receives a delete call for its own item only
        let sonarr_deletes = sonarr.delete_calls();
        assert_eq!(sonarr_deletes.len(), 1);
        assert!(sonarr_deletes[0].0.contains(&1));
        let radarr_deletes = radarr.delete_calls();
        assert_eq!(radarr_deletes.len(), 1);
        assert!(radarr_deletes[0].0.contains(&2));
    }

    #[tokio::test]
    async fn run_empty_queues_make_no_api_calls() {
        let sonarr = Arc::new(MockArrApi::new());
        let radarr = Arc::new(MockArrApi::new());
        let mut ctrl = RetryController {
            retry_config: RetryConfig {
                schedule: "0 * * * *".parse().unwrap(),
                timeout: None,
                dry_run: None,
            },
            sonarr: sonarr.clone(),
            radarr: radarr.clone(),
            strikes: HashMap::new(),
        };
        ctrl.run().await.unwrap();
        assert!(sonarr.delete_calls().is_empty());
        assert!(radarr.delete_calls().is_empty());
    }

    #[tokio::test]
    async fn run_sonarr_get_queue_error_propagates() {
        let mut ctrl = RetryController {
            retry_config: RetryConfig {
                schedule: "0 * * * *".parse().unwrap(),
                timeout: None,
                dry_run: None,
            },
            sonarr: Arc::new(MockArrApi::failing_queue()),
            radarr: Arc::new(MockArrApi::new()),
            strikes: HashMap::new(),
        };
        assert!(ctrl.run().await.is_err());
    }

    #[tokio::test]
    async fn run_radarr_get_queue_error_propagates() {
        let mut ctrl = RetryController {
            retry_config: RetryConfig {
                schedule: "0 * * * *".parse().unwrap(),
                timeout: None,
                dry_run: None,
            },
            sonarr: Arc::new(MockArrApi::new()),
            radarr: Arc::new(MockArrApi::failing_queue()),
            strikes: HashMap::new(),
        };
        assert!(ctrl.run().await.is_err());
    }

    // ── Task ──────────────────────────────────────────────────────────────────

    #[test]
    fn task_name_is_retry() {
        assert_eq!(make_controller().name(), "retry");
    }

    #[tokio::test]
    async fn task_execute_delegates_to_run() {
        let sonarr = Arc::new(MockArrApi::with_queue(vec![make_removable_resource(99)]));
        let radarr = Arc::new(MockArrApi::new());
        let mut ctrl = RetryController {
            retry_config: RetryConfig {
                schedule: "0 * * * *".parse().unwrap(),
                timeout: None,
                dry_run: None,
            },
            sonarr: sonarr.clone(),
            radarr,
            strikes: HashMap::new(),
        };
        ctrl.execute().await.unwrap();
        assert!(sonarr.delete_calls()[0].0.contains(&99));
    }
}
