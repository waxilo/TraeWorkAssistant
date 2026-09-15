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

/// 按需回源补全账号资料（占位名 / 脱敏手机号 → 服务端 `ScreenName` / `NonPlainTextMobile`），
/// 返回更新后的列表。
///
/// 只对资料不全的账号发请求，失败静默保留原值 —— 见 [`crate::profile`]。
#[tauri::command]
pub async fn refresh_account_profiles(app: tauri::AppHandle) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    Ok(crate::profile::sync_profiles(&dir).await)
}

/// 导入账号（去重后追加）。
///
/// 前端只负责「它知道的东西」——不该也不需要在浏览器里编 `id`：候选记录一律先过
/// [`accounts::normalize`] 补齐 `id` / `user_id` / `created_at`，再按手机号或 token 去重。
#[tauri::command]
pub fn import_accounts(app: tauri::AppHandle, accounts: Vec<Account>) -> Result<Vec<Account>, String> {
    let dir = try_data_dir(&app)?;
    let mut list = accounts::load_accounts(&dir);
    for mut a in accounts {
        accounts::normalize(&mut a);
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
    // 临近过期先续签（只在到期窗口内才发请求，见 `renew::needs_renew`）
    if let Some(out) = crate::renew::renew_if_needed(&dir, &mut account).await {
        crate::logs::push(&account.name, out.renewed, format!("续签：{}", out.message));
    }
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
        if let Some(out) = crate::renew::renew_if_needed(&dir, &mut account).await {
            crate::logs::push(&account.name, out.renewed, format!("续签：{}", out.message));
        }
        let result = checkin::do_checkin(&account).await;
        crate::logs::push(&account.name, result.success, format!("签到：{}", result.message));
        results.push(result);
    }
    Ok(results)
}

// ⚠️ 这里原来有个 `renew_accounts`（手动续签）命令。2026-09-15 按用户要求**整体删除**：
// 续签必须全自动，界面上不提供按钮，也就不需要一个「点一下才续」的后端入口。
// 自动路径完全覆盖它 —— 后台线程启动即巡、之后每 30 分钟一轮（`renew::spawn`），
// 并且每次签到之前都会顺手续一次（`checkin_one` / `checkin_all` / `scheduler::run_checkin`）。
// 想「立刻验证某个账号到底还能不能续签」，用真机探针（它是删掉按钮后唯一的手动手段）：
//     cargo test --lib -- --ignored --nocapture live_renew_probe

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
            earliest_expiry_ms: snap.and_then(|s| s.earliest_expiry_ms),
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
    /// `product.json` 里的端点是否**已指向本机反代**（= 改写仍生效）。
    pub installed: bool,
    /// `product.json` 是否带本助手标记（带标记才能一键精确还原）。
    pub ours: bool,
    /// TraeWork 安装目录是否可写。
    pub writable: bool,
    /// 本机反代端点基址（即改写写入的 `remote.domain`）：
    /// 恒为 `http://127.0.0.1:PORT`（免证书形态，唯一来源见 `endpoint::base_url`）。
    pub endpoint_base: String,
    /// 当前生效的接管规则（`proxy-rules.json`，热加载）。
    pub rules: crate::rules::Rules,
    /// TraeWork 主进程「免证书补丁」的状态 —— 端点能走明文的**唯一前提条件**。
    pub patch: crate::patch::PatchStatus,
    /// 原始上游（取自 `product.json`，便于界面展示与排障）。
    pub upstream_http: Option<String>,
    pub upstream_ws: Option<String>,
    /// 端点覆盖租约是否新鲜（反代的心跳）。
    pub lease_fresh: bool,
    pub message: String,
}

