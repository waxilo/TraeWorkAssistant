//! Tauri 命令层：账号管理、签到、积分、网关设置。

use crate::accounts::{self, Account, Settings};
use crate::checkin;
use crate::oauth;
use crate::trae_auth;
use tauri::Manager;
use std::path::PathBuf;

/// 解析应用数据目录（macOS: ~/Library/Application Support/<identifier>）
pub fn try_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位数据目录：{e}"))?;
    let _ = std::fs::create_dir_all(&dir);
    Ok(dir)
}

fn settings(app: &tauri::AppHandle) -> Settings {
    match try_data_dir(app) {
        Ok(dir) => accounts::load_settings(&dir),
        Err(_) => Settings::default(),
    }
}

// ---------------------------------------------------------------------------
// 账号
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_accounts(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    Ok(accounts::load_accounts(&try_data_dir(&app)?))
}

#[tauri::command]
pub fn import_accounts(app: tauri::AppHandle, accounts: Vec<Account>) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    for a in accounts {
        // 按手机号（优先）或 token 去重；已存在则不重复添加
        if accounts::contains_equivalent(&list, &a) {
            continue;
        }
        list.push(a);
    }
    accounts::save_accounts(&dir, &list)?;
    Ok(list)
}

#[tauri::command]
pub fn remove_account(app: tauri::AppHandle, id: String) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let list = accounts::load_accounts(&dir);
    let kept: Vec<_> = list.into_iter().filter(|a| a.id != id).collect();
    accounts::save_accounts(&dir, &kept)?;
    Ok(kept)
}

/// 扫描本机 TraeWork 已登录账号，返回「可导入」列表（不含已在库里的）。
#[tauri::command]
pub fn discover_local(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let existing = accounts::load_accounts(&dir);
    Ok(trae_auth::discover_local_accounts()
        .into_iter()
        .map(Account::from)
        .filter(|a| !accounts::contains_equivalent(&existing, a))
        .collect())
}

#[tauri::command]
pub fn toggle_account(app: tauri::AppHandle, id: String, enabled: bool) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    if let Some(a) = list.iter_mut().find(|a| a.id == id) {
        a.enabled = enabled;
    }
    accounts::save_accounts(&dir, &list)?;
    Ok(list)
}

// ---------------------------------------------------------------------------
// 签到
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn checkin_one(app: tauri::AppHandle, id: String) -> Result<checkin::CheckinResult, String> {
    let dir = try_data_dir(&app)?;
    let list = accounts::load_accounts(&dir);
    let mut account = list
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| "账号不存在".to_string())?;
    // 临近过期先尝试续签（best-effort）
    let _ = crate::refresh::refresh_account(&dir, &mut account).await;
    let result = checkin::do_checkin(&account).await;
    crate::logs::push(&account.name, result.success, format!("签到：{}", result.message));
    Ok(result)
}

#[tauri::command]
pub async fn checkin_all(app: tauri::AppHandle) -> Result<Vec<checkin::CheckinResult>, String> {
    let dir = try_data_dir(&app)?;
    let list = accounts::load_accounts(&dir);
    let mut results = Vec::new();
    for mut account in list {
        let _ = crate::refresh::refresh_account(&dir, &mut account).await;
        let result = checkin::do_checkin(&account).await;
        crate::logs::push(&account.name, result.success, format!("签到：{}", result.message));
        results.push(result);
    }
    Ok(results)
}

