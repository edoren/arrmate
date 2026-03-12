use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use log::{error, info, trace, warn};
use notify::{
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode},
};
use tasks::{Task, cleanup::CleanupController, retry::RetryController};
use tokio::fs;

mod apis;
mod config;
mod tasks;

use config::ConfigData;

use crate::apis::{
    QBittorrentAPIInterface, SonarrAndRadarrAPIInterface, qbittorrent::QBittorrentAPI,
    radarr::RadarrAPI, sonarr::SonarrAPI,
};

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

    let mut tasks: Vec<Box<dyn Task>> = Vec::new();

    let mut qbittorrent_api: Option<Arc<dyn QBittorrentAPIInterface>>;
    let mut sonarr_api: Option<Arc<dyn SonarrAndRadarrAPIInterface>>;
    let mut radarr_api: Option<Arc<dyn SonarrAndRadarrAPIInterface>>;

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

            qbittorrent_api = config.qbittorrent.map(|config| {
                Arc::new(QBittorrentAPI::new(&config)) as Arc<dyn QBittorrentAPIInterface>
            });
            sonarr_api = config.sonarr.map(|config| {
                Arc::new(SonarrAPI::new(&config)) as Arc<dyn SonarrAndRadarrAPIInterface>
            });
            radarr_api = config.radarr.map(|config| {
                Arc::new(RadarrAPI::new(&config)) as Arc<dyn SonarrAndRadarrAPIInterface>
            });

            if let Some(cleanup_config) = config.cleanup
                && let Ok(controller) = CleanupController::new(
                    cleanup_config,
                    qbittorrent_api.clone(),
                    sonarr_api.clone(),
                    radarr_api.clone(),
                )
            {
                tasks.push(Box::new(controller));
            }

            if let Some(retry_config) = config.retry
                && let Ok(controller) =
                    RetryController::new(retry_config, sonarr_api.clone(), radarr_api.clone())
            {
                tasks.push(Box::new(controller));
            }

            interval = tokio::time::interval(config.refresh_interval);

            config_changed = false;
        } else if run_controllers {
            for task in &mut tasks {
                if let Err(e) = task.execute().await {
                    warn!("{} task ignored due to error: {e}", task.name());
                }
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