/// 组装对 UI 的状态。**`message` 只描述「现在挡在你面前的是什么」**，措辞按「谁知道得最准」分配：
///
/// - 文件层的事实（目录找不到 / 不可写 / 端点被谁改的）→ 用 `endpoint.rs` 探针给出的那句
///   （尤其「不可写」：权限位 / macOS「App 管理」TCC / 只读卷的成因只有探针分得清，且它带了处置办法）；
/// - 补丁层的状态 → 用 `patch.rs` 探针那句（它自带「怎么打补丁」这个下一步）；
/// - 运行层的事实（反代在不在监听）→ 只有这里知道，自己出话；
/// - 两边都知道的「反代就绪但端点没指过来」→ 说清楚**下一步做什么**，比任一侧的叙述都有用。
fn build_status(dir: &std::path::Path, s: &Settings) -> TakeoverStatus {
    let endpoint_base = crate::endpoint::base_url(s.takeover_port);
    let ep = crate::endpoint::status(dir, &endpoint_base);
    let px = crate::proxy::status();
    let patch = crate::patch::status();
    let rules = crate::rules::Rules::load(dir);

    let message = if !ep.supported {
        ep.message.clone()
    } else if !patch.patched {
        // **必须排在最前**：端点写的是明文 `http://`，而没打补丁的 TraeWork 会把它拼成
        // 非法 URL pattern、**启动即崩**。所以「打补丁」是此刻唯一的下一步 ——
        // 连「目录不可写」也要由它来说（`patch.message` 已含 TCC 指引），
        // 否则会被下面的端点话术抢答成一句与当下无关的话。
        patch.message.clone()
    } else if !ep.writable && !s.takeover_enabled {
        ep.message.clone()
    } else if ep.installed && !ep.ours {
        ep.message.clone()
    } else if s.takeover_enabled && !px.active {
        "已开启接管，但本地反代未在监听——请检查端口是否被占用。".to_string()
    } else if s.takeover_enabled && !ep.installed {
        "本地反代已就绪，但 TraeWork 的端点还没指过来（TraeWork 升级后会丢失）。重新开启一次开关即可。"
            .to_string()
    } else if s.takeover_enabled {
        "接管已生效：端点走明文回环，不需要任何证书。".to_string()
    } else {
        ep.message.clone()
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
        endpoint_base,
        rules,
        patch,
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

/// 开启智能接管：**先给 TraeWork 打免证书补丁** → 只读预检 → 预检端口 →
/// 启用本地反代 → 确认已监听 → 改写 TraeWork 端点并重启它。
///
/// 顺序是刻意排的：前几步都发生在「动任何开关之前」，任何一步不通过，
/// 开关 / 反代 / TraeWork 进程**一个都没动** —— 不留半开状态。
#[tauri::command]
pub async fn takeover_enable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || enable_endpoint(dir))
        .await
        .map_err(|e| format!("开启接管任务异常：{e}"))?
}

/// 端口预检。被占就**当场**说清「谁占着、该换到哪个」，绝不先把开关打开再回滚 ——
/// 回滚是对的，但用户拿到的只有一句 `Address already in use`，等于死路。
/// 已经在监听同一端口的（例如重复点开关）不算冲突，放行给后面的幂等逻辑。
fn ensure_port_free(port: u16) -> Result<(), String> {
    let st = crate::proxy::status();
    if st.active && st.port == port {
        return Ok(());
    }
    if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
        let hint = crate::portcheck::busy_hint(port);
        return Err(format!("端口 {port} {hint}。改好端口再开接管即可。"));
    }
    Ok(())
}

