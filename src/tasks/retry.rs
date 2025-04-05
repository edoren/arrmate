use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use log::info;
use radarr::models::{
    RadarrQueueStatus, RadarrTrackedDownloadState, RadarrTrackedDownloadStatus,
    RadarrTrackedDownloadStatusMessage,
};
use sonarr::models::{
    SonarrQueueStatus, SonarrTrackedDownloadState, SonarrTrackedDownloadStatus,
    SonarrTrackedDownloadStatusMessage,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    apis::{radarr::RadarrAPI, sonarr::SonarrAPI},
    config::{RadarrConfig, RetryConfig, SonarrConfig},
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

const MAX_NUM_STRIKES: usize = 5;
const STALLED_INTERVAL: Duration = Duration::from_secs(60 * 5);

pub struct RetryController {
    retry_config: RetryConfig,
    sonarr_api: Arc<SonarrAPI>,
    radarr_api: Arc<RadarrAPI>,

    strikes: HashMap<String, StrikeData>,
}

impl RetryController {
    pub fn new(
        retry_config: RetryConfig,
        sonarr_config: &SonarrConfig,
        radarr_config: &RadarrConfig,
    ) -> Result<Self> {
        Ok(Self {
            retry_config,
            sonarr_api: Arc::new(SonarrAPI::new(&sonarr_config)?),
            radarr_api: Arc::new(RadarrAPI::new(&radarr_config)?),
            strikes: HashMap::new(),
        })
    }

    pub async fn execute(&mut self) -> Result<()> {
        let sonarr_queue_items = self.sonarr_api.get_queue().await?;
        let radarr_queue_items = self.radarr_api.get_queue().await?;

        let mut sonarr_ids_to_remove = Vec::new();
        let mut sonarr_ids_to_remove_and_blocklist = Vec::new();

        let mut radarr_ids_to_remove = Vec::new();
        let mut radarr_ids_to_remove_and_blocklist = Vec::new();

        for resource in sonarr_queue_items {
            if resource.id.is_none() {
                continue;
            }
            let download_id = match resource.download_id.as_ref().and_then(|val| val.as_deref()) {
                Some(val) => val.to_string(),
                None => continue,
            };
            let current_time = OffsetDateTime::now_utc();

            let mut add_to_remove = false;
            let mut add_to_blocklist = false;

            if resource.status == Some(SonarrQueueStatus::Warning) {
                if resource.tracked_download_state == Some(SonarrTrackedDownloadState::Downloading)
                    && resource.error_message.as_ref().is_some_and(|val| {
                        val.as_deref()
                            .is_some_and(|val| val.contains("The download is stalled"))
                    })
                {
                    let current_sizeleft = resource.sizeleft.unwrap_or(f64::MAX) as i64;
                    let strike = self.strikes.entry(download_id.clone()).or_insert(StrikeData::new(
                        0,
                        resource.sizeleft.unwrap_or(f64::MAX) as i64,
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
                            resource
                                .title
                                .as_ref()
                                .and_then(|v| v.as_deref())
                                .unwrap_or_default(),
                            strike.num,
                            MAX_NUM_STRIKES
                        );
                    }

                    if strike.num >= MAX_NUM_STRIKES {
                        self.strikes.remove(&download_id);
                        add_to_remove = true;
                        add_to_blocklist = true;
                    }
                }

                if let Some(completion_time) = resource
                    .added
                    .clone()
                    .unwrap_or_default()
                    .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
                {
                    let timeout_datetime = completion_time + Duration::from_secs(3600);
                    let progress_size =
                        resource.size.unwrap_or_default() - resource.sizeleft.unwrap_or_default();
                    if OffsetDateTime::now_utc() > timeout_datetime && progress_size == 0.0 {
                        add_to_remove = true;
                        add_to_blocklist = true;
                    }
                }
            } else {
                if let Some(strike) = self.strikes.get_mut(&download_id) {
                    strike.last_check = current_time;
                }
            }

            if resource.status == Some(SonarrQueueStatus::Completed)
                && resource.tracked_download_status == Some(SonarrTrackedDownloadStatus::Warning)
            {
                if resource.tracked_download_state
                    == Some(SonarrTrackedDownloadState::ImportPending)
                {
                    for SonarrTrackedDownloadStatusMessage { title: _, messages } in resource
                        .status_messages
                        .clone()
                        .unwrap_or_default()
                        .unwrap_or_default()
                    {
                        if messages
                            .unwrap_or_default()
                            .unwrap_or_default()
                            .iter()
                            .any(|msg| msg.contains("Found potentially dangerous file"))
                        {
                            add_to_remove = true;
                            add_to_blocklist = true;
                            break;
                        }
                    }
                }
            }

            if add_to_remove {
                if add_to_blocklist {
                    sonarr_ids_to_remove_and_blocklist.push(resource);
                } else {
                    sonarr_ids_to_remove.push(resource);
                }
            }
        }

        if !sonarr_ids_to_remove.is_empty() {
            let removed: Vec<&String> = sonarr_ids_to_remove
                .iter()
                .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
                .collect();
            info!("Following queue removed: {removed:?}");
            self.sonarr_api
                .queue_id_delete_bulk(
                    sonarr_ids_to_remove
                        .into_iter()
                        .filter_map(|res| res.id)
                        .collect(),
                    Some(true),
                    Some(false),
                    Some(false),
                    Some(false),
                )
                .await?;
        }
        if !sonarr_ids_to_remove_and_blocklist.is_empty() {
            let removed: Vec<&String> = sonarr_ids_to_remove_and_blocklist
                .iter()
                .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
                .collect();
            info!("Following queue removed and blocked: {removed:?}");
            self.sonarr_api
                .queue_id_delete_bulk(
                    sonarr_ids_to_remove_and_blocklist
                        .into_iter()
                        .filter_map(|res| res.id)
                        .collect(),
                    Some(true),
                    Some(true),
                    Some(false),
                    Some(false),
                )
                .await?;
        }

        for resource in radarr_queue_items {
            if resource.id.is_none() {
                continue;
            }
            let download_id = match resource.download_id.as_ref().and_then(|val| val.as_deref()) {
                Some(val) => val.to_string(),
                None => continue,
            };
            let current_time = OffsetDateTime::now_utc();

            let mut add_to_remove = false;
            let mut add_to_blocklist = false;

            if resource.status == Some(RadarrQueueStatus::Warning) {
                if resource.tracked_download_state == Some(RadarrTrackedDownloadState::Downloading)
                    && resource.error_message.as_ref().is_some_and(|val| {
                        val.as_deref()
                            .is_some_and(|val| val.contains("The download is stalled"))
                    })
                {
                    let current_sizeleft = resource.sizeleft.unwrap_or(f64::MAX) as i64;
                    let strike =
                        self.strikes
                            .entry(download_id.clone())
                            .or_insert(StrikeData::new(
                                0,
                                resource.sizeleft.unwrap_or(f64::MAX) as i64,
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
                            resource
                                .title
                                .as_ref()
                                .and_then(|v| v.as_deref())
                                .unwrap_or_default(),
                            strike.num,
                            MAX_NUM_STRIKES
                        );
                    }

                    if strike.num >= MAX_NUM_STRIKES {
                        self.strikes.remove(&download_id);
                        add_to_remove = true;
                        add_to_blocklist = true;
                    }
                }

                if let Some(completion_time) = resource
                    .added
                    .clone()
                    .unwrap_or_default()
                    .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
                {
                    let timeout_datetime = completion_time + Duration::from_secs(3600);
                    let progress_size =
                        resource.size.unwrap_or_default() - resource.sizeleft.unwrap_or_default();
                    if OffsetDateTime::now_utc() > timeout_datetime && progress_size == 0.0 {
                        add_to_remove = true;
                        add_to_blocklist = true;
                    }
                }
            } else {
                if let Some(strike) = self.strikes.get_mut(&download_id) {
                    strike.last_check = current_time;
                }
            }

            if resource.tracked_download_status == Some(RadarrTrackedDownloadStatus::Warning) {
                if resource.tracked_download_state
                    == Some(RadarrTrackedDownloadState::ImportPending)
                {
                    for RadarrTrackedDownloadStatusMessage { title: _, messages } in resource
                        .status_messages
                        .clone()
                        .unwrap_or_default()
                        .unwrap_or_default()
                    {
                        if messages
                            .unwrap_or_default()
                            .unwrap_or_default()
                            .iter()
                            .any(|msg| msg.contains("Found potentially dangerous file"))
                        {
                            add_to_remove = true;
                            add_to_blocklist = true;
                            break;
                        }
                    }
                }

                // if let Some(completion_time) = resource
                //     .estimated_completion_time
                //     .clone()
                //     .unwrap_or_default()
                //     .and_then(|date_str| OffsetDateTime::parse(&date_str, &Rfc3339).ok())
                // {
                //     let timeout_datetime = completion_time + Duration::from_secs(retry_config.timeout);
                //     if OffsetDateTime::now_utc() > timeout_datetime {
                //         add_to_remove = true;
                //     }
                // }
            }

            if add_to_remove {
                if add_to_blocklist {
                    radarr_ids_to_remove_and_blocklist.push(resource);
                } else {
                    radarr_ids_to_remove.push(resource);
                }
            }
        }

        if !radarr_ids_to_remove.is_empty() {
            let removed: Vec<&String> = radarr_ids_to_remove
                .iter()
                .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
                .collect();
            info!("Following queue removed: {removed:?}");
            self.radarr_api
                .queue_id_delete_bulk(
                    radarr_ids_to_remove
                        .into_iter()
                        .filter_map(|res| res.id)
                        .collect(),
                    Some(true),
                    Some(false),
                    Some(false),
                    Some(false),
                )
                .await?;
        }
        if !radarr_ids_to_remove_and_blocklist.is_empty() {
            let removed: Vec<&String> = radarr_ids_to_remove_and_blocklist
                .iter()
                .filter_map(|res| res.title.as_ref().and_then(|inner| inner.as_ref()))
                .collect();
            info!("Following queue removed and blocked: {removed:?}");
            self.radarr_api
                .queue_id_delete_bulk(
                    radarr_ids_to_remove_and_blocklist
                        .into_iter()
                        .filter_map(|res| res.id)
                        .collect(),
                    Some(true),
                    Some(true),
                    Some(false),
                    Some(false),
                )
                .await?;
        }

        Ok(())
    }
}
