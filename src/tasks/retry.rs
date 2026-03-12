use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::info;
use time::OffsetDateTime;

use crate::{
    apis::{
        SonarrAndRadarrAPIInterface,
        types::{
            QueueResource, QueueStatus, TrackedDownloadState, TrackedDownloadStatus,
            TrackedDownloadStatusMessage,
        },
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

    async fn process_queue(
        &mut self,
        api: &Arc<dyn SonarrAndRadarrAPIInterface>,
        items: Vec<QueueResource>,
    ) -> Result<()> {
        let mut ids_to_remove = Vec::new();
        let mut ids_to_remove_and_blocklist = Vec::new();

        for resource in items {
            let download_id = match resource.download_id.as_ref() {
                Some(val) => val,
                None => continue,
            };
            let current_time = OffsetDateTime::now_utc();

            let mut add_to_remove = false;
            let mut add_to_blocklist = false;

            if resource.status == QueueStatus::Warning {
                if resource.tracked_download_state == TrackedDownloadState::Downloading
                    && resource
                        .error_message
                        .as_ref()
                        .is_some_and(|val| val.contains("The download is stalled"))
                {
                    let current_sizeleft = resource.sizeleft;
                    let strike =
                        self.strikes
                            .entry(download_id.clone())
                            .or_insert(StrikeData::new(
                                0,
                                resource.sizeleft,
                                current_time - STALLED_INTERVAL,
                            ));

                    // TODO: add threshold for size difference
                    if current_time >= strike.last_check + STALLED_INTERVAL
                        && current_sizeleft >= strike.last_sizeleft
                    {
                        strike.num += 1;
                        strike.last_sizeleft = current_sizeleft;
                        strike.last_check = current_time;
                        info!(
                            "Torrent '{}' is stalled, strikes {}/{}",
                            resource.title.as_deref().unwrap_or("Unknown"),
                            strike.num,
                            MAX_NUM_STRIKES
                        );
                    }

                    if strike.num >= MAX_NUM_STRIKES {
                        self.strikes.remove(download_id);
                        add_to_remove = true;
                        add_to_blocklist = true;
                    }
                }

                if let Some(completion_time) = resource.added {
                    let timeout_datetime = completion_time + Duration::from_secs(3600);
                    let progress_size = resource.size - resource.sizeleft;
                    if OffsetDateTime::now_utc() > timeout_datetime && progress_size == 0 {
                        add_to_remove = true;
                        add_to_blocklist = true;
                    }
                }
            } else if let Some(strike) = self.strikes.get_mut(download_id) {
                strike.last_check = current_time;
            }

            if resource.status == QueueStatus::Completed
                && resource.tracked_download_status == TrackedDownloadStatus::Warning
                && resource.tracked_download_state == TrackedDownloadState::ImportPending
            {
                for TrackedDownloadStatusMessage { title: _, messages } in &resource.status_messages
                {
                    if messages.iter().any(|msg| {
                        BANNED_MESSAGES
                            .iter()
                            .any(|banned_msg| msg.contains(banned_msg))
                    }) {
                        add_to_remove = true;
                        add_to_blocklist = true;
                        break;
                    }
                }
            }

            if add_to_remove {
                if add_to_blocklist {
                    ids_to_remove_and_blocklist.push(resource);
                } else {
                    ids_to_remove.push(resource);
                }
            }
        }

        if !ids_to_remove.is_empty() {
            let removed: Vec<&String> = ids_to_remove
                .iter()
                .filter_map(|res| res.title.as_ref())
                .collect();
            if self.retry_config.dry_run.unwrap_or(false) {
                info!("Dry run enabled, not removing: {removed:?}");
            } else {
                info!("Following queue removed: {removed:?}");
                api.queue_bulk_delete(
                    ids_to_remove.into_iter().map(|res| res.id).collect(),
                    Some(true),
                    Some(false),
                    Some(false),
                    Some(false),
                )
                .await?;
            }
        }
        if !ids_to_remove_and_blocklist.is_empty() {
            let removed: Vec<&String> = ids_to_remove_and_blocklist
                .iter()
                .filter_map(|res| res.title.as_ref())
                .collect();
            if self.retry_config.dry_run.unwrap_or(false) {
                info!("Dry run enabled, not removing and blocking: {removed:?}");
            } else {
                info!("Following queue removed and blocked: {removed:?}");
                api.queue_bulk_delete(
                    ids_to_remove_and_blocklist
                        .into_iter()
                        .map(|res| res.id)
                        .collect(),
                    Some(true),
                    Some(true),
                    Some(false),
                    Some(false),
                )
                .await?;
            }
        }

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
}