/// 等本地代理真正在监听（最多 ~3s）。不就绪则**回滚开关**并返回原因。
fn wait_proxy_ready(dir: &std::path::Path, port: u16) -> Result<(), String> {
    for _ in 0..30 {
        let st = crate::proxy::status();
        if st.active && st.port == port {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut s2 = accounts::load_settings(dir);
    s2.takeover_enabled = false;
    accounts::save_settings(dir, &s2)?;
    let err = crate::proxy::status().error.unwrap_or_default();
    let tail = if err.is_empty() { String::new() } else { format!("（{err}）") };
    crate::journal::append(
        dir,
        "proxy_error",
        &format!("本地代理未能在 127.0.0.1:{port} 监听{tail}，已回滚，TraeWork 未被改动"),
    );
    Err(format!("本地代理未能在 127.0.0.1:{port} 监听{tail}，已回滚，未改动 TraeWork。"))
}

/// 开启「端点改道」（改 TraeWork 的 `product.json`）—— **现在唯一的改道方式**。
///
/// 顺序是**刻意**的，前几步都发生在「动开关之前」：
/// 0. 先给 TraeWork 打闸门补丁 —— 这是「明文端点」能成立的前提。
///    `patch::apply` 自带 fail-closed 与逐字节还原，失败时开关/反代/进程**一个都没动**。
/// 1. **端点预检**——写进去的值必须能通过 TraeWork 自己的 URL pattern 校验，
///    否则它会启动即崩。不通过就当场报错。
/// 2. 端口预检：见 [`ensure_port_free`]。
/// 3. 必须先确认反代在监听，才允许改端点，否则 TraeWork 全量请求会打到死端口。
fn enable_endpoint(dir: PathBuf) -> Result<TakeoverStatus, String> {
    let mut s = accounts::load_settings(&dir);
    let port = s.takeover_port;
    let endpoint_base = crate::endpoint::base_url(port);

    // 0a) 把规则文件落地（已存在则不动）：让「现在到底用哪张表」随时可见可改。
    //     ⚠️ 这个动作原先挂在已移除的「经系统代理」开启流程里，不能跟着一起丢掉：
    //     接管一旦出问题，第一件要确认的就是「用的是哪张表」，而那时文件必须在。
    if let Err(e) = crate::rules::Rules::write_default_if_absent(&dir) {
        crate::journal::append(&dir, "rules_write", &format!("规则文件落地失败：{e}"));
    }

    // 0) **前置动作：先打补丁。**
    //    这一步必须在端点预检之前 —— 预检的不变量是「明文端点必须具备补丁后的闸门」，
    //    所以得先把闸门改对。`patch::apply` 自身是 fail-closed + 可逐字节还原的：
    //    版本不认识就返回 Err，此时**开关、反代、TraeWork 一个都没动**。
    let applied = match crate::patch::apply(&dir) {
        Ok(p) => p,
        Err(e) => {
            crate::journal::append(&dir, "patch_apply", &format!("打免证书补丁失败：{e}"));
            return Err(e);
        }
    };
    if !applied.patched {
        return Err(applied.message);
    }

    // 0b) 端点预检（只读）。写进去的值若会被 TraeWork 拼成非法 URL pattern，
    //     它会启动即崩 —— 所以这一步必须在**动开关之前**，失败就是「什么都没发生」。
    //     它还负责拦住「没打补丁却写了明文端点」这条最危险的路径。
    crate::endpoint::preflight(&endpoint_base)?;

    // 1) 预检端口
    ensure_port_free(port)?;

    // 2) 先开启：让反代线程开始监听（反代只在 takeover_enabled 时绑定端口）
    if !s.takeover_enabled {
        s.takeover_enabled = true;
        accounts::save_settings(&dir, &s)?;
    }

    // 3) 等反代真正就绪；不就绪则回滚，绝不留下指向死端口的覆盖
    wait_proxy_ready(&dir, port)?;

    // 4) 改写 product.json 的端点 + 重启 TraeWork 使其生效
    let dir2 = dir.clone();
    match crate::endpoint::with_trae_restart(|| crate::endpoint::install(&dir2, &endpoint_base)) {
        Ok((_status, restarted)) => {
            if restarted {
                crate::journal::append(
                    &dir,
                    "restart_trae",
                    "为让端点改写生效，已退出并重启 TraeWork（未保存的输入请自行确认）",
                );
            }
        }
        Err(e) => {
            // 走到这里说明闸门在写盘那一刻才拦住（正常应在第 0 步就拦住）。
            // 此时反代已起来、开关已打开，必须把开关回滚成关闭，不留半开状态。
            let mut s2 = accounts::load_settings(&dir);
            if s2.takeover_enabled {
                s2.takeover_enabled = false;
                let _ = accounts::save_settings(&dir, &s2);
            }
            return Err(e);
        }
    }

    let s3 = accounts::load_settings(&dir);
    Ok(build_status(&dir, &s3))
}

/// 关闭智能接管：还原 `product.json` + **自动还原 TraeWork 补丁** + 重启 TraeWork 恢复官方直连，
/// 再停本地反代。**关开关 = 把应用侧改过的东西全部还回去**。
///
/// 顺序不能反：先恢复 TraeWork（端点不再指向本机，「指向死端口」的风险即消失），再停反代 ——
/// 反过来会留下「反代已停、应用仍指向它」的死状态。
///
/// 端点与补丁在**同一次重启**里一起还原：先端点、后补丁。
/// 反过来则中途会出现「明文端点 + 未打补丁的应用」这个组合，那正是让它**启动即崩**的组合。
///
/// 顺带做一次**残留清扫**：老版本的「经系统代理接管」会把 TraeWork 的
/// `User/settings.json` 指到本机回环代理，而那条路已整体移除（本端点对 CONNECT 一律 405）。
/// 只要那个设置还在，TraeWork 就是整应用不可用 —— 所以关接管时确认一遍并清掉。
/// `traework::uninstall` 按**回环指纹**判定，绝不碰用户自己配的非环代理。
#[tauri::command]
pub async fn takeover_disable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let pre = accounts::load_settings(&dir);
        if crate::traework::applied(pre.takeover_port) {
            match crate::traework::uninstall(&dir) {
                Ok(msg) => crate::journal::append(&dir, "legacy_proxy_clear", &msg),
                Err(e) => crate::journal::append(
                    &dir,
                    "legacy_proxy_clear",
                    &format!("清理 TraeWork 遗留代理设置失败：{e}"),
                ),
            }
        }

        // 先恢复 TraeWork（端点不再指向本机，「指向死端口」的风险即消失），再停反代。
        // 补丁还原塞进**同一个 op** 里 —— 于是全程只重启一次 TraeWork，且中间态永远安全。
        let dir2 = dir.clone();
        let (_removed, restarted) = crate::endpoint::with_trae_restart(|| {
            // ① 端点必须回到官方：这是「应用可用」的硬前提，失败就整件事失败。
            let removed = crate::endpoint::uninstall(&dir2)?;
            // ② 再还原补丁（次序理由见函数文档）。
            //    ⚠️ 这一步**故意不算致命**：端点已经回官方、应用完全可用，只是它还带着补丁。
            //    若把它算成失败，用户会看到「点了关闭、开关又弹回去」—— 比「补丁没还原」难懂得多
            //    （而且下次开启接管会重新打，留下的补丁也不影响任何行为）。
            if let Err(e) = crate::patch::revert(&dir2) {
                crate::journal::append(
                    &dir2,
                    "patch_revert",
                    &format!(
                        "还原免证书补丁失败（端点已恢复官方直连、不影响使用，下次开启接管会重打）：{e}"
                    ),
                );
            }
            Ok(removed)
        })?;
        let mut s = accounts::load_settings(&dir);
        s.takeover_enabled = false;
        accounts::save_settings(&dir, &s)?;
        if restarted {
            crate::journal::append(
                &dir,
                "restart_trae",
                "为恢复官方直连已重启 TraeWork（端点改写与免证书补丁一并还原）",
            );
        }
        Ok(build_status(&dir, &s))
    })
    .await
    .map_err(|e| format!("关闭接管任务异常：{e}"))?
}

