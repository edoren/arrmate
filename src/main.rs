use std::time::Duration;

use anyhow::{Result, anyhow};
use log::{error, info, trace, warn};
use notify::{
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use tasks::{cleanup::CleanupController, retry::RetryController};
use tokio::fs;

mod apis;
mod config;
mod tasks;

use config::ConfigData;

async fn get_config() -> Result<ConfigData> {
    Ok(serde_yaml::from_str(
        match fs::read_to_string("config.yaml").await {
            Ok(data) => Ok(data),
            Err(_) => fs::read_to_string("config.yml").await,
        }
        .map_err(|e| anyhow!("Failed to read config file: {e}"))?
        .as_str(),
    )?)
}

async fn run() -> Result<()> {
    edolib::log::setup("arrmate").await?;

    let current_dir =
        std::env::current_dir().map_err(|e| anyhow!("Failed to get current directory: {e}"))?;

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
        return Err(anyhow!("No config file found (config.yaml or config.yml)"));
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

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

    let mut config_changed = true;

    let mut cleanup_controller: Option<CleanupController> = None;
    let mut retry_controller: Option<RetryController> = None;

    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        let mut run_controllers = false;

        tokio::select! {
            _ = interval.tick() => {
                run_controllers = true;
           },

            response = rx.recv() => {
                match response {
                    Some(event) => {
                        trace!("Received event: {:?}", event);
                        if let EventKind::Access(AccessKind::Close(AccessMode::Write)) = event.kind
                            && let Some(path) = event.paths.first()
                            && path == &config_path
                        {
                            info!("Config file changed, reloading...");
                            config_changed = true;
                        }
                    }
                    None => {}
                }
            }

            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
                break Ok(());
            }
        }

        if config_changed {
            let config: ConfigData = get_config()
                .await
                .map_err(|e| anyhow!("Failed to reload config: {e}"))?;

            cleanup_controller = {
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

            retry_controller = if let ConfigData {
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

            interval = tokio::time::interval(Duration::from_secs(config.refresh_interval));

            config_changed = false;
        } else if run_controllers {
            if let Some(cleanup_controller) = cleanup_controller.as_mut()
                && let Err(e) = cleanup_controller.execute().await
            {
                warn!("Cleanup task ignored due to error: {e}");
            }

            if let Some(retry_controller) = retry_controller.as_mut()
                && let Err(e) = retry_controller.execute().await
            {
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
