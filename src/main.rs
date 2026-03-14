use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use log::{error, info, trace, warn};
use notify::{
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use tasks::{Task, cleanup::CleanupController, retry::RetryController};
use thiserror::Error;
use time::UtcOffset;
use tokio::fs;

mod apis;
mod config;
mod tasks;

use config::ConfigData;

use crate::apis::{
    QBittorrentAPIInterface, SonarrAndRadarrAPIInterface, qbittorrent::QBittorrentAPI,
    radarr::RadarrAPI, sonarr::SonarrAPI,
};

#[derive(Error, Debug, PartialEq, Clone)]
enum MainError {
    #[error("failed to parse config file: {0}")]
    ConfigFileParseError(String),

    #[error("failed to read config file: {0}")]
    ConfigFileReadingError(String),

    #[error("no config file found (config.yaml or config.yml)")]
    ConfigFileNotFound,

    #[error("failed to get current working directory")]
    CurrentWorkingDirectoryNotFound,
}

async fn get_config_file() -> Result<PathBuf, MainError> {
    let current_dir =
        std::env::current_dir().map_err(|_| MainError::CurrentWorkingDirectoryNotFound)?;

    let config_path = if fs::metadata("config.yaml")
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        current_dir.join("config.yaml")
    } else if fs::metadata("config.yml")
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        current_dir.join("config.yml")
    } else {
        return Err(MainError::ConfigFileNotFound);
    };

    return Ok(config_path);
}

async fn get_config() -> Result<ConfigData, MainError> {
    Ok(serde_yaml::from_str(
        fs::read_to_string(get_config_file().await?)
            .await
            .map_err(|e| MainError::ConfigFileReadingError(e.to_string()))?
            .as_str(),
    )
    .map_err(|e| MainError::ConfigFileParseError(e.to_string()))?)
}

async fn wait_terminate_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate_signal = signal(SignalKind::terminate())?;
        let mut quit_signal = signal(SignalKind::quit())?;
        let mut hangup_signal = signal(SignalKind::hangup())?;
        let mut interrupt_signal = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = terminate_signal.recv() => {}
            _ = quit_signal.recv() => {}
            _ = hangup_signal.recv() => {}
            _ = interrupt_signal.recv() => {}
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};

        let mut ctrl_c_signal = ctrl_c()?;
        let mut ctrl_break_signal = ctrl_break()?;
        let mut ctrl_close_signal = ctrl_close()?;
        let mut ctrl_logoff_signal = ctrl_logoff()?;
        let mut ctrl_shutdown_signal = ctrl_shutdown()?;

        tokio::select! {
            _ = ctrl_c_signal.recv() => {}
            _ = ctrl_break_signal.recv() => {}
            _ = ctrl_close_signal.recv() => {}
            _ = ctrl_logoff_signal.recv() => {}
            _ = ctrl_shutdown_signal.recv() => {}
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        tokio::signal::ctrl_c().await;
    }

    Ok(())
}

/// Snapshot the next scheduled date for every task that has one, relative to `from`.
fn snapshot_dates(
    tasks: &[Box<dyn Task>],
    from: time::OffsetDateTime,
) -> Vec<(usize, time::OffsetDateTime)> {
    tasks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.next_date(from).map(|d| (i, d)))
        .collect()
}

/// Return the indices of all tasks whose scheduled time is at or before `now`.
fn collect_due(
    scheduled: &[(usize, time::OffsetDateTime)],
    now: time::OffsetDateTime,
) -> Vec<usize> {
    scheduled
        .iter()
        .filter(|(_, when)| *when <= now)
        .map(|(idx, _)| *idx)
        .collect()
}

struct ArrMate {
    config: Option<ConfigData>,
    config_watcher: Option<RecommendedWatcher>,
    last_execution_time: time::OffsetDateTime,
    tasks: Vec<Box<dyn Task>>,
}

impl ArrMate {
    fn new() -> Self {
        ArrMate {
            config: None,
            config_watcher: None,
            last_execution_time: time::OffsetDateTime::now_utc(),
            tasks: Vec::new(),
        }
    }

    fn report_task_next_time(&self, task: &Box<dyn Task>) {
        if let Some(next) = task.next_date(self.last_execution_time) {
            info!(
                "Task '{}' is scheduled to execute at {}",
                task.name(),
                match UtcOffset::current_local_offset() {
                    Ok(offset) => next.to_offset(offset),
                    Err(_) => next,
                }
            );
        } else {
            info!(
                "Task '{}' has no schedule and will not be executed",
                task.name()
            );
        }
    }