#[tauri::command]
pub async fn checkin_status(app: tauri::AppHandle) -> Result<Vec<(String, serde_json::Value)>, String> {
    let dir = try_data_dir(&app)?;
    let list = accounts::load_accounts(&dir);
    let mut out = Vec::new();
    for account in &list {
        if let Some(data) = checkin::query_status(account).await {
            out.push((account.id.clone(), data));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_logs() -> Result<Vec<crate::logs::LogEntry>, String> {
    Ok(crate::logs::entries())
}

#[tauri::command]
pub fn clear_logs() -> Result<(), String> {
    crate::logs::clear();
    Ok(())
}

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    Ok(settings(&app))
}

#[tauri::command]
pub fn save_settings(app: tauri::AppHandle, settings: Settings) -> Result<Settings, String> {
    let dir = try_data_dir(&app)?;
    accounts::save_settings(&dir, &settings)?;
    Ok(settings)
}

#[tauri::command]
pub fn gateway_status() -> crate::gateway::GatewayStatus {
    crate::gateway::status()
}

// ---------------------------------------------------------------------------
// 浏览器登录（OAuth）
// ---------------------------------------------------------------------------

/// 「浏览器登录」第一步：申请 state + 授权链接。
///
/// `host` 省略时默认国内版 `https://api.trae.cn`；国际版传 `https://api.trae.ai`。
/// 端点需抓包逆向确认后才可用（见 [`crate::oauth`] 顶部说明）。
#[tauri::command]
pub async fn oauth_start(host: Option<String>) -> Result<oauth::OAuthStart, String> {
    oauth::start(host).await
}

/// 「浏览器登录」第二步：轮询一次授权结果。
///
/// 返回 `done=false` 表示用户还没完成授权（继续轮询即可，**不是错误**）；
/// `done=true` 且带 `token` 表示授权完成，`nickname`/`phone`/`uid` 一并带回。
#[tauri::command]
pub async fn oauth_poll(login_id: String) -> Result<oauth::OAuthPoll, String> {
    oauth::poll(&login_id).await
}

/// 在系统默认浏览器打开链接（用于打开浏览器登录的授权页）
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    oauth::open_in_browser(&url)
}

// ---------------------------------------------------------------------------
// 池化网关「自动接管」（用户已手动添加自定义模型 + 助手自动选中）
// ---------------------------------------------------------------------------

/// 接管结果。`matched=false` 表示本机尚未检测到用户添加的网关模型（需用户先手动添加）。
#[derive(serde::Serialize, Clone)]
pub struct TakeoverOutcome {
    /// 本次是否自动退出了 TraeWork 后再写入选中并重启。
    pub restarted: bool,
    /// 本机是否已有指向网关的自定义模型（用户是否已手动添加）。
    pub matched: bool,
    /// 被自动选中的 agent 入口列表。
    pub labels: Vec<String>,
    /// 本机网关地址（即用户需填的 Base URL）。
    pub base_url: String,
    pub message: String,
}

/// 自动接管：把用户已添加的网关模型选为各 agent 入口的当前模型。
///
/// 先做一次**只读**预检（即便 TraeWork 运行中也能判断用户是否已添加）；若已添加，
/// 则在「TraeWork 未运行时」写入选中记录（必要时自动退出 → 写入 → 重启）。
/// 若用户尚未在 TraeWork 添加自定义模型，返回 `matched=false` 与引导文案，不报错。
#[tauri::command]
pub fn takeover_model(app: tauri::AppHandle) -> Result<TakeoverOutcome, String> {
    let dir = try_data_dir(&app)?;
    let port = settings(&app).gateway_port;
    let base_url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    // 只读预检：用户是否已添加指向本机网关的自定义模型
    let matched_models = crate::inject::find_gateway_models(port);
    let mut settings = settings(&app);
    if matched_models.is_empty() {
        // 未添加：保留偏好（下次添加后自动生效），返回引导文案
        settings.injection_enabled = true;
        accounts::save_settings(&dir, &settings)?;
        return Ok(TakeoverOutcome {
            restarted: false,
            matched: false,
            labels: vec![],
            base_url: base_url.clone(),
            message: format!(
                "未在 TraeWork 检测到指向本机网关的自定义模型（Base URL 应为 {base_url}）。请先：\
                 TraeWork 设置 → 模型 → 添加自定义模型（OpenAI 兼容），Base URL 填上面的地址并保存；\
                 之后重新开启本开关，助手会自动选中它。"
            ),
        });
    }
    // 已添加：在 TraeWork 未运行时写入选中（必要时自动退出 → 写入 → 重启）
    let (res, restarted) =
        crate::inject::with_trae_restart(|| crate::inject::select_gateway_model(port))?;
    let (_count, labels) = res;
    settings.injection_enabled = true;
    accounts::save_settings(&dir, &settings)?;
    Ok(TakeoverOutcome {
        restarted,
        matched: true,
        labels: labels.clone(),
        base_url: base_url.clone(),
        message: format!(
            "已为 {} 个 agent 入口自动选中你添加的网关模型（{}）。{}",
            labels.len(),
            base_url,
            if restarted {
                "已自动退出并重启 TraeWork，重新打开即可见为已选中。"
            } else {
                "重启 TraeWork 后，在模型选择器即可见为已选中。"
            }
        ),
    })
}

/// 当前接管状态 + TraeWork 运行态。
#[tauri::command]
pub fn takeover_status() -> crate::inject::TakeoverStatus {
    let port = crate::gateway::status().port;
    crate::inject::takeover_status(port)
}

/// 解除接管：仅清空助手写入的「网关模型选中」，绝不删除用户自己的自定义模型。
/// 若检测到 TraeWork 运行中，会**自动退出 → 清空选中 → 再启动**。
#[tauri::command]
pub fn release_takeover(app: tauri::AppHandle) -> Result<usize, String> {
    let dir = try_data_dir(&app)?;
    let port = settings(&app).gateway_port;
    let (n, _restarted) = crate::inject::with_trae_restart(|| crate::inject::clear_gateway_selection(port))?;
    let mut settings = settings(&app);
    settings.injection_enabled = false;
    accounts::save_settings(&dir, &settings)?;
    Ok(n)
}
