use std::{sync::Arc, time::Duration};

use anyhow::Context;
use parking_lot::RwLock;
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::{
    sync::{SemaphorePermit, watch},
    time::sleep,
};

use crate::{
    events::DownloadEvent,
    extensions::{AnyhowErrorToStringChain, AppHandleExt},
    types::create_download_task_params::CreateDownloadTaskParams,
};

use super::{download_progress::DownloadProgress, download_task_state::DownloadTaskState};

pub struct DownloadTask {
    pub app: AppHandle,
    pub state_sender: watch::Sender<DownloadTaskState>,
    pub restart_sender: watch::Sender<()>,
    pub cancel_sender: watch::Sender<()>,
    pub delete_sender: watch::Sender<()>,
    pub task_id: String,
    pub progress: RwLock<DownloadProgress>,
}

impl DownloadTask {
    pub fn from_params(app: &AppHandle, params: &CreateDownloadTaskParams) -> Vec<Arc<Self>> {
        use CreateDownloadTaskParams::{Bangumi, Cheese, Normal};

        let mut progresses = Vec::new();
        match params {
            Normal(params) => {
                for &(aid, cid) in &params.aid_cid_pairs {
                    let progress = match DownloadProgress::from_normal(app, &params.info, aid, cid)
                    {
                        Ok(progress) => progress,
                        Err(err) => {
                            let cid = cid.map_or("None".to_string(), |id| id.to_string());
                            let ids_string = format!("aid: {aid}, cid: {cid}");
                            let err_title = format!("{ids_string} 创建普通视频的下载进度失败");
                            let string_chain = err.to_string_chain();
                            tracing::error!(err_title, message = string_chain);
                            continue;
                        }
                    };

                    progresses.extend(progress);
                }
            }
            Bangumi(params) => {
                for ep_id in &params.ep_ids {
                    let progress = match DownloadProgress::from_bangumi(app, &params.info, *ep_id) {
                        Ok(progress) => progress,
                        Err(err) => {
                            let ids_string = format!("ep_id: {ep_id}");
                            let err_title = format!("{ids_string} 创建番剧的下载进度失败");
                            let string_chain = err.to_string_chain();
                            tracing::error!(err_title, message = string_chain);
                            continue;
                        }
                    };

                    progresses.push(progress);
                }
            }
            Cheese(params) => {
                for ep_id in &params.ep_ids {
                    let progress = match DownloadProgress::from_cheese(app, &params.info, *ep_id) {
                        Ok(progress) => progress,
                        Err(err) => {
                            let ids_string = format!("ep_id: {ep_id}");
                            let err_title = format!("{ids_string} 创建课程的下载进度失败");
                            let string_chain = err.to_string_chain();
                            tracing::error!(err_title, message = string_chain);
                            continue;
                        }
                    };

                    progresses.push(progress);
                }
            }
        }

        let mut tasks = Vec::new();
        for progress in progresses {
            if let Err(err) = progress.save(app, true) {
                let ids_string = progress.get_ids_string();
                let episode_title = &progress.episode_title;
                let err_title = format!("{ids_string} `{episode_title}`保存下载进度到文件失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
            }

            let auto_start = app.get_config().read().auto_start_download_task;
            let init_state = if auto_start {
                DownloadTaskState::Pending
            } else {
                DownloadTaskState::Paused
            };

            let (state_sender, _) = watch::channel(init_state);
            let (restart_sender, _) = watch::channel(());
            let (cancel_sender, _) = watch::channel(());
            let (delete_sender, _) = watch::channel(());

            let task = Arc::new(Self {
                app: app.clone(),
                state_sender,
                restart_sender,
                cancel_sender,
                delete_sender,
                task_id: progress.task_id.clone(),
                progress: RwLock::new(progress),
            });

            tauri::async_runtime::spawn(task.clone().process());

            tasks.push(task);
        }

        tasks
    }

    pub fn from_progress(app: AppHandle, progress: DownloadProgress) -> Arc<Self> {
        let init_state = if progress.is_completed() {
            DownloadTaskState::Completed
        } else {
            DownloadTaskState::Paused
        };
        let (state_sender, _) = watch::channel(init_state);
        let (restart_sender, _) = watch::channel(());
        let (cancel_sender, _) = watch::channel(());
        let (delete_sender, _) = watch::channel(());

        let task = Arc::new(Self {
            app,
            state_sender,
            restart_sender,
            cancel_sender,
            delete_sender,
            task_id: progress.task_id.clone(),
            progress: RwLock::new(progress),
        });

        tauri::async_runtime::spawn(task.clone().process());

        task
    }

    async fn process(self: Arc<Self>) {
        let task_id = &self.task_id;
        let state = *self.state_sender.borrow();
        let progress = self.progress.read().clone();
        let _ = DownloadEvent::TaskCreate { state, progress }.emit(&self.app);

        let mut state_receiver = self.state_sender.subscribe();
        state_receiver.mark_changed();

        let mut restart_receiver = self.restart_sender.subscribe();
        let mut cancel_receiver = self.cancel_sender.subscribe();
        let mut delete_receiver = self.delete_sender.subscribe();

        let mut permit = None;
        let mut download_task_option = None;

        loop {
            let state = *state_receiver.borrow();
            let state_is_downloading = state == DownloadTaskState::Downloading;
            let state_is_pending = state == DownloadTaskState::Pending;

            let download_task = async {
                download_task_option
                    .get_or_insert_with(|| Box::pin(self.download()))
                    .await;
            };

            tokio::select! {
                () = download_task, if state_is_downloading && permit.is_some() => {
                    download_task_option = None;
                    if let Some(permit) = permit.take() {
                        drop(permit);
                    }
                }

                () = self.acquire_task_permit(&mut permit), if state_is_pending => {},

                _ = state_receiver.changed() => {
                    self.handle_state_change(&mut permit, &mut state_receiver).await;
                }

                _ = restart_receiver.changed() => {
                    self.handle_restart_notify();
                    tracing::debug!("ID为`{task_id}`的下载任务已重来");
                    download_task_option = None;
                }

                _ = cancel_receiver.changed() => return,

                _ = delete_receiver.changed() => {
                    let _ = DownloadEvent::TaskDelete {
                        task_id: self.task_id.clone(),
                    }
                    .emit(&self.app);

                    if permit.is_some() {
                        // 如果有permit则稍微等一下再退出
                        // 这是为了避免大批量删除时，本应删除的任务因拿到permit而又稍微下载一小段
                        sleep(Duration::from_millis(100)).await;
                    }

                    tracing::debug!("ID为`{task_id}`的下载任务已删除");
                    return;
                }
            }
        }
    }

    async fn download(self: &Arc<Self>) {
        let mut progress = self.progress.read().clone();
        let ids_string = progress.get_ids_string();
        let episode_title = progress.episode_title.clone();

        if progress.is_completed() {
            tracing::info!("{ids_string} 跳过`{episode_title}`的下载，因为它已经完成");
            self.set_state(DownloadTaskState::Completed);
            return;
        }

        tracing::debug!("{ids_string} 开始下载`{episode_title}`");
        if let Err(err) = progress
            .process(self)
            .await
            .context("[继续]失败的任务可以断点续传")
        {
            let err_title = format!("{ids_string} `{episode_title}`下载失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);

            self.set_state(DownloadTaskState::Failed);

            return;
        }

        self.sleep_between_task().await;

        self.set_state(DownloadTaskState::Completed);
        tracing::info!("{ids_string} `{episode_title}`下载完成");
    }

    async fn sleep_between_task(&self) {
        let task_id = &self.task_id;
        let mut remaining_sec = self.app.get_config().read().task_download_interval_sec;
        while remaining_sec > 0 {
            // 发送章节休眠事件
            let _ = DownloadEvent::TaskSleeping {
                task_id: task_id.clone(),
                remaining_sec,
            }
            .emit(&self.app);
            sleep(Duration::from_secs(1)).await;
            remaining_sec -= 1;
        }
    }

    async fn acquire_task_permit<'a>(&'a self, permit: &mut Option<SemaphorePermit<'a>>) {
        let (episode_title, ids_string) = {
            let progress = self.progress.read();
            (progress.episode_title.clone(), progress.get_ids_string())
        };

        *permit = match permit.take() {
            // 如果有permit，则直接用
            Some(permit) => Some(permit),
            // 如果没有permit，则获取permit
            None => match self
                .app
                .get_download_manager()
                .inner()
                .task_sem
                .acquire()
                .await
                .map_err(anyhow::Error::from)
            {
                Ok(permit) => Some(permit),
                Err(err) => {
                    let err_title =
                        format!("{ids_string} `{episode_title}`获取下载任务的permit失败");
                    let string_chain = err.to_string_chain();
                    tracing::error!(err_title, message = string_chain);

                    self.set_state(DownloadTaskState::Failed);

                    return;
                }
            },
        };
        // 如果当前任务状态不是`Pending`，则不将任务状态设置为`Downloading`
        if *self.state_sender.borrow() != DownloadTaskState::Pending {
            return;
        }
        // 将任务状态设置为`Downloading`
        if let Err(err) = self
            .state_sender
            .send(DownloadTaskState::Downloading)
            .map_err(anyhow::Error::from)
        {
            let err_title = format!("{ids_string} `{episode_title}`发送状态`Downloading`失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);

            self.set_state(DownloadTaskState::Failed);
        }
    }

    async fn handle_state_change<'a>(
        &'a self,
        permit: &mut Option<SemaphorePermit<'a>>,
        state_receiver: &mut watch::Receiver<DownloadTaskState>,
    ) {
        let state = *state_receiver.borrow();
        let task_id = self.task_id.clone();
        let _ = DownloadEvent::TaskStateUpdate { task_id, state }.emit(&self.app);

        if state == DownloadTaskState::Paused {
            // 稍微等一下再释放permit
            // 避免大批量暂停时，本应暂停的任务因拿到permit而稍微下载一小段(虽然最终会被暂停)
            sleep(Duration::from_millis(100)).await;
            let task_id = &self.task_id;
            tracing::debug!("ID为`{task_id}`的下载任务已暂停");
            if let Some(permit) = permit.take() {
                drop(permit);
            }
        }
    }

    fn handle_restart_notify(&self) {
        self.update_progress(|p| {
            p.mark_uncompleted();
        });
        self.set_state(DownloadTaskState::Pending);
    }

    pub fn set_state(&self, state: DownloadTaskState) {
        let (episode_title, ids_string) = {
            let progress = self.progress.read();
            (progress.episode_title.clone(), progress.get_ids_string())
        };

        if let Err(err) = self.state_sender.send(state).map_err(anyhow::Error::from) {
            let err_title = format!("{ids_string} `{episode_title}`发送状态`{state:?}`失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }
    }

    pub fn update_progress(&self, update_fn: impl FnOnce(&mut DownloadProgress)) {
        // 修改数据
        let updated_progress = {
            let mut progress = self.progress.write();
            update_fn(&mut progress);
            progress
        };
        // 发送更新事件并保存到文件
        let _ = DownloadEvent::ProgressUpdate {
            progress: updated_progress.clone(),
        }
        .emit(&self.app);

        if let Err(err) = updated_progress.save(&self.app, false) {
            let ids_string = updated_progress.get_ids_string();
            let episode_title = &updated_progress.episode_title;
            let err_title = format!("{ids_string} `{episode_title}`保存下载进度到文件失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }
    }
}
