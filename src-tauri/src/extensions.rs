use eyre::WrapErr;
use parking_lot::RwLock;
use tauri::{AppHandle, Manager, State};

use crate::{
    bili_client::BiliClient,
    config::Config,
    downloader::{download_manager::DownloadManager, download_progress::DownloadProgress},
    types::player_info::PlayerInfo,
};

pub trait EyreReportToMessage {
    fn to_message(&self) -> String;
}

impl EyreReportToMessage for eyre::Report {
    fn to_message(&self) -> String {
        format!("{self:?}")
    }
}

pub trait AppHandleExt {
    fn get_config(&self) -> State<'_, RwLock<Config>>;
    fn get_bili_client(&self) -> State<'_, BiliClient>;
    fn get_download_manager(&self) -> State<'_, DownloadManager>;
}

impl AppHandleExt for AppHandle {
    fn get_config(&self) -> State<'_, RwLock<Config>> {
        self.state::<RwLock<Config>>()
    }
    fn get_bili_client(&self) -> State<'_, BiliClient> {
        self.state::<BiliClient>()
    }
    fn get_download_manager(&self) -> State<'_, DownloadManager> {
        self.state::<DownloadManager>()
    }
}

pub trait GetOrInitPlayerInfo {
    async fn get_or_init<'a>(
        &'a mut self,
        app: &AppHandle,
        progress: &DownloadProgress,
    ) -> eyre::Result<&'a mut PlayerInfo>;
}

impl GetOrInitPlayerInfo for Option<PlayerInfo> {
    async fn get_or_init<'a>(
        &'a mut self,
        app: &AppHandle,
        progress: &DownloadProgress,
    ) -> eyre::Result<&'a mut PlayerInfo> {
        if let Some(info) = self {
            return Ok(info);
        }

        let bili_client = app.get_bili_client();
        let info = bili_client
            .get_player_info(progress.aid, progress.cid)
            .await
            .wrap_err("获取播放器信息失败")?;

        Ok(self.insert(info))
    }
}
