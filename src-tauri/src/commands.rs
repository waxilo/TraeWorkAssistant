//! Tauri 命令层：账号管理、签到、设置、智能接管。

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
pub async fn checkin_status(app: tauri::AppHandle) -> Result<Vec<checkin::AccountStatus>, String> {
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    let mut out = Vec::new();
    let mut dirty = false;
    for account in list.iter_mut() {
        // ① 今日是否已签到（签到状态接口）
        let status = checkin::query_status(account).await;
        // ② 账号已有积分（entitlement 用量接口）；拉到就落盘，供界面展示与接管选号
        if let Some(u) = checkin::fetch_ent_usage(account).await {
            let next = accounts::CreditSnapshot::now(u.remaining, u.unlimited, u.earliest_expiry_ms);
            let prev = account.credit_snapshot.as_ref();
            let changed = prev.map(|p| (p.credits, p.unlimited, p.earliest_expiry_ms))
                != Some((next.credits, next.unlimited, next.earliest_expiry_ms));
            if changed {
                account.credit_snapshot = Some(next);
                dirty = true;
            }
        }
        // 拉不到（限流 9074 / 掉线）时沿用上次已知的积分，而不是把已有数字抹成未知
        let snap = account.credit_snapshot.as_ref();
        out.push(checkin::AccountStatus {
            id: account.id.clone(),
            checked_in: status.as_ref().map(checkin::is_checked_in).unwrap_or(false),
            message: status.as_ref().map(checkin::message_of).unwrap_or_default(),
            credits: snap.and_then(|s| s.credits),
            unlimited: snap.map(|s| s.unlimited).unwrap_or(false),
        });
    }
    if dirty {
        let _ = accounts::save_accounts(&dir, &list);
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
// 智能接管：本地反代 + TraeWork 端点覆盖（一体开关）
// ---------------------------------------------------------------------------

/// 智能接管的完整状态（供界面一次性渲染）。
#[derive(serde::Serialize, Clone)]
pub struct TakeoverStatus {
    /// 用户是否开启了智能接管（对应 `Settings.takeover_enabled`）。
    pub enabled: bool,
    /// 本地反代监听端口。
    pub port: u16,
    /// 本地反代是否正在监听。
    pub proxy_active: bool,
    /// 反代启动失败原因（端口占用等）。
    pub proxy_error: Option<String>,
    /// TraeWork 当前是否在运行。
    pub trae_running: bool,
    /// 是否找到 TraeWork 安装目录（找不到则本机不支持接管）。
    pub supported: bool,
    pub app_dir: Option<String>,
    /// 端点覆盖文件当前是否存在。
    pub installed: bool,
    /// 覆盖文件是否由本助手写入（用户自己写的文件为 `false`，不会被清理）。
    pub ours: bool,
    /// TraeWork 安装目录是否可写。
    pub writable: bool,
    /// 本机反代基址（即覆盖写入的 `remote.domain`）。
    pub http_base: String,
    /// 原始上游（取自 `product.json`，便于界面展示与排障）。
    pub upstream_http: Option<String>,
    pub upstream_ws: Option<String>,
    /// 端点覆盖租约是否新鲜（反代的心跳）。
    pub lease_fresh: bool,
    pub message: String,
}

fn build_status(dir: &std::path::Path, s: &Settings) -> TakeoverStatus {
    let http_base = format!("http://127.0.0.1:{}", s.takeover_port);
    let ep = crate::endpoint::status(dir, &http_base);
    let px = crate::proxy::status();
    let message = if !ep.supported {
        "未找到 TraeWork 安装目录，本机不支持「智能接管」。".to_string()
    } else if ep.installed && !ep.ours {
        "检测到 product.desktop.local.json，但并非本助手写入——助手不会接管也不会清理它。".to_string()
    } else if s.takeover_enabled && px.active && ep.installed {
        "接管已生效：TraeWork 的模型/会话请求将经本机反代按账号池转发。".to_string()
    } else if s.takeover_enabled && px.active {
        "本地反代已就绪，但端点覆盖尚未写入。重新开启开关即可完成接管。".to_string()
    } else if s.takeover_enabled {
        "已开启接管，但本地反代未在监听——请检查端口是否被占用。".to_string()
    } else {
        "未开启智能接管。".to_string()
    };
    TakeoverStatus {
        enabled: s.takeover_enabled,
        port: s.takeover_port,
        proxy_active: px.active,
        proxy_error: px.error,
        trae_running: crate::endpoint::is_trae_running(),
        supported: ep.supported,
        app_dir: ep.app_dir,
        installed: ep.installed,
        ours: ep.ours,
        writable: ep.writable,
        http_base,
        upstream_http: ep.upstream_http,
        upstream_ws: ep.upstream_ws,
        lease_fresh: ep.lease_fresh,
        message,
    }
}

/// 只读：当前智能接管状态（不修改任何文件）。
#[tauri::command]
pub fn takeover_status(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    let s = accounts::load_settings(&dir);
    Ok(build_status(&dir, &s))
}

/// 开启智能接管：启用本地反代 → 确认已监听 → 写入 TraeWork 端点覆盖并重启它。
///
/// 顺序是**刻意**的：必须先确认反代在监听，才允许写覆盖，否则 TraeWork 全量请求会打到死端口。
/// 退出/重启 TraeWork 可能耗时到 20s，故整体走 `spawn_blocking`，避免同步命令冻结 UI。
#[tauri::command]
pub async fn takeover_enable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || enable_blocking(dir))
        .await
        .map_err(|e| format!("开启接管任务异常：{e}"))?
}

