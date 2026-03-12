use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use radarr::{
    apis::{
        Api, ApiClient,
        configuration::{ApiKey, Configuration},
        queue_api::{ApiV3QueueBulkDeleteParams, ApiV3QueueGetParams},
    },
    models::{
        RadarrHealthCheckResult, RadarrQueueBulkResource, RadarrQueueResource, RadarrQueueStatus,
        RadarrSystemResource, RadarrTrackedDownloadState, RadarrTrackedDownloadStatus,
        RadarrTrackedDownloadStatusMessage,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    apis::{
        SonarrAndRadarrAPIInterface,
        types::{
            QueueResource, QueueStatus, SystemStatus, TrackedDownloadState, TrackedDownloadStatus,
            TrackedDownloadStatusMessage,
        },
    },
    config::RadarrConfig,
};

impl From<RadarrTrackedDownloadStatusMessage> for TrackedDownloadStatusMessage {
    fn from(m: RadarrTrackedDownloadStatusMessage) -> Self {
        TrackedDownloadStatusMessage {
            title: m.title.flatten(),
            messages: m.messages.flatten().unwrap_or_default(),
        }
    }
}

impl From<RadarrTrackedDownloadStatus> for TrackedDownloadStatus {
    fn from(s: RadarrTrackedDownloadStatus) -> Self {
        match s {
            RadarrTrackedDownloadStatus::Ok => TrackedDownloadStatus::Ok,
            RadarrTrackedDownloadStatus::Warning => TrackedDownloadStatus::Warning,
            RadarrTrackedDownloadStatus::Error => TrackedDownloadStatus::Error,
        }
    }
}

impl From<RadarrQueueStatus> for QueueStatus {
    fn from(s: RadarrQueueStatus) -> Self {
        match s {
            RadarrQueueStatus::Unknown => QueueStatus::Unknown,
            RadarrQueueStatus::Queued => QueueStatus::Queued,
            RadarrQueueStatus::Paused => QueueStatus::Paused,
            RadarrQueueStatus::Downloading => QueueStatus::Downloading,
            RadarrQueueStatus::Completed => QueueStatus::Completed,
            RadarrQueueStatus::Failed => QueueStatus::Failed,
            RadarrQueueStatus::Warning => QueueStatus::Warning,
            RadarrQueueStatus::Delay => QueueStatus::Delay,
            RadarrQueueStatus::DownloadClientUnavailable => QueueStatus::DownloadClientUnavailable,
            RadarrQueueStatus::Fallback => QueueStatus::Fallback,
        }
    }
}

impl From<RadarrTrackedDownloadState> for TrackedDownloadState {
    fn from(s: RadarrTrackedDownloadState) -> Self {
        match s {
            RadarrTrackedDownloadState::Downloading => TrackedDownloadState::Downloading,
            RadarrTrackedDownloadState::ImportBlocked => TrackedDownloadState::ImportBlocked,
            RadarrTrackedDownloadState::ImportPending => TrackedDownloadState::ImportPending,
            RadarrTrackedDownloadState::Importing => TrackedDownloadState::Importing,
            RadarrTrackedDownloadState::Imported => TrackedDownloadState::Imported,
            RadarrTrackedDownloadState::FailedPending => TrackedDownloadState::FailedPending,
            RadarrTrackedDownloadState::Failed => TrackedDownloadState::Failed,
            RadarrTrackedDownloadState::Ignored => TrackedDownloadState::Ignored,
        }
    }
}

impl TryFrom<RadarrSystemResource> for SystemStatus {
    type Error = anyhow::Error;

    fn try_from(r: RadarrSystemResource) -> Result<Self> {
        Ok(SystemStatus {
            start_time: r.start_time.context("start_time not found").and_then(|t| {
                OffsetDateTime::parse(&t, &Rfc3339).context("start_time parsing failed")
            })?,
        })
    }
}

impl TryFrom<RadarrQueueResource> for QueueResource {
    type Error = anyhow::Error;

    fn try_from(r: RadarrQueueResource) -> Result<Self> {
        Ok(QueueResource {
            id: r.id.context("id not found")?,
            added: r
                .added
                .flatten()
                .map(|t| OffsetDateTime::parse(&t, &Rfc3339))
                .transpose()
                .context("added parsing failed")?,
            size: r.size.unwrap_or_default() as i64,
            title: r.title.flatten(),
            download_id: r.download_id.flatten(),
            status: r
                .status
                .map(QueueStatus::from)
                .context("status not found")?,
            tracked_download_status: r
                .tracked_download_status
                .map(TrackedDownloadStatus::from)
                .context("tracked_download_status not found")?,
            tracked_download_state: r
                .tracked_download_state
                .map(TrackedDownloadState::from)
                .context("tracked_download_state not found")?,
            sizeleft: r.sizeleft.context("sizeleft not found")? as i64,
            error_message: r.error_message.flatten(),
            status_messages: r
                .status_messages
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(TrackedDownloadStatusMessage::from)
                .collect(),
        })
    }
}

pub struct RadarrAPI {
    api: ApiClient,
}

impl RadarrAPI {
    pub fn new(app_config: &RadarrConfig) -> Self {
        let mut config = Configuration::default();
        config.base_path = app_config
            .host
            .to_string()
            .trim_end_matches(|c| c == '/')
            .to_string();
        config.api_key = Some(ApiKey {
            prefix: None,
            key: app_config.api_key.to_string(),
        });
        RadarrAPI {
            api: ApiClient::new(config.into()),
        }
    }
}

#[async_trait]
impl SonarrAndRadarrAPIInterface for RadarrAPI {
    async fn get_system_status(&self) -> Result<SystemStatus> {
        self.api
            .system_api()
            .api_v3_system_status_get()
            .await
            .map_err(|e| anyhow!("Could not retrieve system status: {e}"))
            .and_then(SystemStatus::try_from)
    }

    async fn get_queue(&self) -> Result<Vec<QueueResource>> {
        for health_resource in self.api.health_api().api_v3_health_get().await? {
            if health_resource.r#type == Some(RadarrHealthCheckResult::Error)
                && health_resource
                    .source
                    .is_some_and(|v| v.is_some_and(|s| s == "DownloadClientCheck"))
            {
                return Err(anyhow!("Health check failed for download client"));
            }
        }
        let total_records = self
            .api
            .queue_api()
            .api_v3_queue_get(ApiV3QueueGetParams::builder().page_size(0).build())
            .await?
            .total_records
            .ok_or(anyhow!("Error getting queue size"))?;
        self.api
            .queue_api()
            .api_v3_queue_get(
                ApiV3QueueGetParams::builder()
                    .page_size(total_records)
                    .build(),
            )
            .await?
            .records
            .unwrap_or_default()
            .unwrap_or_default()
            .into_iter()
            .map(QueueResource::try_from)
            .collect()
    }

    async fn queue_bulk_delete(
        &self,
        ids: Vec<i32>,
        remove_from_client: Option<bool>,
        blocklist: Option<bool>,
        skip_redownload: Option<bool>,
        change_category: Option<bool>,
    ) -> Result<()> {
        let params = ApiV3QueueBulkDeleteParams {
            remove_from_client,
            blocklist,
            skip_redownload,
            change_category,
            radarr_queue_bulk_resource: Some(RadarrQueueBulkResource {
                ids: Some(Some(ids)),
            }),
        };
        Ok(self
            .api
            .queue_api()
            .api_v3_queue_bulk_delete(params)
            .await?)
    }
}
