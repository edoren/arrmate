use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use log::error;
use qbit_rs::{
    Qbit,
    model::{Credential, GetTorrentListArg},
};
use time::OffsetDateTime;

use crate::{apis::QBittorrentAPIInterface, config::QBittorrentConfig};
pub struct QBittorrentAPI {
    api: Qbit,
}

#[derive(Clone, Debug)]
pub struct Torrent {
    pub name: String,
    pub hash: String,
    #[allow(unused)]
    pub total_size: i64,
    pub save_path: String,
    pub category: String,
    pub ratio: f64,
    pub seeding_time: Duration,
    pub progress: f64,
    #[allow(unused)]
    pub last_activity: Option<OffsetDateTime>,
    pub trackers: Vec<qbit_rs::model::Tracker>,
    pub contents: Vec<qbit_rs::model::TorrentContent>,
}

impl std::cmp::PartialEq for Torrent {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl std::cmp::Eq for Torrent {}

impl std::hash::Hash for Torrent {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

async fn process_torrent(api: Qbit, torrent: qbit_rs::model::Torrent) -> Result<Torrent> {
    let name = torrent.name.context("Torrent missing name")?;
    let hash = torrent.hash.context("Torrent missing hash")?;
    let total_size = torrent.total_size.context("Torrent missing total_size")?;
    let save_path = torrent.save_path.context("Torrent missing save_path")?;

    let contents = api
        .get_torrent_contents(&hash, None)
        .await
        .map_err(|e| anyhow!("Could not retrieve contents for torrent '{}': {e}", name))?;

    let trackers = api
        .get_torrent_trackers(&hash)
        .await
        .map_err(|e| anyhow!("Could not retrieve trackers for torrent '{}': {e}", name))?;

    Ok(Torrent {
        name,
        hash,
        total_size,
        save_path,
        category: torrent.category.unwrap_or_default(),
        ratio: torrent.ratio.unwrap_or(0.0),
        seeding_time: Duration::from_secs(
            torrent.seeding_time.unwrap_or(0).try_into().unwrap_or(0),
        ),
        progress: torrent.progress.unwrap_or(0.0),
        last_activity: torrent
            .last_activity
            .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok()),
        trackers,
        contents,
    })
}

async fn get_torrents(api: &Qbit) -> Result<Vec<Torrent>> {
    let torrent_list = api.get_torrent_list(GetTorrentListArg::default()).await?;

    let mut set = tokio::task::JoinSet::new();
    for torrent in torrent_list {
        let api = api.clone();
        set.spawn(async move { process_torrent(api, torrent).await });
    }

    let mut results = Vec::new();
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(Ok(torrent)) => results.push(torrent),
            Ok(Err(e)) => error!("Failed to process torrent: {e}"),
            Err(e) => error!("Torrent processing task panicked: {e}"),
        }
    }
    Ok(results)
}

impl QBittorrentAPI {
    pub fn new(config: &QBittorrentConfig) -> Self {
        QBittorrentAPI {
            api: Qbit::new(
                config.host.clone(),
                Credential::new(config.username.clone(), config.password.clone()),
            ),
        }
    }
}

#[async_trait]
impl QBittorrentAPIInterface for QBittorrentAPI {
    async fn get_torrent_list(&self) -> Result<Vec<Torrent>> {
        get_torrents(&self.api).await
    }

    async fn delete_torrents(
        &self,
        hashes: Vec<&Torrent>,
        delete_files: Option<bool>,
    ) -> Result<()> {
        let hash_values: Vec<String> = hashes.into_iter().map(|t| t.hash.clone()).collect();
        self.api
            .delete_torrents(hash_values, delete_files)
            .await
            .map_err(|e| anyhow!("{e}"))
    }
}
