use anyhow::{Result, anyhow};
use async_trait::async_trait;
use qbit_rs::{
    Qbit,
    model::{Credential, GetTorrentListArg},
};

use crate::{
    apis::{
        QBittorrentAPIInterface,
        types::{Torrent, TorrentContent, Tracker},
    },
    config::QBittorrentConfig,
};
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
