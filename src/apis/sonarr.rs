use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sonarr::{
    apis::{
        Api as _, ApiClient,
        configuration::{ApiKey, Configuration},
        queue_api::{ApiV3QueueBulkDeleteParams, ApiV3QueueGetParams},
    },
    models::{
        SonarrHealthCheckResult, SonarrQueueBulkResource, SonarrQueueResource, SonarrQueueStatus,
        SonarrSystemResource, SonarrTrackedDownloadState, SonarrTrackedDownloadStatus,
        SonarrTrackedDownloadStatusMessage,
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    apis::{
        QueueResource, QueueStatus, SonarrAndRadarrAPIInterface, SystemStatus,
        TrackedDownloadState, TrackedDownloadStatus, TrackedDownloadStatusMessage,
    },
    config::SonarrConfig,
};

impl From<SonarrTrackedDownloadStatusMessage> for TrackedDownloadStatusMessage {
    fn from(m: SonarrTrackedDownloadStatusMessage) -> Self {
        TrackedDownloadStatusMessage {
            title: m.title.flatten(),
            messages: m.messages.flatten().unwrap_or_default(),
        }
    }
}

impl From<SonarrQueueStatus> for QueueStatus {
    fn from(s: SonarrQueueStatus) -> Self {
        match s {
            SonarrQueueStatus::Unknown => QueueStatus::Unknown,
            SonarrQueueStatus::Queued => QueueStatus::Queued,
            SonarrQueueStatus::Paused => QueueStatus::Paused,
            SonarrQueueStatus::Downloading => QueueStatus::Downloading,
            SonarrQueueStatus::Completed => QueueStatus::Completed,
            SonarrQueueStatus::Failed => QueueStatus::Failed,
            SonarrQueueStatus::Warning => QueueStatus::Warning,
            SonarrQueueStatus::Delay => QueueStatus::Delay,
            SonarrQueueStatus::DownloadClientUnavailable => QueueStatus::DownloadClientUnavailable,
            SonarrQueueStatus::Fallback => QueueStatus::Fallback,
        }
    }
}

impl From<SonarrTrackedDownloadState> for TrackedDownloadState {
    fn from(s: SonarrTrackedDownloadState) -> Self {
        match s {
            SonarrTrackedDownloadState::Downloading => TrackedDownloadState::Downloading,
            SonarrTrackedDownloadState::ImportBlocked => TrackedDownloadState::ImportBlocked,
            SonarrTrackedDownloadState::ImportPending => TrackedDownloadState::ImportPending,
            SonarrTrackedDownloadState::Importing => TrackedDownloadState::Importing,
            SonarrTrackedDownloadState::Imported => TrackedDownloadState::Imported,
            SonarrTrackedDownloadState::FailedPending => TrackedDownloadState::FailedPending,
            SonarrTrackedDownloadState::Failed => TrackedDownloadState::Failed,
            SonarrTrackedDownloadState::Ignored => TrackedDownloadState::Ignored,
        }
    }
}

impl From<SonarrTrackedDownloadStatus> for TrackedDownloadStatus {
    fn from(s: SonarrTrackedDownloadStatus) -> Self {
        match s {
            SonarrTrackedDownloadStatus::Ok => TrackedDownloadStatus::Ok,
            SonarrTrackedDownloadStatus::Warning => TrackedDownloadStatus::Warning,
            SonarrTrackedDownloadStatus::Error => TrackedDownloadStatus::Error,
        }
    }
}

impl TryFrom<SonarrSystemResource> for SystemStatus {
    type Error = anyhow::Error;

    fn try_from(r: SonarrSystemResource) -> Result<Self> {
        Ok(SystemStatus {
            start_time: r.start_time.context("start_time not found").and_then(|t| {
                OffsetDateTime::parse(&t, &Rfc3339).context("start_time parsing failed")
            })?,
        })
    }
}

impl TryFrom<SonarrQueueResource> for QueueResource {
    type Error = anyhow::Error;

    fn try_from(r: SonarrQueueResource) -> Result<Self> {
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

pub struct SonarrAPI {
    api: ApiClient,
}

impl SonarrAPI {
    pub fn new(app_config: &SonarrConfig) -> Self {
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
        SonarrAPI {
            api: ApiClient::new(config.into()),
        }
    }
}

#[async_trait]
impl SonarrAndRadarrAPIInterface for SonarrAPI {
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
            if health_resource.r#type == Some(SonarrHealthCheckResult::Error)
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
            sonarr_queue_bulk_resource: Some(SonarrQueueBulkResource {
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
