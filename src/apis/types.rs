use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum QueueStatus {
    Unknown,
    Queued,
    Paused,
    Downloading,
    Completed,
    Failed,
    Warning,
    Delay,
    DownloadClientUnavailable,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TrackedDownloadState {
    Downloading,
    ImportBlocked,
    ImportPending,
    Importing,
    Imported,
    FailedPending,
    Failed,
    Ignored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TrackedDownloadStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Clone)]
pub struct TrackedDownloadStatusMessage {
    #[allow(unused)]
    pub title: Option<String>,
    pub messages: Vec<String>,
}

#[derive(Clone)]
pub struct SystemStatus {
    pub start_time: OffsetDateTime,
}

#[derive(Clone)]
pub struct QueueResource {
    pub id: i32,
    pub added: Option<OffsetDateTime>,
    pub size: i64,
    pub title: Option<String>,
    pub download_id: Option<String>,
    pub status: QueueStatus,
    pub tracked_download_status: TrackedDownloadStatus,
    pub tracked_download_state: TrackedDownloadState,
    pub status_messages: Vec<TrackedDownloadStatusMessage>,
    // #[deprecated]
    pub sizeleft: i64,
    pub error_message: Option<String>,
}
