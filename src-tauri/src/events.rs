use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

use crate::downloader::{
    download_progress::DownloadProgress, download_task_state::DownloadTaskState,
};

#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    pub json_raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
#[serde(tag = "event", content = "data")]
pub enum DownloadEvent {
    Speed {
        speed: String,
    },

    TaskCreate {
        state: DownloadTaskState,
        progress: DownloadProgress,
    },

    TaskStateUpdate {
        task_id: String,
        state: DownloadTaskState,
    },

    TaskSleeping {
        task_id: String,
        remaining_sec: u64,
    },

    TaskDelete {
        task_id: String,
    },

    ProgressPreparing {
        task_id: String,
    },

    ProgressUpdate {
        progress: DownloadProgress,
    },
}
