use std::sync::Arc;

use anyhow::{anyhow, Result};
use sonarr::{
    apis::{
        configuration::{ApiKey, Configuration},
        queue_api::{ApiV3QueueBulkDeleteParams, ApiV3QueueGetParams},
        Api as _, ApiClient,
    },
    models::{QueueBulkResource, QueueResourcePagingResource},
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

    pub async fn get_queue(&self) -> Result<QueueResourcePagingResource> {
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
            .await?)
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
            queue_bulk_resource: Some(QueueBulkResource {
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