fn enable_blocking(dir: PathBuf) -> Result<TakeoverStatus, String> {
    let mut s = accounts::load_settings(&dir);
    let port = s.takeover_port;

    // 1) 先开启：让反代线程开始监听（反代只在 takeover_enabled 时绑定端口）
    if !s.takeover_enabled {
        s.takeover_enabled = true;
        accounts::save_settings(&dir, &s)?;
    }

    // 2) 等反代真正就绪（最多 ~3s）；不就绪则回滚，绝不留下指向死端口的覆盖
    let mut ready = false;
    for _ in 0..30 {
        let st = crate::proxy::status();
        if st.active && st.port == port {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !ready {
        let mut s2 = accounts::load_settings(&dir);
        s2.takeover_enabled = false;
        accounts::save_settings(&dir, &s2)?;
        let err = crate::proxy::status().error.unwrap_or_default();
        crate::journal::append(
            &dir,
            "proxy_error",
            &format!(
                "本地反代未能在 127.0.0.1:{port} 监听{}，已回滚，TraeWork 未被改动",
                if err.is_empty() { String::new() } else { format!("（{err}）") }
            ),
        );
        return Err(format!(
            "本地反代未能在 127.0.0.1:{port} 监听{}，已回滚，未改动 TraeWork。",
            if err.is_empty() { String::new() } else { format!("（{err}）") }
        ));
    }

    // 3) 写端点覆盖 + 重启 TraeWork 使其生效
    let http_base = format!("http://127.0.0.1:{port}");
    let dir2 = dir.clone();
    let (_status, restarted) =
        crate::endpoint::with_trae_restart(|| crate::endpoint::install(&dir2, &http_base, None))?;
    if restarted {
        crate::journal::append(
            &dir,
            "restart_trae",
            "为让端点覆盖生效，已退出并重启 TraeWork（未保存的输入请自行确认）",
        );
    }

    let s3 = accounts::load_settings(&dir);
    Ok(build_status(&dir, &s3))
}

/// 关闭智能接管：删除端点覆盖并重启 TraeWork 恢复官方直连，然后停掉本地反代。
#[tauri::command]
pub async fn takeover_disable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        // 先恢复 TraeWork（覆盖不在了，「指向死端口」的风险即消失），再停反代
        let dir2 = dir.clone();
        let (_removed, restarted) =
            crate::endpoint::with_trae_restart(|| crate::endpoint::uninstall(&dir2))?;
        let mut s = accounts::load_settings(&dir);
        s.takeover_enabled = false;
        accounts::save_settings(&dir, &s)?;
        if restarted {
            crate::journal::append(
                &dir,
                "restart_trae",
                "为恢复官方直连已重启 TraeWork",
            );
        }
        Ok(build_status(&dir, &s))
    })
    .await
    .map_err(|e| format!("关闭接管任务异常：{e}"))?
}

// ---------------------------------------------------------------------------
// 接管动态（journal）
// ---------------------------------------------------------------------------

/// 读取接管动态（最新在前）：谁在什么时候用了哪个账号、有没有被限流换号、代理有没有报错。
#[tauri::command]
pub fn takeover_events(app: tauri::AppHandle) -> Result<Vec<crate::journal::JournalEvent>, String> {
    Ok(crate::journal::read(&try_data_dir(&app)?))
}

/// 清空接管动态（不可恢复）。
#[tauri::command]
pub fn clear_takeover_events(app: tauri::AppHandle) -> Result<(), String> {
    crate::journal::clear(&try_data_dir(&app)?);
    Ok(())
}
