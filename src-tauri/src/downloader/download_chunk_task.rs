use std::{
    fs::File,
    io::{Seek, Write},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use tokio::{sync::SemaphorePermit, time::sleep};
use tracing::instrument;

use crate::{
    downloader::{download_task::DownloadTask, download_task_state::DownloadTaskState},
    extensions::AppHandleExt,
};

pub struct DownloadChunkTask {
    pub download_task: Arc<DownloadTask>,
    pub start: u64,
    pub end: u64,
    pub url: String,
    pub file: Arc<Mutex<File>>,
    pub chunk_index: usize,
}

/// 分片任务的结束方式
pub enum DownloadChunkResult {
    /// 分片确实下载完成了，携带分片下标
    Downloaded(usize),
    /// 分片被暂停/重来/删除打断了，还没下载完
    /// 上层不能把它标记为已下载，否则续传时会跳过，留下一个空洞
    Interrupted,
}

impl DownloadChunkTask {
    #[instrument(
        level = "error",
        skip_all,
        fields(
            url = self.url,
            chunk_index = ?self.chunk_index,
            start = self.start,
            end = self.end,
        )
    )]
    pub async fn process(self) -> eyre::Result<DownloadChunkResult> {
        let download_chunk_task = self.download_chunk();
        tokio::pin!(download_chunk_task);

        let mut state_receiver = self.download_task.state_sender.subscribe();
        state_receiver.mark_changed();

        let mut restart_receiver = self.download_task.restart_sender.subscribe();
        let mut delete_receiver = self.download_task.delete_sender.subscribe();

        let mut permit = None;

        loop {
            let state_is_downloading = *state_receiver.borrow() == DownloadTaskState::Downloading;
            tokio::select! {
                result = &mut download_chunk_task, if state_is_downloading && permit.is_some() => break result,

                result = self.acquire_chunk_permit(&mut permit), if state_is_downloading && permit.is_none() => {
                    match result {
                        Ok(()) => {},
                        Err(err) => break Err(err),
                    }
                },

                _ = state_receiver.changed() => {
                    if *state_receiver.borrow() == DownloadTaskState::Paused {
                        // 稍微等一下再释放permit
                        sleep(Duration::from_millis(100)).await;
                        if let Some(permit) = permit.take() {
                            drop(permit);
                        }
                    }
                },
                // 被重来/删除打断，分片还没下载完，交给上层处理
                _ = restart_receiver.changed() => break Ok(DownloadChunkResult::Interrupted),

                _ = delete_receiver.changed() => break Ok(DownloadChunkResult::Interrupted),
            }
        }
    }

    #[instrument(level = "error", skip_all)]
    async fn download_chunk(&self) -> eyre::Result<DownloadChunkResult> {
        let bili_client = self.download_task.app.get_bili_client();
        let chunk_data = bili_client
            .get_media_chunk(&self.url, self.start, self.end)
            .await?;

        let len = chunk_data.len() as u64;
        self.download_task
            .app
            .get_download_manager()
            .byte_per_sec
            .fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        // 将下载的内容写入文件
        {
            let mut file = self.file.lock();
            file.seek(std::io::SeekFrom::Start(self.start))?;
            file.write_all(&chunk_data)?;
        }

        let chunk_download_interval_sec = self
            .download_task
            .app
            .get_config()
            .read()
            .chunk_download_interval_sec;
        sleep(Duration::from_secs(chunk_download_interval_sec)).await;

        Ok(DownloadChunkResult::Downloaded(self.chunk_index))
    }

    #[instrument(level = "error", skip_all)]
    async fn acquire_chunk_permit<'a>(
        &'a self,
        permit: &mut Option<SemaphorePermit<'a>>,
    ) -> eyre::Result<()> {
        *permit = match permit.take() {
            // 如果有permit，则直接用
            Some(permit) => Some(permit),
            // 如果没有permit，则获取permit
            None => Some(
                self.download_task
                    .app
                    .get_download_manager()
                    .inner()
                    .media_chunk_sem
                    .acquire()
                    .await?,
            ),
        };

        Ok(())
    }
}
