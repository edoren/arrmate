use anyhow::{Result, anyhow};
use async_trait::async_trait;
use qbit_rs::{
    Qbit,
    model::{Credential, GetTorrentListArg},
};

use crate::config::QBittorrentConfig;

pub type TorrentContent = qbit_rs::model::TorrentContent;
pub type Tracker = qbit_rs::model::Tracker;
pub type Torrent = qbit_rs::model::Torrent;

#[async_trait]
pub trait QBittorrentAPIInterface: Send + Sync {
    async fn get_torrent_list(&self) -> Result<Vec<Torrent>>;
    async fn get_torrent_contents(&self, hash: &str) -> Result<Vec<TorrentContent>>;
    async fn get_torrent_trackers(&self, hash: &str) -> Result<Vec<Tracker>>;
    async fn delete_torrents(&self, hashes: Vec<String>, delete_files: Option<bool>) -> Result<()>;
}

pub struct QBittorrentAPI {
    api: Qbit,
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
        self.api
            .get_torrent_list(GetTorrentListArg::default())
            .await
            .map_err(|e| anyhow!("{e}"))
    }

    async fn get_torrent_contents(&self, hash: &str) -> Result<Vec<TorrentContent>> {
        self.api
            .get_torrent_contents(hash, None)
            .await
            .map_err(|e| anyhow!("{e}"))
    }

    async fn get_torrent_trackers(&self, hash: &str) -> Result<Vec<Tracker>> {
        self.api
            .get_torrent_trackers(&hash)
            .await
            .map_err(|e| anyhow!("{e}"))
    }

    async fn delete_torrents(&self, hashes: Vec<String>, delete_files: Option<bool>) -> Result<()> {
        self.api
            .delete_torrents(hashes, delete_files)
            .await
            .map_err(|e| anyhow!("{e}"))
    }
}