// ---------------------------------------------------------------------------
// TraeWork 主进程补丁（免证书的唯一前提）—— 生命周期已**并入「智能接管」开关**
//
// 补丁把 TraeWork 的 URL pattern 闸门从「只认 https」改成「认任何 scheme」，
// 于是本地端点可以走**明文回环**，自签 CA 与钥匙串那一整套都可以不要 ——
// 连同「把 CA 装进系统信任库」这个动作一起消失了（2026-09-15 按用户要求移除）。
//
// ⚠️ 2026-09-15 按用户要求：「还原补丁」与开关**合并**，不再有独立的打/还原命令与按钮：
//   · **开启接管** → `enable_endpoint()` 第 0 步 `patch::apply`（幂等）；
//   · **关闭接管** → `takeover_disable()` 恢复端点后**自动还原补丁**（best-effort，
//     与端点还原合并在**同一次重启**里完成）。
// 这么做的直接好处：不可能再出现「接管关了、TraeWork 还带着补丁」这种只靠人记住的状态；
// 也不会出现「关了开关又得手动点一次还原」的两步操作。
//
// 补丁改的是**别的应用**的**可执行文件**，所以那套保护一个都不能少：
// fail-closed（版本不认识就拒打）、逐字节还原、以及还原后的 sha256 指纹自证（见 `patch.rs`）。
// ---------------------------------------------------------------------------

