use std::time::Duration;

use anyhow::{Result, anyhow};
use log::{error, info, warn};
use tasks::{cleanup::CleanupController, retry::RetryController};
use tokio::fs;

mod apis;
mod config;
mod tasks;

use config::ConfigData;

async fn run() -> Result<()> {
    edolib::log::setup("arrmate").await?;

    let config: ConfigData = serde_yaml::from_str(
        match fs::read_to_string("config.yaml").await {
            Ok(data) => Ok(data),
            Err(_) => fs::read_to_string("config.yml").await,
        }
        .map_err(|e| anyhow!("Failed to read config file: {e}"))?
        .as_str(),
    )?;

    let mut cleanup_controller = {
        let config = config.clone();
        let mut cleanup_config = config.cleanup;
        cleanup_config.dry_run = config.dry_run.or(cleanup_config.dry_run);
        CleanupController::new(
            cleanup_config,
            config.qbittorrent,
            config.sonarr,
            config.radarr,
        )
        .ok()
    };

    let mut retry_controller = if let ConfigData {
        retry: Some(mut retry_config),
        sonarr: Some(sonarr_config),
        radarr: Some(radarr_config),
        dry_run: main_dry_run,
        ..
    } = config
    {
        retry_config.dry_run = main_dry_run.or(retry_config.dry_run);
        RetryController::new(retry_config, &sonarr_config, &radarr_config).ok()
    } else {
        None
    };

    let mut interval = tokio::time::interval(Duration::from_secs(config.refresh_interval));
    loop {
        interval.tick().await;

        if let Some(cleanup_controller) = cleanup_controller.as_mut() {
            if let Err(e) = cleanup_controller.execute().await {
                warn!("Cleanup task ignored due to error: {e}");
            }
        }

        if let Some(retry_controller) = retry_controller.as_mut() {
            if let Err(e) = retry_controller.execute().await {
                warn!("Retry task ignored due to error: {e}");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let result = run().await;
    if let Err(e) = &result {
        error!("Application failed with error: {e}");
    } else {
        info!("Application closed successfully");
    }
    result
}
