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

/// 账号的 API host（账号数据优先，否则默认 api.trae.cn）
pub fn account_host(a: &Account) -> String {
    a.host
        .clone()
        .unwrap_or_else(|| "https://api.trae.cn".into())
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
        // 按 token 去重；已存在则更新
        if let Some(existing) = list.iter_mut().find(|x| x.token == a.token) {
            *existing = a;
        } else {
            list.push(a);
        }
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

/// 从指定的登录态文件（storage.json）导入账号（用于「添加新账号 → 导入外部登录态」）。
#[tauri::command]
pub fn import_from_file(app: tauri::AppHandle, path: String) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    if let Some(tla) = trae_auth::parse_local_account(&std::path::Path::new(&path)) {
        let a: Account = tla.into();
        if let Some(existing) = list.iter_mut().find(|x| x.token == a.token) {
            *existing = a;
        } else {
            list.push(a);
        }
        accounts::save_accounts(&dir, &list)?;
        Ok(list)
    } else {
        Err("无法解析该登录态文件：不是有效的 TraeWork storage.json，或文件已失效".into())
    }
}

/// 手动添加账号：粘贴 token（+ host/name/region），不依赖本机安装。
#[tauri::command]
pub fn add_manual_account(
    app: tauri::AppHandle,
    name: Option<String>,
    host: Option<String>,
    token: String,
    region: Option<String>,
) -> Result<Vec<Account>, String> {
    if token.trim().len() < 40 {
        return Err("token 过短，请粘贴完整的登录令牌".into());
    }
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    let account = Account {
        id: uuid::Uuid::new_v4().to_string(),
        name: name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "手动导入账号".into()),
        phone: None,
        region,
        user_id: None,
        token: token.trim().to_string(),
        refresh_token: None,
        host: host.filter(|h| !h.trim().is_empty()),
        expires_at: None,
        refresh_expires_at: None,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        enabled: true,
    };
    if let Some(existing) = list.iter_mut().find(|x| x.token == account.token) {
        *existing = account;
    } else {
        list.push(account);
    }
    accounts::save_accounts(&dir, &list)?;
    Ok(list)
}

/// 扫描本机 TraeWork 已登录账号，返回「可导入」列表（不含已在库里的）。
#[tauri::command]
pub fn discover_local(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let existing_tokens: Vec<String> =
        accounts::load_accounts(&dir).iter().map(|a| a.token.clone()).collect();
    Ok(trae_auth::discover_local_accounts()
        .into_iter()
        .map(Account::from)
        .filter(|a| !existing_tokens.contains(&a.token))
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
// 内置模型自动注入（写 state.vscdb → 本地池化网关）
// ---------------------------------------------------------------------------

/// 注入结果。`restarted` 表示本次是否自动退出了 TraeWork 后再写入并重启。
#[derive(serde::Serialize, Clone)]
pub struct InjectOutcome {
    pub needs_quit: bool,
    pub restarted: bool,
    pub injected_db_count: usize,
    pub labels: Vec<String>,
    pub message: String,
}

/// 自动注入「自定义模型」条目到各登录态 state.vscdb 并切选中模型。
/// 若检测到 TraeWork 运行中，会**自动退出 → 写入（关闭窗口，避免被覆盖）→ 再启动**。
#[tauri::command]
pub fn inject_model(app: tauri::AppHandle) -> Result<InjectOutcome, String> {
    let dir = try_data_dir(&app)?;
    let port = settings(&app).gateway_port;
    let (labels, restarted) = crate::inject::with_trae_restart(|| crate::inject::inject_all(port))?;
    // 记录注入状态，供启动自愈
    let mut settings = settings(&app);
    settings.injection_enabled = true;
    accounts::save_settings(&dir, &settings)?;
    Ok(InjectOutcome {
        needs_quit: false,
        restarted,
        injected_db_count: labels,
        labels: crate::inject::injection_status(port)
            .into_iter()
            .flat_map(|i| i.labels)
            .collect(),
        message: format!(
            "已向 {} 个 state.vscdb 注入「{}」并切为选中模型。{}",
            labels,
            crate::inject::INJECT_DISPLAY_NAME,
            if restarted { "已自动退出并重启 TraeWork。" } else { "仅需重启 TraeWork 即可使用。" }
        ),
    })
}

/// 当前注入状态 + TraeWork 运行态。
#[tauri::command]
pub fn injection_status() -> serde_json::Value {
    let port = crate::gateway::status().port;
    serde_json::json!({
        "trae_running": crate::inject::is_trae_running(),
        "entries": crate::inject::injection_status(port),
    })
}

/// 还原所有 state.vscdb 到注入前备份。
/// 若检测到 TraeWork 运行中，会**自动退出 → 还原 → 再启动**。
#[tauri::command]
pub fn revert_injection(app: tauri::AppHandle) -> Result<usize, String> {
    let (n, _restarted) = crate::inject::with_trae_restart(crate::inject::revert_all)?;
    let dir = try_data_dir(&app)?;
    let mut settings = settings(&app);
    settings.injection_enabled = false;
    accounts::save_settings(&dir, &settings)?;
    Ok(n)
}