/// 读接管规则（`proxy-rules.json`，热加载，不用重编译）。
#[tauri::command]
pub fn takeover_rules(app: tauri::AppHandle) -> Result<crate::rules::Rules, String> {
    let dir = try_data_dir(&app)?;
    Ok(crate::rules::Rules::load(&dir))
}

/// 写接管规则。**立即生效**（读侧 1s 缓存）。
#[tauri::command]
pub fn takeover_save_rules(
    app: tauri::AppHandle,
    rules: crate::rules::Rules,
) -> Result<crate::rules::Rules, String> {
    let dir = try_data_dir(&app)?;
    let path = crate::rules::Rules::path(&dir);
    let text = serde_json::to_string_pretty(&rules).map_err(|e| format!("序列化规则失败：{e}"))?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    crate::journal::append(
        &dir,
        "rules_save",
        &format!(
            "接管规则已更新：观察模式={} · 换号前缀 {} 条 · 换 WS 凭据={} · 强制透传 {} 条",
            rules.observe_only,
            rules.swap_http_prefixes.len(),
            rules.swap_ws,
            rules.never_swap_prefixes.len()
        ),
    );
    // 绕过 1s 缓存，让调用方立刻看到新值（界面需要立即回显）
    crate::rules::invalidate();
    Ok(crate::rules::Rules::load(&dir))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机诊断：把「开启接管」这条路径上所有会说话的东西一次性打出来 ——
    /// 状态、端点基址、闸门判定、证书与系统信任情况。
    ///
    /// 跑法：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture dump_takeover_enable_on_this_machine
    /// ```
    ///
    /// **默认只读**：不动 `product.json`、不动开关、不重启 TraeWork。要在本机真跑一遍
    /// 完整路径（会改写端点并重启 TraeWork），必须显式加环境变量：
    /// ```text
    /// TWA_REAL_TAKEOVER=1 cargo test --lib -- --ignored --nocapture dump_takeover_enable_on_this_machine
    /// ```
    #[test]
    #[ignore = "真机诊断：读真实配置；只有设 TWA_REAL_TAKEOVER=1 才会真的改写端点并重启 TraeWork"]
    fn dump_takeover_enable_on_this_machine() {
        let Some(base) = dirs::data_dir() else {
            eprintln!("[skip] 无法定位系统数据目录");
            return;
        };
        let dir = base.join("cn.traework.assistant");
        let s = accounts::load_settings(&dir);
        let endpoint_base = crate::endpoint::base_url(s.takeover_port);

        println!("\n===== 真机「开启接管」诊断 =====");
        println!("数据目录  : {}", dir.display());
        println!(
            "开关/端口 : takeover_enabled={} port={}",
            s.takeover_enabled, s.takeover_port
        );
        println!("端点基址  : {endpoint_base}");
        println!("product   : {:?}", crate::endpoint::product_path());

        // 明文端点能不能过闸，取决于**当前**补丁状态（这是那条双向不变量）。
        println!("\n--- ① 端点闸门 ---");
        println!("preflight(当前端点) = {:?}", crate::endpoint::preflight(&endpoint_base));

        // 免证书补丁：明文模式的前提
        println!("\n--- ①b 免证书补丁 ---");
        let p = crate::patch::status();
        println!(
            "supported={} patched={} recognized={} writable={} gates={} identity_patterns={}",
            p.supported, p.patched, p.recognized, p.writable, p.gates, p.identity_patterns
        );
        println!("target    : {:?}", p.target);
        println!("message   : {}", p.message);

        // 证书与系统信任那一段已随「证书模式 / 经系统代理」一起移除 ——
        // 现在本机一个证书都不需要（端点走明文回环）。

        println!("\n--- ③ 当前状态（界面看到的那一份）---");
        let st = build_status(&dir, &accounts::load_settings(&dir));
        println!("proxy_active={} installed={} ours={} writable={}", st.proxy_active, st.installed, st.ours, st.writable);
        println!("message   : {}", st.message);

        if std::env::var("TWA_REAL_TAKEOVER").as_deref() != Ok("1") {
            println!("\n（只读模式：未开启接管。要真跑一遍请设 TWA_REAL_TAKEOVER=1）");
            return;
        }

        // ── 真跑：会改写 product.json 并重启 TraeWork ────────────────
        println!("\n--- ④ 真跑 enable_endpoint（会改写端点 + 重启 TraeWork）---");
        match enable_endpoint(dir.clone()) {
            Ok(st) => println!("成功：{}", st.message),
            Err(e) => println!("返回错误：{e}"),
        }
        let after = build_status(&dir, &accounts::load_settings(&dir));
        println!(
            "收尾：enabled={} proxy_active={} installed={}",
            after.enabled, after.proxy_active, after.installed
        );
        println!("（要还原请调用 takeover_disable，或重启助手让它自愈）");
    }

    // -----------------------------------------------------------------------
    // 免证书模式（A′）真机驱动：不依赖界面，直接走生产代码路径
    // -----------------------------------------------------------------------

    fn real_data_dir() -> Option<PathBuf> {
        dirs::data_dir().map(|b| b.join("cn.traework.assistant"))
    }

    /// 只读报告：补丁状态 + 端点模式 + 接管状态。**不动任何文件。**
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_takeover_report
    /// ```
    #[test]
    #[ignore = "真机只读报告"]
    fn live_takeover_report() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let s = accounts::load_settings(&dir);
        let p = crate::patch::status();
        let st = build_status(&dir, &s);
        println!("\n===== 接管 / 补丁 现状 =====");
        println!("数据目录    : {}", dir.display());
        println!(
            "开关        : enabled={} port={}",
            s.takeover_enabled, s.takeover_port
        );
        println!("白名单      : {:?}", s.billing_account_ids);
        println!("端点基址    : {}", st.endpoint_base);
        println!("反代        : active={} error={:?}", st.proxy_active, st.proxy_error);
        println!("端点改写    : installed={} ours={}", st.installed, st.ours);
        println!("TraeWork    : running={}", st.trae_running);
        println!(
            "补丁        : supported={} patched={} recognized={} writable={} gates={} identity_patterns={}",
            p.supported, p.patched, p.recognized, p.writable, p.gates, p.identity_patterns
        );
        println!("补丁目标    : {:?}", p.target);
        println!("补丁说明    : {}", p.message);
        println!("界面那句话  : {}", st.message);
        println!("上游        : http={:?} ws={:?}", st.upstream_http, st.upstream_ws);
    }

    /// 真机：给 TraeWork 打「免证书补丁」。**只改 `out/main.js`**，不碰开关、不重启进程。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_patch_apply
    /// ```
    ///
    /// 故意写成「直接调 `patch::apply`」而不是走界面：这是**生产用的同一段代码**，
    /// 而且没有 Tauri 上下文也能跑，便于在真机上把补丁这一步单独验证干净。
    #[test]
    #[ignore = "真机：会修改 TraeWork 的 out/main.js（可逐字节还原）"]
    fn live_patch_apply() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let before = crate::patch::status();
        println!("\n===== 打补丁前 =====");
        println!("{}", before.message);
        assert!(before.supported, "本机不支持：{}", before.message);
        // fail-safe：版本不认识就到此为止，绝不写盘
        assert!(before.recognized, "{}", before.message);
        if !before.writable {
            // 正常现象：`cargo test` 跑在**终端/工具**的进程里，macOS 的「App 管理」TCC
            // 只授权给过用户点头的 App。要真打补丁请从助手本体走
            // （免证书模式下「开启接管」会自动调用同一段 `patch::apply`）。
            eprintln!("\n[skip] 当前进程无权重写 TraeWork 包：{}", before.message);
            eprintln!("       这不是 bug —— 用助手本体开「免证书模式」即可（它走的正是这段代码）。");
            return;
        }

        if before.patched {
            println!("已经打过补丁，跳过（幂等）");
            return;
        }
        let after = crate::patch::apply(&dir).expect("打补丁失败");
        assert!(after.patched, "打完补丁后状态仍不是 patched：{}", after.message);
        println!("\n===== 打补丁后 =====");
        println!("{}", after.message);
        println!("指纹记录 : {:?}", dir.join("patch_main_js.json"));
    }

    /// 真机：还原补丁（会先确保接管已关闭，因为明文端点遇上未打补丁的应用会让它启动即崩）。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_patch_revert
    /// ```
    #[test]
    #[ignore = "真机：还原 out/main.js；若接管开着会先还原端点并重启 TraeWork"]
    fn live_patch_revert() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let mut s = accounts::load_settings(&dir);
        if s.takeover_enabled {
            println!("接管开着，先还原端点并重启 TraeWork");
            let dir2 = dir.clone();
            crate::endpoint::with_trae_restart(|| crate::endpoint::uninstall(&dir2))
                .expect("还原端点失败");
            s.takeover_enabled = false;
            accounts::save_settings(&dir, &s).expect("回写设置失败");
        }
        let changed = crate::patch::revert(&dir).expect("还原补丁失败");
        let st = crate::patch::status();
        println!("\n还原动作 : {}", if changed { "已还原" } else { "无需还原" });
        println!("当前形态 : patched={} recognized={}", st.patched, st.recognized);
        println!("{}", st.message);
    }

    // -----------------------------------------------------------------------
    // 真机驱动：不依赖界面，直接走生产代码路径
    //
    // 真机排障时往往只能对着终端，所以这些测试把「点按钮」换成了「敲命令」。
    // ⚠️ 「经系统代理接管」与「证书模式」那一组（`live_ca_install` /
    // `live_proxy_route_on` / `live_proxy_route_off`）已随两条路一起删除。
    // -----------------------------------------------------------------------

    /// 真机：把本机 TraeWork 的登录账号导入账号池。
    ///
    /// **只读 TraeWork 的文件，只写助手自己的 `accounts.json`** —— 不碰应用包、不碰网络设置。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_pool_import_local
    /// ```
    #[test]
    #[ignore = "真机：读本机登录态并写入助手 accounts.json"]
    fn live_pool_import_local() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let mut list = accounts::load_accounts(&dir);
        let before = list.len();
        println!("\n===== 导入本机登录态到账号池 =====");
        println!("数据目录    : {}", dir.display());

        for cand in crate::trae_auth::discover_local_accounts() {
            let mut a = Account::from(cand);
            accounts::normalize(&mut a);
            if accounts::contains_equivalent(&list, &a) {
                println!("  跳过（已在池里）：{}", a.name);
                continue;
            }
            println!(
                "  导入：name={} phone={} region={:?} token={} refresh={}",
                a.name,
                a.phone.clone().unwrap_or_else(|| "-".into()),
                a.region,
                if a.token.is_empty() { "无" } else { "有" },
                if a.refresh_token.is_some() { "有" } else { "无" }
            );
            list.push(a);
        }

        if list.len() != before {
            accounts::save_accounts(&dir, &list).expect("写入 accounts.json 失败");
        }
        println!("\n池内账号    : {} 个（本次新增 {}）", list.len(), list.len() - before);
        if list.len() == before && before == 0 {
            println!("⚠️ 一个都没扫到：TraeWork 是否已登录？（登录态在它的 User/globalStorage 里）");
        }
    }

}
