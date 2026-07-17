use std::time::{Duration, SystemTime};

use base64::{Engine, engine::general_purpose};
use bytes::Bytes;
use eyre::{OptionExt, WrapErr, eyre};
use parking_lot::RwLock;
use prost::Message;
use reqwest::{Client, StatusCode};
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::{
    Jitter, RetryDecision, RetryPolicy, RetryTransientMiddleware, policies::ExponentialBackoff,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{
    AppHandle,
    http::{HeaderMap, HeaderValue},
};
use tokio::{task::JoinSet, time::sleep};
use tracing::{Instrument, instrument};

use crate::{
    config::ProxyMode,
    extensions::{AppHandleExt, EyreReportToMessage},
    protobuf::DmSegMobileReply,
    types::{
        bangumi_follow_info::BangumiFollowInfo, bangumi_info::BangumiInfo,
        bangumi_media_url::BangumiMediaUrl, bangumi_media_url_v2::BangumiMediaUrlV2,
        cheese_info::CheeseInfo, cheese_media_url::CheeseMediaUrl, fav_folders::FavFolders,
        fav_info::FavInfo, get_bangumi_follow_info_params::GetBangumiFollowInfoParams,
        get_bangumi_info_params::GetBangumiInfoParams, get_cheese_info_params::GetCheeseInfoParams,
        get_fav_info_params::GetFavInfoParams, get_history_info_params::GetHistoryInfoParams,
        get_normal_info_params::GetNormalInfoParams,
        get_user_video_info_params::GetUserVideoInfoParams, history_info::HistoryInfo,
        normal_info::NormalInfo, normal_media_url::NormalMediaUrl, player_info::PlayerInfo,
        qrcode_data::QrcodeData, qrcode_status::QrcodeStatus, skip_segments::SkipSegments,
        subtitle::Subtitle, tags::Tags, user_info::UserInfo, user_video_info::UserVideoInfo,
        watch_later_info::WatchLaterInfo,
    },
};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
const REFERRER: &str = "https://www.bilibili.com/";

/// 分片下载失败后最多重试多少次
const CHUNK_MAX_RETRIES: u32 = 20;
/// 分片重试的间隔范围
const CHUNK_RETRY_MIN_INTERVAL: Duration = Duration::from_secs(1);
const CHUNK_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(300);
/// 超过这么久没收到任何数据就认为连接已经死了
/// 不能用`timeout`(整个请求的超时)，那会误杀正常但缓慢的分片下载
const CHUNK_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// 单次分片下载尝试的错误，用于区分`可以重试`和`重试没有意义`
enum ChunkAttemptError {
    Retryable(eyre::Report),
    Fatal(eyre::Report),
}

impl ChunkAttemptError {
    fn new(retryable: bool, report: eyre::Report) -> Self {
        if retryable {
            Self::Retryable(report)
        } else {
            Self::Fatal(report)
        }
    }
}

/// 判断http状态码是否值得重试
fn is_retryable_status(status: StatusCode) -> bool {
    match status {
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => true,
        status if status.is_server_error() => true,
        // 403(链接过期)、404、416(range不合法)重试没有意义
        // 200说明服务器忽略了range，响应体是整个文件，重试也不会变
        _ => false,
    }
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_body() || err.is_decode() || err.is_request()
}

fn is_retryable_middleware_error(err: &reqwest_middleware::Error) -> bool {
    match err {
        reqwest_middleware::Error::Reqwest(err) => is_retryable_reqwest_error(err),
        // 中间件自身的错误(例如代理配置有误)，重试没有意义
        reqwest_middleware::Error::Middleware(_) => false,
    }
}

/// 分片重试的退避策略
/// base必须是2：base为1时`1^n`恒等于1，退化成固定间隔且`Jitter::Bounded`完全失效
fn chunk_retry_policy() -> ExponentialBackoff {
    ExponentialBackoff::builder()
        .retry_bounds(CHUNK_RETRY_MIN_INTERVAL, CHUNK_RETRY_MAX_INTERVAL)
        .base(2)
        .jitter(Jitter::Bounded)
        .build_with_max_retries(CHUNK_MAX_RETRIES)
}

pub struct BiliClient {
    pub app: AppHandle,
    pub api_client: RwLock<ClientWithMiddleware>,
    pub media_client: RwLock<ClientWithMiddleware>,
    pub content_length_client: RwLock<Client>,
}

impl BiliClient {
    pub fn new(app: AppHandle) -> Self {
        let api_client = create_api_client(&app);
        let api_client = RwLock::new(api_client);

        let media_client = create_media_client(&app);
        let media_client = RwLock::new(media_client);

        let content_length_client = create_content_length_client(&app);
        let content_length_client = RwLock::new(content_length_client);

        Self {
            app,
            api_client,
            media_client,
            content_length_client,
        }
    }

    pub fn reload_client(&self) {
        let api_client = create_api_client(&self.app);
        *self.api_client.write() = api_client;
        let media_client = create_media_client(&self.app);
        *self.media_client.write() = media_client;
        let content_length_client = create_content_length_client(&self.app);
        *self.content_length_client.write() = content_length_client;
    }

    #[instrument(level = "error", skip_all)]
    pub async fn generate_qrcode(&self) -> eyre::Result<QrcodeData> {
        // 发送生成二维码请求
        let request = self
            .api_client
            .read()
            .get("https://passport.bilibili.com/x/passport-login/web/qrcode/generate");
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为二维码数据
        let data_str = data.to_string();
        let qrcode_data: QrcodeData = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为QrcodeData失败: {data_str}"))?;

        Ok(qrcode_data)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_qrcode_status(&self, qrcode_key: &str) -> eyre::Result<QrcodeStatus> {
        // 发送获取二维码状态请求
        let params = json!({"qrcode_key": qrcode_key});
        let request = self
            .api_client
            .read()
            .get("https://passport.bilibili.com/x/passport-login/web/qrcode/poll")
            .query(&params);
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为二维码状态
        let data_str = data.to_string();
        let qrcode_status: QrcodeStatus = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为QrcodeStatus失败: {data_str}"))?;
        if ![0, 86101, 86090, 86038].contains(&qrcode_status.code) {
            return Err(eyre!("预料之外的二维码code: {qrcode_status:?}"));
        }
        Ok(qrcode_status)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_user_info(&self, sessdata: &str) -> eyre::Result<UserInfo> {
        // 发送获取用户信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/web-interface/nav")
            .header("cookie", format!("SESSDATA={sessdata}"));
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code == -101 {
            return Err(eyre!("cookie错误或已过期，请重新登录: {bili_resp:?}"));
        } else if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为UserInfo
        let data_str = data.to_string();
        let user_info: UserInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为UserInfo失败: {data_str}"))?;

        Ok(user_info)
    }

    #[instrument(level = "error", skip_all, fields(bvid = params.get_bvid(), aid = params.get_aid()))]
    pub async fn get_normal_info(&self, params: GetNormalInfoParams) -> eyre::Result<NormalInfo> {
        use GetNormalInfoParams::{Aid, Bvid};
        let params = match params {
            Bvid(bvid) => json!({"bvid": bvid}),
            Aid(aid) => json!({"aid": aid}),
        };
        // 发送获取普通视频信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/web-interface/view")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为NormalInfo
        let data_str = data.to_string();
        let normal_info: NormalInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为NormalInfo失败: {data_str}"))?;

        Ok(normal_info)
    }

    #[instrument(level = "error", skip_all, fields(ep_id = params.get_ep_id(), season_id = params.get_season_id()))]
    pub async fn get_bangumi_info(
        &self,
        params: GetBangumiInfoParams,
    ) -> eyre::Result<BangumiInfo> {
        use GetBangumiInfoParams::{EpId, SeasonId};
        let params = match params {
            EpId(ep_id) => json!({"ep_id": ep_id}),
            SeasonId(season_id) => json!({"season_id": season_id}),
        };
        // 发送获取番剧视频信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/pgc/view/web/season")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为BangumiInfo
        let data_str = data.to_string();
        let bangumi_info: BangumiInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为BangumiInfo失败: {data_str}"))?;

        Ok(bangumi_info)
    }

    #[instrument(level = "error", skip_all, fields(ep_id = params.get_ep_id(), season_id = params.get_season_id()))]
    pub async fn get_cheese_info(&self, params: GetCheeseInfoParams) -> eyre::Result<CheeseInfo> {
        use GetCheeseInfoParams::{EpId, SeasonId};
        let params = match params {
            EpId(ep_id) => json!({"ep_id": ep_id}),
            SeasonId(season_id) => json!({"season_id": season_id}),
        };
        // 发送获取课程视频信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/pugv/view/web/season")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为CheeseInfo
        let data_str = data.to_string();
        let cheese_info: CheeseInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为CheeseInfo失败: {data_str}"))?;

        Ok(cheese_info)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_user_video_info(
        &self,
        params: GetUserVideoInfoParams,
    ) -> eyre::Result<UserVideoInfo> {
        const DM_IMG_INTER: &str = r#"{"ds":[],"wh":[0,0,0],"of":[0,0,0]}"#;

        fn random_base64() -> String {
            let random_bytes: Vec<u8> = (0..48).map(|_| rand::random_range(32..=127)).collect();

            general_purpose::STANDARD.encode(&random_bytes)
        }

        let mut dm_img_str = random_base64();
        dm_img_str.truncate(dm_img_str.len() - 2);

        let mut dm_cover_img_str = random_base64();
        dm_cover_img_str.truncate(dm_cover_img_str.len() - 2);

        let mut params: Vec<(&str, String)> = vec![
            ("pn", params.pn.to_string()),
            ("ps", "42".to_string()),
            ("mid", params.mid.to_string()),
            ("dm_img_list", "[]".to_string()),
            ("dm_img_str", dm_img_str),
            ("dm_cover_img_str", dm_cover_img_str),
            ("dm_img_inter", DM_IMG_INTER.to_string()),
        ];
        self.wbi(&mut params).await?;

        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/space/wbi/arc/search")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为UserVideoInfo
        let data_str = data.to_string();
        let user_video_info: UserVideoInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为UserVideoInfo失败: {data_str}"))?;

        Ok(user_video_info)
    }

    #[instrument(level = "error", skip_all, fields(bvid = bvid, cid = cid))]
    pub async fn get_normal_url(&self, bvid: &str, cid: i64) -> eyre::Result<NormalMediaUrl> {
        let params = json!({
            "bvid": bvid,
            "cid": cid,
            "qn": 127,
            "fnval": 4048,
        });
        // 发送获取普通url的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/player/wbi/playurl")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为NormalMediaUrl
        let data_str = data.to_string();
        let media_url: NormalMediaUrl = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为NormalMediaUrl失败: {data_str}"))?;

        Ok(media_url)
    }

    #[instrument(level = "error", skip_all, fields(cid = cid))]
    pub async fn get_bangumi_url(&self, cid: i64) -> eyre::Result<BangumiMediaUrl> {
        let media_url_v2 = self.get_bangumi_url_v2(cid).await?;
        if media_url_v2.video_info.is_drm {
            self.get_bangumi_url_v1(cid).await
        } else {
            Ok(media_url_v2.video_info)
        }
    }

    #[instrument(level = "error", skip_all, fields(cid = cid))]
    async fn get_bangumi_url_v1(&self, cid: i64) -> eyre::Result<BangumiMediaUrl> {
        let params = json!({
            "cid": cid,
            "qn": 127,
            "fnval": 4048,
            "drm_tech_type": 2,
        });
        // 发送获取番剧url的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/pgc/player/web/playurl")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code == -10403 {
            return Err(eyre!("地区限制，请使用代理或切换线路后重试: {bili_resp:?}"));
        } else if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为BangumiMediaUrl
        let data_str = data.to_string();
        let media_url: BangumiMediaUrl = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为BangumiMediaUrl失败: {data_str}"))?;

        Ok(media_url)
    }

    #[instrument(level = "error", skip_all, fields(cid = cid))]
    async fn get_bangumi_url_v2(&self, cid: i64) -> eyre::Result<BangumiMediaUrlV2> {
        let params = json!({
            "cid": cid,
            "qn": 127,
            "fnval": 4048,
            "drm_tech_type": 2,
            "from_client": "BROWSER",
        });
        // 发送获取番剧url的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/pgc/player/web/v2/playurl")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code == -10403 {
            return Err(eyre!("地区限制，请使用代理或切换线路后重试: {bili_resp:?}"));
        } else if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为BangumiMediaUrlV2
        let data_str = data.to_string();
        let media_url: BangumiMediaUrlV2 = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为BangumiMediaUrlV2失败: {data_str}"))?;

        Ok(media_url)
    }

    #[instrument(level = "error", skip_all, fields(ep_id = ep_id))]
    pub async fn get_cheese_url(&self, ep_id: i64) -> eyre::Result<CheeseMediaUrl> {
        let params = json!({
            "ep_id": ep_id,
            "qn": 127,
            "fnval": 4048,
            "drm_tech_type": 2,
        });
        // 发送获取课程url的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/pugv/player/web/playurl")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code == -403 {
            return Err(eyre!("没有观看权限，请先购买: {bili_resp:?}"));
        } else if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为CheeseMediaUrl
        let data_str = data.to_string();
        let media_url: CheeseMediaUrl = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为CheeseMediaUrl失败: {data_str}"))?;

        Ok(media_url)
    }

    #[instrument(level = "error", skip_all, fields(aid = aid, cid = cid))]
    pub async fn get_player_info(&self, aid: i64, cid: i64) -> eyre::Result<PlayerInfo> {
        let params = json!({
            "aid": aid,
            "cid": cid,
        });
        // 发送获取播放器信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/player/wbi/v2")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为PlayerInfo
        let data_str = data.to_string();
        let player_info: PlayerInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为PlayerInfo失败: {data_str}"))?;

        Ok(player_info)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_fav_folders(&self, uid: i64) -> eyre::Result<FavFolders> {
        let params = json!({"up_mid": uid});
        // 发送获取收藏夹信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/v3/fav/folder/created/list-all")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为FavFolders
        let data_str = data.to_string();
        let fav_folders: FavFolders = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为FavFolders失败: {data_str}"))?;

        Ok(fav_folders)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_fav_info(&self, params: GetFavInfoParams) -> eyre::Result<FavInfo> {
        let params = json!({
            "media_id": params.media_list_id,
            "pn": params.pn,
            "ps": 36,
            "platform": "web",
        });
        // 发送获取收藏夹信息的请求
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/v3/fav/resource/list")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为FavInfo
        let data_str = data.to_string();
        let fav_info: FavInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为FavInfo失败: {data_str}"))?;

        Ok(fav_info)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_watch_later_info(&self, page: i32) -> eyre::Result<WatchLaterInfo> {
        // 发送获取稍后观看信息的请求
        let params = json!({"ps": 20, "pn": page});
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/v2/history/toview")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为WatchLaterInfo
        let data_str = data.to_string();
        let watch_later_info: WatchLaterInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为WatchLaterInfo失败: {data_str}"))?;

        Ok(watch_later_info)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_bangumi_follow_info(
        &self,
        params: GetBangumiFollowInfoParams,
    ) -> eyre::Result<BangumiFollowInfo> {
        // 发送获取番剧追踪信息的请求
        let params = json!({
            "vmid": params.vmid,
            "type": params.type_field,
            "pn": params.pn,
            "ps": 24,
            "follow_status": params.follow_status,
        });
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/space/bangumi/follow/list")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为BangumiFollowInfo
        let data_str = data.to_string();
        let bangumi_follow_info: BangumiFollowInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为BangumiFollowInfo失败: {data_str}"))?;

        Ok(bangumi_follow_info)
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_history_info(
        &self,
        params: GetHistoryInfoParams,
    ) -> eyre::Result<HistoryInfo> {
        let device_type: i64 = params.device_type.into();
        let params = json!({
            "pn": params.pn,
            "keyword": params.keyword,
            "business": "archive",
            "add_time_start": params.add_time_start,
            "add_time_end": params.add_time_end,
            "arc_max_duration": params.arc_max_duration,
            "arc_min_duration": params.arc_min_duration,
            "device_type": device_type,
        });
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/web-interface/history/search")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为HistoryInfo
        let data_str = data.to_string();
        let history_info: HistoryInfo = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为HistoryInfo失败: {data_str}"))?;

        Ok(history_info)
    }

    #[instrument(level = "error", skip_all, fields(start = start, end = end))]
    pub async fn get_media_chunk(
        &self,
        media_url: &str,
        start: u64,
        end: u64,
    ) -> eyre::Result<Bytes> {
        let retry_policy = chunk_retry_policy();

        let start_time = SystemTime::now();
        let mut n_past_retries: u32 = 0;

        loop {
            let err = match self.try_get_media_chunk(media_url, start, end).await {
                Ok(bytes) => {
                    if n_past_retries > 0 {
                        tracing::info!("分片重试`{n_past_retries}`次后下载成功");
                    }
                    return Ok(bytes);
                }
                Err(ChunkAttemptError::Fatal(err)) => return Err(err),
                Err(ChunkAttemptError::Retryable(err)) => err,
            };

            let RetryDecision::Retry { execute_after } =
                retry_policy.should_retry(start_time, n_past_retries)
            else {
                return Err(err.wrap_err(format!("重试`{n_past_retries}`次后仍然失败")));
            };
            // 时钟回拨时立即重试即可
            let wait_for = execute_after
                .duration_since(SystemTime::now())
                .unwrap_or_default();

            tracing::warn!(
                err_title = "分片下载失败，即将重试",
                n_past_retries,
                wait_ms = wait_for.as_millis(),
                message = err.to_message(),
            );
            // 这里sleep不影响暂停/重来/删除的响应，`process`的`select!`仍在轮询其他分支
            sleep(wait_for).await;
            n_past_retries += 1;
        }
    }

    /// 下载一个分片，只尝试一次
    #[instrument(level = "error", skip_all)]
    async fn try_get_media_chunk(
        &self,
        media_url: &str,
        start: u64,
        end: u64,
    ) -> Result<Bytes, ChunkAttemptError> {
        // 必须单独一条语句，让`media_client`的读锁在语句结束时释放，不要跨await持有
        let request = self
            .media_client
            .read()
            .get(media_url)
            .header("range", format!("bytes={start}-{end}"));

        let http_resp = match request.send().await {
            Ok(http_resp) => http_resp,
            Err(err) => {
                let retryable = is_retryable_middleware_error(&err);
                let report = eyre::Report::new(err).wrap_err("发送分片请求失败");
                return Err(ChunkAttemptError::new(retryable, report));
            }
        };

        // 这个检查必须留在重试循环内：服务器可能忽略range返回200(响应体是整个文件)
        // 那样写到`start`偏移会静默损坏文件
        let status = http_resp.status();
        if status != StatusCode::PARTIAL_CONTENT {
            let report = eyre!("预料之外的状态码({status})");
            return Err(ChunkAttemptError::new(is_retryable_status(status), report));
        }

        // B站CDN经常在这里中断连接，这就是分片下载失败的根源
        // 之前这里的错误不会被重试，因为中间件只重试`send`，而`send`在响应头到达时就返回了
        let bytes = match http_resp.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                let retryable = is_retryable_reqwest_error(&err);
                let report = eyre::Report::new(err).wrap_err("读取分片响应体失败");
                return Err(ChunkAttemptError::new(retryable, report));
            }
        };

        // 校验长度，防止服务器返回了别的range导致文件损坏
        let expected_len = end - start + 1;
        let actual_len = bytes.len() as u64;
        if actual_len != expected_len {
            let report = eyre!("分片长度不符(预期`{expected_len}`字节，实际`{actual_len}`字节)");
            return Err(ChunkAttemptError::Retryable(report));
        }

        Ok(bytes)
    }

    #[instrument(level = "error", skip_all, fields(media_url = media_url))]
    pub async fn get_content_length(&self, media_url: &str) -> eyre::Result<u64> {
        #[instrument(level = "error", skip_all)]
        fn parse_content_length(headers: &HeaderMap) -> eyre::Result<u64> {
            headers
                .get("Content-Length")
                .ok_or_eyre("缺少 Content-Length 响应头")?
                .to_str()
                .wrap_err("Content-Length 响应头无法转换为字符串")?
                .parse::<u64>()
                .wrap_err("Content-Length 响应头无法转换为整数")
        }

        #[instrument(level = "error", skip_all)]
        fn parse_total_from_content_range(headers: &HeaderMap) -> eyre::Result<u64> {
            // Example: "bytes 0-0/12345"
            let content_range = headers
                .get("Content-Range")
                .ok_or_eyre("缺少 Content-Range 响应头")?
                .to_str()
                .wrap_err("Content-Range 响应头无法转换为字符串")?;

            let Some((_, total)) = content_range.split_once('/') else {
                return Err(eyre!("预料之外的 Content-Range 格式: {content_range}"));
            };

            total
                .parse::<u64>()
                .wrap_err("Content-Range 总大小无法转换为整数")
        }

        // 优先使用 HEAD 获取 Content-Length
        let request = self.content_length_client.read().head(media_url);
        let http_resp = request.send().await?;
        let status = http_resp.status();
        if status == StatusCode::OK
            && let Ok(content_length) = parse_content_length(http_resp.headers())
        {
            return Ok(content_length);
        }

        // 部分 upos/CDN 对 HEAD 支持不完整（尤其是高码率/高帧率流），这里降级用 Range 请求探测总大小
        let request = self
            .content_length_client
            .read()
            .get(media_url)
            .header("range", "bytes=0-0");
        let http_resp = request.send().await?;
        let status = http_resp.status();

        if status == StatusCode::PARTIAL_CONTENT {
            return parse_total_from_content_range(http_resp.headers());
        }

        if status == StatusCode::OK {
            return parse_content_length(http_resp.headers());
        }

        Err(eyre!("预料之外的状态码({status})"))
    }

    #[instrument(level = "error", skip_all)]
    pub async fn get_url_with_content_length(&self, urls: Vec<String>) -> Vec<(String, u64)> {
        let mut url_with_content_length = Vec::new();
        let mut join_set = JoinSet::new();

        for url in urls {
            let app = self.app.clone();
            let get_content_length_task = async move {
                let bili_client = app.get_bili_client();
                let Ok(content_length) = bili_client.get_content_length(&url).await else {
                    return None;
                };
                Some((url, content_length))
            };
            join_set.spawn(get_content_length_task.in_current_span());
        }

        while let Some(join_result) = join_set.join_next().await {
            let Ok(Some((url, content_length))) = join_result else {
                continue;
            };

            url_with_content_length.push((url, content_length));
        }

        url_with_content_length
    }

    #[instrument(level = "error", skip_all, fields(aid = aid, cid = cid, duration = duration))]
    pub async fn get_danmaku(
        &self,
        aid: i64,
        cid: i64,
        duration: u64,
    ) -> eyre::Result<Vec<DmSegMobileReply>> {
        let client = self.api_client.read().clone();
        // 以6分钟为单位分段
        let segment_count = duration.div_ceil(360);

        let mut join_set = JoinSet::new();
        for segment_index in 1..=segment_count {
            let client = client.clone();
            let cookie = self.get_cookie();

            let segment_span =
                tracing::error_span!("get_danmaku_segment", segment_index = segment_index);
            let segment_task = async move {
                // 发送获取分段弹幕的请求
                let params = json!({
                    "type": 1,
                    "oid": cid,
                    "pid": aid,
                    "segment_index": segment_index,
                });
                let http_resp = client
                    .get("https://api.bilibili.com/x/v2/dm/web/seg.so")
                    .query(&params)
                    .header("cookie", cookie)
                    .send()
                    .await?;
                let status = http_resp.status();
                if status != StatusCode::OK {
                    let body = http_resp.text().await?;
                    return Err(eyre!("预料之外的状态码({status}): {body}"));
                }
                let body = http_resp.bytes().await?;
                let reply =
                    DmSegMobileReply::decode(body).wrap_err("将body解析为DmSegMobileReply失败")?;

                Ok(reply)
            };
            join_set.spawn(segment_task.instrument(segment_span));
        }

        let mut replies = Vec::new();
        while let Some(join_result) = join_set.join_next().await {
            let Ok(res) = join_result else {
                continue;
            };

            let reply = res?;
            replies.push(reply);
        }

        Ok(replies)
    }

    #[instrument(level = "error", skip_all, fields(url = url))]
    pub async fn get_subtitle(&self, url: &str) -> eyre::Result<Subtitle> {
        let request = self.api_client.read().get(url);
        let http_resp = request.send().await?;
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为Subtitle
        let subtitle: Subtitle =
            serde_json::from_str(&body).wrap_err(format!("将body解析为Subtitle失败: {body}"))?;

        Ok(subtitle)
    }

    #[instrument(level = "error", skip_all, fields(url = url))]
    pub async fn get_cover_data_and_ext(&self, url: &str) -> eyre::Result<(Bytes, String)> {
        let request = self.api_client.read().get(url);
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        if status != StatusCode::OK {
            let body = http_resp.text().await?;
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }

        let content_type = http_resp
            .headers()
            .get("Content-Type")
            .ok_or_eyre("缺少 Content-Type 响应头")?
            .to_str()
            .wrap_err("Content-Type 响应头无法转换为字符串")?
            .to_string();

        let ext = match content_type.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/avif" => "avif",
            _ => "jpg",
        };

        let bytes = http_resp.bytes().await?;

        Ok((bytes, ext.to_string()))
    }

    #[instrument(level = "error", skip_all, fields(aid = aid))]
    pub async fn get_tags(&self, aid: i64) -> eyre::Result<Tags> {
        // 发送获取普通视频标签的请求
        let params = json!({"aid": aid});
        let request = self
            .api_client
            .read()
            .get("https://api.bilibili.com/x/web-interface/view/detail/tag")
            .query(&params)
            .header("cookie", self.get_cookie());
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为BiliResp
        let bili_resp: BiliResp =
            serde_json::from_str(&body).wrap_err(format!("将body解析为BiliResp失败: {body}"))?;
        // 检查BiliResp的code字段
        if bili_resp.code != 0 {
            return Err(eyre!("预料之外的code: {bili_resp:?}"));
        }
        // 检查BiliResp的data是否存在
        let Some(data) = bili_resp.data else {
            return Err(eyre!("BiliResp中不存在data字段: {bili_resp:?}"));
        };
        // 尝试将data解析为Tags
        let data_str = data.to_string();
        let tags: Tags = serde_json::from_str(&data_str)
            .wrap_err(format!("将data解析为Tags失败: {data_str}"))?;

        Ok(tags)
    }

    #[instrument(level = "error", skip_all, fields(bvid = bvid, cid = cid))]
    pub async fn get_skip_segments(
        &self,
        bvid: &str,
        cid: Option<i64>,
    ) -> eyre::Result<SkipSegments> {
        // 发送获取跳过片段的请求
        let mut params = json!({
            "videoID": bvid,
            "actionType": "skip",
        });
        if let Some(cid) = cid {
            params["cid"] = cid.into();
        }

        let request = self
            .api_client
            .read()
            .get("https://bsbsb.top/api/skipSegments")
            .query(&params);
        let http_resp = request.send().await?;
        // 检查http响应状态码
        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(SkipSegments(Vec::new()));
        } else if status != StatusCode::OK {
            return Err(eyre!("预料之外的状态码({status}): {body}"));
        }
        // 尝试将body解析为SkipSegments
        let skip_segments: SkipSegments = serde_json::from_str(&body)
            .wrap_err(format!("将body解析为SkipSegments失败: {body}"))?;

        Ok(skip_segments)
    }

    pub fn get_cookie(&self) -> String {
        let sessdata = self.app.get_config().read().sessdata.clone();
        format!("SESSDATA={}", sessdata.trim_end_matches(';'))
    }
}

fn create_api_client(app: &AppHandle) -> ClientWithMiddleware {
    // base必须是2：base为1时`1^n`恒等于1，退化成固定间隔且`Jitter::Bounded`完全失效
    let retry_policy = ExponentialBackoff::builder()
        .retry_bounds(Duration::from_secs(1), Duration::from_secs(2))
        .base(2)
        .jitter(Jitter::Bounded)
        .build_with_total_retry_duration(Duration::from_secs(5));

    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("referer", HeaderValue::from_static(REFERRER));

    let client = reqwest::ClientBuilder::new()
        .set_proxy(app, "api_client")
        .timeout(Duration::from_secs(3))
        .default_headers(headers)
        .build()
        .unwrap();

    reqwest_middleware::ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

fn create_media_client(app: &AppHandle) -> ClientWithMiddleware {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("referer", HeaderValue::from_static(REFERRER));

    let client = reqwest::ClientBuilder::new()
        .set_proxy(app, "media_client")
        // 只限制单次读取的间隔，读到数据就重置
        // 用来发现已经死掉的连接，又不会误杀正常但缓慢的分片下载
        .read_timeout(CHUNK_READ_TIMEOUT)
        .default_headers(headers)
        .build()
        .unwrap();

    // 不再使用`RetryTransientMiddleware`：它只重试`send`，看不到读取响应体时的失败
    // 重试逻辑已上移到`get_media_chunk`，覆盖`send`+状态码检查+读取响应体
    reqwest_middleware::ClientBuilder::new(client).build()
}

fn create_content_length_client(app: &AppHandle) -> Client {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("referer", HeaderValue::from_static(REFERRER));

    reqwest::ClientBuilder::new()
        .set_proxy(app, "content_length_client")
        .timeout(Duration::from_secs(5))
        .default_headers(headers)
        .build()
        .unwrap()
}

trait ClientBuilderExt {
    fn set_proxy(self, app: &AppHandle, client_name: &str) -> Self;
}

impl ClientBuilderExt for reqwest::ClientBuilder {
    fn set_proxy(self, app: &AppHandle, client_name: &str) -> reqwest::ClientBuilder {
        let proxy_mode = app.get_config().read().proxy_mode;
        match proxy_mode {
            ProxyMode::NoProxy => self.no_proxy(),
            ProxyMode::System => self,
            ProxyMode::Custom => {
                let config = app.get_config().inner().read();
                let proxy_host = &config.proxy_host;
                let proxy_port = &config.proxy_port;
                let proxy_url = format!("http://{proxy_host}:{proxy_port}");

                match reqwest::Proxy::all(&proxy_url).map_err(eyre::Report::from) {
                    Ok(proxy) => self.proxy(proxy),
                    Err(err) => {
                        let err_title = format!("{client_name}将`{proxy_url}`设为代理失败，将直连");
                        let message = err.to_message();
                        tracing::error!(err_title, message);
                        self.no_proxy()
                    }
                }
            }
        }
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct BiliResp {
    pub code: i64,
    #[serde(default, alias = "message")]
    pub msg: String,
    #[serde(alias = "result")]
    pub data: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::{
        chunk_retry_policy, is_retryable_reqwest_error, is_retryable_status, RetryDecision,
        RetryPolicy, StatusCode, CHUNK_MAX_RETRIES, CHUNK_RETRY_MAX_INTERVAL,
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, SystemTime},
    };

    /// 返回`should_retry`要求等待的时长，`DoNotRetry`时返回`None`
    fn wait_for(n_past_retries: u32) -> Option<Duration> {
        let now = SystemTime::now();
        match chunk_retry_policy().should_retry(now, n_past_retries) {
            RetryDecision::Retry { execute_after } => {
                Some(execute_after.duration_since(now).unwrap_or_default())
            }
            RetryDecision::DoNotRetry => None,
        }
    }

    /// 这是`base(1)`那个bug的回归测试
    /// `base`为1时`1^n`恒等于1，每次都恰好等待1秒，`Jitter::Bounded`也会被完全抵消
    /// 所以这里断言等待时长确实会随重试次数增长
    /// jitter是随机的，单次采样可能偏小，因此多采样几次看最大值
    #[test]
    fn 重试间隔应该随次数增长() {
        // 第0次重试恒等于最小间隔，这是`Jitter::Bounded`的固有行为
        // 留一点余量：`should_retry`内部重新取了一次`now`，会比这里晚几微秒
        let first_wait = wait_for(0).unwrap();
        assert!(
            (Duration::from_secs(1)..Duration::from_millis(1100)).contains(&first_wait),
            "第0次重试应该恰好等待最小间隔，实际`{first_wait:?}`"
        );

        // 第2次重试的未抖动间隔是4秒，抖动后落在[1s, 4s]
        // 如果`base`是1，这里恒等于1秒，取多少次最大值都不会超过1秒
        let max_wait = (0..20).filter_map(|_| wait_for(2)).max().unwrap();
        assert!(
            max_wait > Duration::from_millis(1500),
            "重试间隔没有随次数增长，`base`可能被改回了1，实际最大间隔`{max_wait:?}`"
        );
    }

    #[test]
    fn 重试间隔不应该超过上限() {
        // 取一个未抖动间隔(`2^n`秒)远超上限的重试次数，确保真的触发了封顶
        let n = CHUNK_MAX_RETRIES - 1;
        assert!(
            2u64.pow(n) > CHUNK_RETRY_MAX_INTERVAL.as_secs(),
            "第`{n}`次重试的未抖动间隔没有超过上限，这个测试没测到封顶"
        );
        for _ in 0..20 {
            assert!(wait_for(n).unwrap() <= CHUNK_RETRY_MAX_INTERVAL);
        }
    }

    #[test]
    fn 超过最大重试次数后应该放弃() {
        assert!(wait_for(CHUNK_MAX_RETRIES - 1).is_some());
        assert_eq!(wait_for(CHUNK_MAX_RETRIES), None);
    }

    /// 复现线上的分片下载失败：服务器声明了`Content-Length`，但发到一半就断开连接
    /// 这正是`end of file before message length reached`的成因
    /// 断言这种错误会被判定为`可以重试`，否则就回到了这个bug本身
    #[tokio::test]
    async fn 响应体中途断开应该判定为可以重试() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            // 声明1000字节，实际只发10字节就断开
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\n\
                      Content-Length: 1000\r\n\
                      Content-Range: bytes 0-999/2000\r\n\r\n\
                      0123456789",
                )
                .unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let http_resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("响应头是完整的，`send`应该成功");
        // 这就是问题的核心：`send`成功了，中间件到此为止，然后读取响应体才失败
        assert_eq!(http_resp.status(), StatusCode::PARTIAL_CONTENT);

        let err = http_resp.bytes().await.expect_err("响应体不完整，应该失败");
        assert!(
            is_retryable_reqwest_error(&err),
            "响应体中途断开必须能重试，实际错误`{err:?}`"
        );
    }

    #[test]
    fn 链接过期等状态码不应该重试() {
        // 403是链接过期，404是资源不存在，416是range不合法，重试没有意义
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::RANGE_NOT_SATISFIABLE));
        // 200说明服务器忽略了range，重试也不会变
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[test]
    fn 暂时性故障应该重试() {
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
    }
}
