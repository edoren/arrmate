use std::sync::Arc;

use anyhow::{Result, anyhow};
use sonarr::{
    apis::{
        Api as _, ApiClient,
        configuration::{ApiKey, Configuration},
        queue_api::{ApiV3QueueBulkDeleteParams, ApiV3QueueGetParams},
    },
    models::{
        SonarrHealthCheckResult, SonarrQueueBulkResource, SonarrQueueResource,
        SonarrQueueResourcePagingResource, SonarrSystemResource,
    },
};

use crate::config::SonarrConfig;

pub struct SonarrAPI {
    api: ApiClient,
}

impl SonarrAPI {
    pub fn new(app_config: &SonarrConfig) -> Result<Self> {
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

        Ok(SonarrAPI {
            api: ApiClient::new(Arc::new(config)),
        })
    }

    pub async fn get_system_status(&self) -> Result<SonarrSystemResource> {
        self.api
            .system_api()
            .api_v3_system_status_get()
            .await
            .map_err(|e| anyhow!("Could not retrieve system status: {e}"))
    }

    pub async fn get_queue(&self) -> Result<Vec<SonarrQueueResource>> {
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
        Ok(self
            .api
            .queue_api()
            .api_v3_queue_get(
                ApiV3QueueGetParams::builder()
                    .page_size(total_records)
                    .build(),
            )
            .await?
            .records
            .unwrap_or_default()
            .unwrap_or_default())
    }

    pub async fn queue_id_delete_bulk(
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
