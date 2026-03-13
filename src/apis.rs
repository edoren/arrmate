use anyhow::Result;
use async_trait::async_trait;

pub mod qbittorrent;
pub mod radarr;
pub mod sonarr;
pub mod types;

#[async_trait]
pub trait QBittorrentAPIInterface: Send + Sync {
    async fn get_torrent_list(&self) -> Result<Vec<qbittorrent::Torrent>>;
    async fn delete_torrents(&self, torrents: Vec<&qbittorrent::Torrent>, delete_files: Option<bool>) -> Result<()>;
}

#[async_trait]
pub trait SonarrAndRadarrAPIInterface: Send + Sync {
    async fn get_system_status(&self) -> Result<types::SystemStatus>;
    async fn get_queue(&self) -> Result<Vec<types::QueueResource>>;
    async fn queue_bulk_delete(
        &self,
        ids: Vec<i32>,
        remove_from_client: Option<bool>,
        blocklist: Option<bool>,
        skip_redownload: Option<bool>,
        change_category: Option<bool>,
    ) -> Result<()>;
}