    async fn reload_config(&mut self) {
        match get_config().await {
            Ok(config) => {
                self.config = Some(config);
                self.tasks = self.create_tasks().await;
                info!("Config loaded successfully");
                for task in &self.tasks {
                    self.report_task_next_time(task);
                }
            }
            Err(e) => {
                error!("Failed to load config: {e}");
            }
        }
    }

    async fn run(mut self) -> Result<()> {
        let (watcher_tx, mut watcher_rx) = tokio::sync::mpsc::channel(100);

        let config_path = get_config_file().await?;

        self.create_config_watcher(watcher_tx, config_path.clone())
            .await?;

        self.reload_config().await;

        loop {
            // Snapshot each task's next scheduled date now, before sleeping.
            // This ensures tasks with close/equal dates are all captured even
            // if their window passes while we are executing another task.
            let scheduled = snapshot_dates(&self.tasks, self.last_execution_time);
            let earliest = scheduled.iter().map(|(_, d)| *d).min();

            tokio::select! {
                _ = async {
                    if let Some(when) = earliest {
                        let delay = (when - time::OffsetDateTime::now_utc())
                            .try_into()
                            .unwrap_or_default();
                        tokio::time::sleep(delay).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    // Execute every task that is now due. Re-check after each
                    // batch so tasks that became due while others were running
                    // are caught within the same loop iteration.
                    self.last_execution_time = time::OffsetDateTime::now_utc();
                    let due = collect_due(&scheduled, self.last_execution_time);
                    for idx in due {
                        let task = &mut self.tasks[idx];
                        let name = task.name().to_owned();
                        info!("Executing task '{name}'");
                        if let Err(e) = task.execute().await {
                            warn!("{name} task ignored due to error: {e}");
                        }
                        self.report_task_next_time(&self.tasks[idx]);
                    }
                }

                response = watcher_rx.recv() => {
                    if let Some(event) = response {
                        trace!("Received event: {:?}", event);
                        if let EventKind::Access(AccessKind::Close(AccessMode::Write)) = event.kind
                            && let Some(path) = event.paths.first()
                            && path == &config_path
                        {
                            info!("Config file changed, reloading...");
                            self.config = None;
                            self.tasks.clear();
                            self.last_execution_time = time::OffsetDateTime::now_utc();
                            self.reload_config().await;
                        }
                    }
                }

                _ = wait_terminate_signal() => {
                    info!("Signal received, shutting down...");
                    break Ok(());
                }
            }
        }
    }

    async fn create_config_watcher(
        &mut self,
        tx: tokio::sync::mpsc::Sender<notify::Event>,
        config_path: PathBuf,
    ) -> Result<&RecommendedWatcher> {
        let mut watcher = RecommendedWatcher::new(
            move |res| match res {
                Ok(event) => {
                    if tx.blocking_send(event).is_err() {
                        error!("Config watcher stopped, receiver dropped");
                    }
                }
                Err(e) => error!("Config watcher error: {:?}", e),
            },
            notify::Config::default(),
        )?;

        watcher.watch(&config_path, RecursiveMode::NonRecursive)?;

        self.config_watcher = Some(watcher);

        Ok(self.config_watcher.as_ref().unwrap())
    }

    async fn create_tasks(&self) -> Vec<Box<dyn Task>> {
        let config = match &self.config {
            Some(config) => config,
            None => {
                return Vec::new();
            }
        };

        let mut tasks: Vec<Box<dyn Task>> = Vec::new();

        let qbittorrent_api = config.qbittorrent.as_ref().map(|config| {
            Arc::new(QBittorrentAPI::new(&config)) as Arc<dyn QBittorrentAPIInterface>
        });
        let sonarr_api = config.sonarr.as_ref().map(|config| {
            Arc::new(SonarrAPI::new(&config)) as Arc<dyn SonarrAndRadarrAPIInterface>
        });
        let radarr_api = config.radarr.as_ref().map(|config| {
            Arc::new(RadarrAPI::new(&config)) as Arc<dyn SonarrAndRadarrAPIInterface>
        });

        if let Some(cleanup_config) = config.cleanup.clone()
            && let Ok(controller) = CleanupController::new(
                cleanup_config,
                qbittorrent_api.clone(),
                sonarr_api.clone(),
                radarr_api.clone(),
            )
        {
            tasks.push(Box::new(controller));
        }

        if let Some(retry_config) = config.retry.clone()
            && let Ok(controller) =
                RetryController::new(retry_config, sonarr_api.clone(), radarr_api.clone())
        {
            tasks.push(Box::new(controller));
        }

        tasks
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    edolib::log::setup("arrmate").await?;

    let arrmate = ArrMate::new();

    let result = arrmate.run().await;

    if let Err(e) = &result {
        error!("Application failed with error: {e}");
    } else {
        info!("Application closed successfully");
    }

    result
}

#[cfg(test)]
mod tests {
    use config::{CleanupConfig, QBittorrentConfig, RadarrConfig, RetryConfig, SonarrConfig};
    use url::Url;

    use super::*;

    fn test_schedule() -> config::Schedule {
        "0 * * * * * *".parse().unwrap()
    }

    fn test_url() -> Url {
        "http://localhost".parse().unwrap()
    }

    // ── MainError display ─────────────────────────────────────────────────────

    #[test]
    fn main_error_config_parse_display() {
        let e = MainError::ConfigFileParseError("bad yaml".into());
        assert_eq!(e.to_string(), "failed to parse config file: bad yaml");
    }

    #[test]
    fn main_error_config_read_display() {
        let e = MainError::ConfigFileReadingError("no such file".into());
        assert_eq!(e.to_string(), "failed to read config file: no such file");
    }

    #[test]
    fn main_error_not_found_display() {
        assert_eq!(
            MainError::ConfigFileNotFound.to_string(),
            "no config file found (config.yaml or config.yml)"
        );
    }

    #[test]
    fn main_error_cwd_not_found_display() {
        assert_eq!(
            MainError::CurrentWorkingDirectoryNotFound.to_string(),
            "failed to get current working directory"
        );
    }

    // ── ArrMate::new ─────────────────────────────────────────────────────────

    #[test]
    fn arrmate_new_defaults() {
        let arrmate = ArrMate::new();
        assert!(arrmate.config.is_none());
        assert!(arrmate.config_watcher.is_none());
        assert!(arrmate.tasks.is_empty());
    }

    // ── snapshot_dates / collect_due ─────────────────────────────────────────

    struct MockTask {
        next: Option<time::OffsetDateTime>,
        executed: bool,
    }

    impl MockTask {
        fn due_in(delta: time::Duration) -> Self {
            Self {
                next: Some(time::OffsetDateTime::now_utc() + delta),
                executed: false,
            }
        }
        fn no_schedule() -> Self {
            Self {
                next: None,
                executed: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Task for MockTask {
        fn name(&self) -> &str {
            "mock"
        }
        async fn execute(&mut self) -> anyhow::Result<()> {
            self.executed = true;
            Ok(())
        }
        fn next_date(&self, _from: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
            self.next
        }
    }

    fn boxed(t: MockTask) -> Box<dyn Task> {
        Box::new(t)
    }

    #[test]
    fn snapshot_dates_skips_tasks_with_no_schedule() {
        let tasks: Vec<Box<dyn Task>> = vec![boxed(MockTask::no_schedule())];
        let now = time::OffsetDateTime::now_utc();
        assert!(snapshot_dates(&tasks, now).is_empty());
    }

    #[test]
    fn snapshot_dates_includes_tasks_with_schedule() {
        let tasks: Vec<Box<dyn Task>> = vec![boxed(MockTask::due_in(time::Duration::seconds(60)))];
        let now = time::OffsetDateTime::now_utc();
        let snap = snapshot_dates(&tasks, now);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, 0);
    }

    #[test]
    fn snapshot_dates_preserves_original_indices() {
        let tasks: Vec<Box<dyn Task>> = vec![
            boxed(MockTask::no_schedule()),
            boxed(MockTask::due_in(time::Duration::seconds(60))),
            boxed(MockTask::no_schedule()),
            boxed(MockTask::due_in(time::Duration::seconds(120))),
        ];
        let now = time::OffsetDateTime::now_utc();
        let snap = snapshot_dates(&tasks, now);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, 1);
        assert_eq!(snap[1].0, 3);
    }

    #[test]
    fn collect_due_returns_empty_when_all_in_future() {
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let scheduled = vec![(0usize, future)];
        let now = time::OffsetDateTime::now_utc();
        assert!(collect_due(&scheduled, now).is_empty());
    }

    #[test]
    fn collect_due_returns_past_tasks() {
        let past = time::OffsetDateTime::now_utc() - time::Duration::seconds(1);
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let scheduled = vec![(0usize, past), (1usize, future)];
        let now = time::OffsetDateTime::now_utc();
        let due = collect_due(&scheduled, now);
        assert_eq!(due, vec![0]);
    }

    #[test]
    fn collect_due_includes_tasks_at_exact_now() {
        let now = time::OffsetDateTime::now_utc();
        let scheduled = vec![(0usize, now)];
        let due = collect_due(&scheduled, now);
        assert_eq!(due, vec![0]);
    }

    #[test]
    fn collect_due_returns_all_overdue_tasks() {
        let now = time::OffsetDateTime::now_utc();
        let past1 = now - time::Duration::seconds(10);
        let past2 = now - time::Duration::seconds(1);
        let scheduled = vec![(0usize, past1), (1usize, past2)];
        let due = collect_due(&scheduled, now);
        assert_eq!(due, vec![0, 1]);
    }

    // ── create_tasks ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_tasks_no_config_returns_empty() {
        let arrmate = ArrMate::new();
        let tasks = arrmate.create_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn create_tasks_empty_config_returns_empty() {
        let mut arrmate = ArrMate::new();
        arrmate.config = Some(ConfigData {
            cleanup: None,
            retry: None,
            qbittorrent: None,
            sonarr: None,
            radarr: None,
        });
        let tasks = arrmate.create_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn create_tasks_cleanup_without_qbittorrent_returns_empty() {
        let mut arrmate = ArrMate::new();
        arrmate.config = Some(ConfigData {
            cleanup: Some(CleanupConfig {
                schedule: test_schedule(),
                ratio: None,
                trackers: None,
                categories: None,
                dry_run: None,
            }),
            retry: None,
            qbittorrent: None,
            sonarr: None,
            radarr: None,
        });
        let tasks = arrmate.create_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn create_tasks_cleanup_with_qbittorrent_creates_cleanup_task() {
        let mut arrmate = ArrMate::new();
        arrmate.config = Some(ConfigData {
            cleanup: Some(CleanupConfig {
                schedule: test_schedule(),
                ratio: None,
                trackers: None,
                categories: None,
                dry_run: None,
            }),
            retry: None,
            qbittorrent: Some(QBittorrentConfig {
                username: "user".into(),
                password: "pass".into(),
                host: test_url(),
            }),
            sonarr: None,
            radarr: None,
        });
        let tasks = arrmate.create_tasks().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name(), "cleanup");
    }

    #[tokio::test]
    async fn create_tasks_retry_without_arr_returns_empty() {
        let mut arrmate = ArrMate::new();
        arrmate.config = Some(ConfigData {
            cleanup: None,
            retry: Some(RetryConfig {
                schedule: test_schedule(),
                timeout: None,
                dry_run: None,
            }),
            qbittorrent: None,
            sonarr: Some(SonarrConfig {
                host: test_url(),
                api_key: "key".into(),
            }),
            radarr: None,
        });
        let tasks = arrmate.create_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn create_tasks_retry_with_sonarr_and_radarr_creates_retry_task() {
        let mut arrmate = ArrMate::new();
        arrmate.config = Some(ConfigData {
            cleanup: None,
            retry: Some(RetryConfig {
                schedule: test_schedule(),
                timeout: None,
                dry_run: None,
            }),
            qbittorrent: None,
            sonarr: Some(SonarrConfig {
                host: test_url(),
                api_key: "key".into(),
            }),
            radarr: Some(RadarrConfig {
                host: test_url(),
                api_key: "key".into(),
            }),
        });
        let tasks = arrmate.create_tasks().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name(), "retry");
    }

    #[tokio::test]
    async fn create_tasks_all_configs_creates_two_tasks() {
        let mut arrmate = ArrMate::new();
        arrmate.config = Some(ConfigData {
            cleanup: Some(CleanupConfig {
                schedule: test_schedule(),
                ratio: None,
                trackers: None,
                categories: None,
                dry_run: None,
            }),
            retry: Some(RetryConfig {
                schedule: test_schedule(),
                timeout: None,
                dry_run: None,
            }),
            qbittorrent: Some(QBittorrentConfig {
                username: "user".into(),
                password: "pass".into(),
                host: test_url(),
            }),
            sonarr: Some(SonarrConfig {
                host: test_url(),
                api_key: "key".into(),
            }),
            radarr: Some(RadarrConfig {
                host: test_url(),
                api_key: "key".into(),
            }),
        });
        let tasks = arrmate.create_tasks().await;
        assert_eq!(tasks.len(), 2);
    }
}
