//! Tauri 命令层：账号管理、签到、设置、智能接管。

use crate::accounts::{self, Account, Settings};
use crate::checkin;
use crate::oauth;
use crate::trae_auth;
use tauri::Manager;
use std::path::{Path, PathBuf};

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

/// **单个**目标应用的接管状态。
///
/// 界面按这个数组渲染「接管哪些应用」的多选，以及每个应用自己那句「挡着你的话」。
#[derive(serde::Serialize, Clone)]
pub struct AppStatus {
    /// 稳定 id（macOS 下 = `.app` 名），也是设置里记录选择用的键。
    pub id: String,
    /// 显示名（当前与 id 相同：`.app` 名本来就够清楚，多一层映射只会多一处会漂的东西）。
    pub label: String,
    /// 应用包（macOS）/ 安装目录（Windows）—— 排障时要能一眼看到在改谁。
    pub bundle: String,
    pub app_dir: String,
    /// 是否在接管名单里（名单为空 = 全部 ⇒ 这里恒 `true`）。
    pub selected: bool,
    /// 当前是否在运行。
    pub running: bool,
    /// `product.json` 的端点是否已指向本机反代。
    pub installed: bool,
    /// 上述改写是不是本助手写的（只有带标记才敢还原）。
    pub ours: bool,
    /// 它的安装目录是否**真能写** —— 不能写就没有任何一步能成。
    pub writable: bool,
    /// 它自己的闸门补丁状态。
    pub patch: crate::patch::PatchStatus,
    pub upstream_http: Option<String>,
    pub upstream_ws: Option<String>,
    /// **只在有事要说时非空**（版本不认识 / 不可写 / 被别人改过 / 端点丢了）。
    /// 一切正常时是空串 —— 界面上一行文字都不该出现（见 `TakeoverPage` 的取向）。
    pub message: String,
}

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
    /// 本机反代端点基址（写进 `product.json` 的那个值）：
    /// 恒为 `http://127.0.0.1:PORT`（免证书形态，唯一来源见 `endpoint::base_url`）。
    pub endpoint_base: String,
    /// 当前生效的接管规则（`proxy-rules.json`，热加载）。
    pub rules: crate::rules::Rules,
    /// 端点覆盖租约是否新鲜（反代的心跳）。
    pub lease_fresh: bool,
    /// 本机发现到的**全部** Trae 应用（含未勾选的）—— 界面据此渲染「接管应用」多选。
    pub apps: Vec<AppStatus>,
    /// 接管名单里、但本机已经不存在的 id（应用卸载了 / 改名了）。
    pub missing_apps: Vec<String>,
    /// 总状态那句话（成功时也可以是陈述句；界面只在有东西挡路时才显示）。
    pub message: String,
}

/// 组装对 UI 的状态。**`message` 只描述「现在挡在你面前的是什么」**，措辞按「谁知道得最准」分配：
///
/// - 文件层的事实（目录找不到 / 不可写 / 端点被谁改的）→ 用 `endpoint.rs` 探针给出的那句
///   （尤其「不可写」：权限位 / macOS「App 管理」TCC / 只读卷的成因只有探针分得清，且它带了处置办法）；
/// - 补丁层的状态 → 用 `patch.rs` 探针那句（它自带「怎么打补丁」这个下一步）；
/// - 运行层的事实（反代在不在监听）→ 只有这里知道，自己出话。
///
/// ⚠️ 现在是**按应用**各给一句（`AppStatus::message`），顶层的 `message` 只是「最先要说的那句」。
/// 之所以要拆开：本机可能有两个 Trae shell，一个能接管、另一个版本不认识 ——
/// 合成一句话必然要说谎，用户也无从知道该点掉哪一个。
fn build_status(dir: &std::path::Path, s: &Settings) -> TakeoverStatus {
    let endpoint_base = crate::endpoint::base_url(s.takeover_port);
    let px = crate::proxy::status();
    let rules = crate::rules::Rules::load(dir);

    let targets = crate::target::discover();
    let apps: Vec<AppStatus> = targets
        .iter()
        .map(|t| {
            let ep = crate::endpoint::status(t, dir, &endpoint_base);
            let patch = crate::patch::status(t);
            let selected = crate::target::is_selected(&s.takeover_apps, &t.id);
            let message = app_message(s, &px, selected, &ep, &patch, &t.id);
            AppStatus {
                id: t.id.clone(),
                label: t.id.clone(),
                bundle: t.bundle.display().to_string(),
                app_dir: ep.app_dir.clone().unwrap_or_default(),
                selected,
                running: t.running(),
                installed: ep.installed,
                ours: ep.ours,
                writable: ep.writable,
                patch,
                upstream_http: ep.upstream_http,
                upstream_ws: ep.upstream_ws,
                message,
            }
        })
        .collect();

    let missing_apps = crate::target::missing(&s.takeover_apps);

    // 顶层那句 = 「最先要说的」：先挑真的挡着路的（被选中的应用里的第一条），
    // 都没有则给一句陈述 —— 界面只在有东西挡路时才把它显示出来（见 `TakeoverPage`）。
    let message = if apps.is_empty() {
        "本机没有发现可接管的 Trae 应用（在应用目录里找不到带 bootConfig 的 product.json）。"
            .to_string()
    } else if let Some(first) = apps.iter().find(|a| a.selected && !a.message.is_empty()) {
        first.message.clone()
    } else if !missing_apps.is_empty() {
        format!(
            "接管名单里的这些应用本机已不存在：{}。取消勾选即可（不影响其它应用）。",
            missing_apps.join("、")
        )
    } else if s.takeover_enabled {
        "接管已生效：端点走明文回环，不需要任何证书。".to_string()
    } else {
        "未接管：应用仍直连官方。".to_string()
    };

    TakeoverStatus {
        enabled: s.takeover_enabled,
        port: s.takeover_port,
        proxy_active: px.active,
        proxy_error: px.error,
        endpoint_base,
        rules,
        lease_fresh: crate::endpoint::lease_fresh(dir),
        apps,
        missing_apps,
        message,
    }
}

/// 某个应用此刻「挡着路的是什么」。**空串 = 没事要说**。
///
/// 顺序是刻意排的（与旧版单目标完全一致，只是现在按应用各算一遍）：
/// 1. 补丁打不上（版本不认识 / 目录不可写）**必须排最前** —— 端点写的是明文 `http://`，
///    而没打补丁的应用会把它拼成非法 URL pattern、**启动即崩**。
///    所以「打补丁」是此刻唯一的下一步，连「目录不可写」也由 `patch.message` 来说
///    （它已含 TCC 指引），否则会被端点话术抢答成一句与当下无关的话。
/// 2. 端点被**别人**改过 → 说清楚我们为什么不动它。
/// 3. 开着但反代没监听 / 端点没指过来 → 说下一步做什么。
fn app_message(
    s: &Settings,
    px: &crate::proxy::ProxyStatus,
    selected: bool,
    ep: &crate::endpoint::EndpointStatus,
    patch: &crate::patch::PatchStatus,
    id: &str,
) -> String {
    // 没被选中的应用不需要说任何话（用户明确不让接管它）
    if !selected {
        return String::new();
    }
    if !patch.patched {
        return patch.message.clone();
    }
    if ep.installed && !ep.ours {
        return format!(
            "「{id}」的端点已指向本机反代，但不是本助手改的——为免误伤，助手不会动它。"
        );
    }
    if s.takeover_enabled && !px.active {
        return format!("已开启接管，但本地反代未在监听，所以「{id}」用不上账号池——请检查端口是否被占用。");
    }
    if s.takeover_enabled && !ep.installed {
        return format!(
            "本地反代已就绪，但「{id}」的端点还没指过来（升级后会丢失）。重新开启一次开关即可。"
        );
    }
    String::new()
}

/// 只读：当前智能接管状态（不修改任何文件）。
#[tauri::command]
pub fn takeover_status(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    let s = accounts::load_settings(&dir);
    Ok(build_status(&dir, &s))
}

/// 开启智能接管：**先给名单里的应用逐个打免证书补丁** → 只读预检 → 预检端口 →
/// 启用本地反代 → 确认已监听 → 改写各应用端点并重启它们。
///
/// 顺序是刻意排的：前几步都发生在「动任何开关之前」，任何一步不通过，
/// 开关 / 反代 / 应用进程**一个都没动** —— 不留半开状态。
#[tauri::command]
pub async fn takeover_enable(app: tauri::AppHandle) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || enable_endpoint(dir))
        .await
        .map_err(|e| format!("开启接管任务异常：{e}"))?
}

/// 解析「这次要接管哪些应用」。
///
/// 名单为空 = 全部已发现（与「参与扣费的账号」`billing_account_ids` 同一套语义）。
/// 两种失败都**自带出路**：一个都没发现（本机没装 / 装在别处）、名单里的应用都不在了。
fn resolve_targets(s: &Settings) -> Result<Vec<crate::target::AppTarget>, String> {
    let all = crate::target::discover();
    if all.is_empty() {
        return Err(
            "本机没有发现可接管的 Trae 应用（在应用目录里找不到带 bootConfig 的 product.json），\
             没有可以改道的对象。"
                .to_string(),
        );
    }
    let targets = crate::target::select(&s.takeover_apps);
    if targets.is_empty() {
        return Err(format!(
            "接管名单里的应用本机都不存在了（{}）。请在接管页重新勾选要接管的应用。",
            crate::target::missing(&s.takeover_apps).join("、")
        ));
    }
    Ok(targets)
}

/// 把**本次刚打上的**补丁还回去（只在开启流程中途失败时调用）。
///
/// 为什么需要它：本模块的明文约定是「补丁的生命周期完全跟着开关走 ——
/// 不存在『接管关了、应用还带着补丁』这种只靠人记住的状态」。多目标之后出现了新窗口：
/// 第一个应用的补丁已经打上，而第二个应用在打补丁 / 预检 / 端口预检那一步失败。
/// 不回滚的话，开关还是关的、第一个应用却带着补丁 —— 正好落进那个被否掉的状态。
fn rollback_patches(dir: &Path, applied: &[crate::target::AppTarget]) {
    for t in applied {
        if let Err(e) = crate::patch::revert(dir, t) {
            crate::journal::append(
                dir,
                "patch_revert",
                &format!(
                    "回滚「{}」的免证书补丁失败（接管并未开启，不影响使用；下次开启会重打）：{e}",
                    t.id
                ),
            );
        }
    }
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
        &format!("本地代理未能在 127.0.0.1:{port} 监听{tail}，已回滚，各应用未被改动"),
    );
    Err(format!("本地代理未能在 127.0.0.1:{port} 监听{tail}，已回滚，未改动任何应用。"))
}

/// 开启「端点改道」（改各应用的 `product.json`）—— **现在唯一的改道方式**。
///
/// 顺序是**刻意**的，前几步都发生在「动开关之前」：
/// 0a. 只读前置检查：**每个**目标都得「版本认识 + 目录可写」，否则先在什么都不动时报错。
/// 0b. 逐个打闸门补丁 —— 这是「明文端点」能成立的前提。中途失败会把已打上的那些还回去
///     （见 [`rollback_patches`]），所以「关了开关却带着补丁」这个状态不会出现。
/// 0c. **端点预检**——写进去的值必须能通过**每个应用自己**的 URL pattern 校验，
///     否则它会启动即崩。不通过就当场报错。
/// 1. 端口预检：见 [`ensure_port_free`]。
/// 2. 必须先确认反代在监听，才允许改端点，否则应用的请求会打到死端口。
/// 3. 改写 + **只重启被接管的那几个应用**（`target::with_restart`）。
fn enable_endpoint(dir: PathBuf) -> Result<TakeoverStatus, String> {
    let mut s = accounts::load_settings(&dir);
    let port = s.takeover_port;
    let endpoint_base = crate::endpoint::base_url(port);
    let targets = resolve_targets(&s)?;

    // 0a) 把规则文件落地（已存在则不动）：让「现在到底用哪张表」随时可见可改。
    //     ⚠️ 这个动作原先挂在已移除的「经系统代理」开启流程里，不能跟着一起丢掉：
    //     接管一旦出问题，第一件要确认的就是「用的是哪张表」，而那时文件必须在。
    if let Err(e) = crate::rules::Rules::write_default_if_absent(&dir) {
        crate::journal::append(&dir, "rules_write", &format!("规则文件落地失败：{e}"));
    }

    // 0b) **只读**前置检查：版本不认识 / 目录不可写这类「必然失败」，要在动任何东西之前说。
    //     `patch::apply` 自己也是 fail-closed 的，但那已经是**写操作途中**了。
    for t in &targets {
        let p = crate::patch::status(t);
        if !p.recognized || !p.writable {
            return Err(p.message);
        }
    }

    // 0c) 打补丁。`patch::apply` 幂等，所以重复点开关不会重复写盘。
    let mut freshly_patched: Vec<crate::target::AppTarget> = Vec::new();
    for t in &targets {
        if crate::patch::is_patched(t) {
            continue;
        }
        match crate::patch::apply(&dir, t) {
            Ok(p) if p.patched => freshly_patched.push(t.clone()),
            Ok(p) => {
                rollback_patches(&dir, &freshly_patched);
                return Err(p.message);
            }
            Err(e) => {
                crate::journal::append(&dir, "patch_apply", &format!("打免证书补丁失败：{e}"));
                rollback_patches(&dir, &freshly_patched);
                return Err(e);
            }
        }
    }

    // 0d) 端点预检（只读）。写进去的值若会被某个应用拼成非法 URL pattern，它会启动即崩
    //     —— 所以这一步必须在**动开关之前**，失败就是「什么都没发生」。
    //     它还负责拦住「没打补丁却写了明文端点」这条最危险的路径。
    for t in &targets {
        if let Err(e) = crate::endpoint::preflight(t, &endpoint_base) {
            rollback_patches(&dir, &freshly_patched);
            return Err(e);
        }
    }

    // 1) 预检端口
    if let Err(e) = ensure_port_free(port) {
        rollback_patches(&dir, &freshly_patched);
        return Err(e);
    }

    // 2) 先开启：让反代线程开始监听（反代只在 takeover_enabled 时绑定端口）
    if !s.takeover_enabled {
        s.takeover_enabled = true;
        accounts::save_settings(&dir, &s)?;
    }

    // 3) 等反代真正就绪；不就绪则回滚，绝不留下指向死端口的覆盖
    wait_proxy_ready(&dir, port)?;

    // 4) 改写各应用的端点 + 重启它们使其生效
    let dir2 = dir.clone();
    let targets2 = targets.clone();
    match crate::target::with_restart(&targets, || {
        for t in &targets2 {
            crate::endpoint::install(t, &dir2, &endpoint_base)?;
        }
        Ok(())
    }) {
        Ok((_unit, restarted)) => {
            if !restarted.is_empty() {
                crate::journal::append(
                    &dir,
                    "restart_trae",
                    &format!(
                        "为让端点改写生效，已重启：{}（未保存的输入请自行确认）",
                        restarted.join("、")
                    ),
                );
            }
        }
        Err(e) => {
            // 走到这里说明闸门在写盘那一刻才拦住（正常应在第 0c 步就拦住）。
            // 此时反代已起来、开关已打开，必须把开关回滚成关闭，不留半开状态。
            let mut s2 = accounts::load_settings(&dir);
            if s2.takeover_enabled {
                s2.takeover_enabled = false;
                let _ = accounts::save_settings(&dir, &s2);
            }
            rollback_patches(&dir, &freshly_patched);
            return Err(e);
        }
    }

    // 5) 收尾：把**名单之外**的应用放下（它们可能刚被取消勾选，或上次接管留下的痕迹）
    let keep = crate::target::select(&accounts::load_settings(&dir).takeover_apps);
    crate::endpoint::sweep(&dir, &crate::target::discover(), &keep);

    let s3 = accounts::load_settings(&dir);
    Ok(build_status(&dir, &s3))
}

/// 关闭智能接管：还原各应用 `product.json` + **自动还原免证书补丁** + 重启它们恢复官方直连，
/// 再停本地反代。**关开关 = 把应用侧改过的东西全部还回去**。
///
/// 顺序不能反：先恢复应用（端点不再指向本机，「指向死端口」的风险即消失），再停反代 ——
/// 反过来会留下「反代已停、应用仍指向它」的死状态。
///
/// 端点与补丁在**同一次重启**里一起还原：先端点、后补丁。
/// 反过来则中途会出现「明文端点 + 未打补丁的应用」这个组合，那正是让它**启动即崩**的组合。
///
/// **只碰真的被我们动过的应用**（带标记的端点 / 带标记的补丁）—— 关闭接管不该去
/// 重启一个从头到尾没参与过的应用。
///
/// 顺带做一次**残留清扫**：老版本的「经系统代理接管」会把应用的 `User/settings.json`
/// 指到本机回环代理，而那条路已整体移除（本端点对 CONNECT 一律 405）。
/// 只要那个设置还在，整个应用就不可用 —— 所以关接管时**逐个应用**确认一遍并清掉。
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
                    &format!("清理遗留代理设置失败：{e}"),
                ),
            }
        }

        let touched: Vec<crate::target::AppTarget> = crate::target::discover()
            .into_iter()
            .filter(|t| crate::endpoint::is_ours(t) || crate::patch::is_patched(t))
            .collect();

        // 先恢复应用（端点不再指向本机，「指向死端口」的风险即消失），再停反代。
        // 补丁还原塞进**同一个 op** 里 —— 于是每个应用只重启一次，且中间态永远安全。
        let dir2 = dir.clone();
        let mut endpoint_failures: Vec<String> = Vec::new();
        let (_, restarted) = crate::target::with_restart(&touched, || {
            for t in &touched {
                // ① 端点必须回到官方：这是「应用可用」的硬前提，失败要如实上报。
                match crate::endpoint::uninstall(t, &dir2) {
                    Ok(_) => {}
                    Err(e) => {
                        // 端点没能还回去 ⇒ **绝不能**碰它的补丁（明文端点 + 未打补丁 = 启动即崩）
                        endpoint_failures.push(format!("{}：{e}", t.id));
                        continue;
                    }
                }
                // ② 再还原补丁（次序理由见函数文档）。
                //    ⚠️ 这一步**故意不算致命**：端点已经回官方、应用完全可用，只是它还带着补丁。
                //    若把它算成失败，用户会看到「点了关闭、开关又弹回去」—— 比「补丁没还原」难懂得多
                //    （而且下次开启接管会重新打，留下的补丁也不影响任何行为）。
                if let Err(e) = crate::patch::revert(&dir2, t) {
                    crate::journal::append(
                        &dir2,
                        "patch_revert",
                        &format!(
                            "还原「{}」的免证书补丁失败（端点已恢复官方直连、不影响使用，下次开启接管会重打）：{e}",
                            t.id
                        ),
                    );
                }
            }
            Ok(())
        })?;
        if !endpoint_failures.is_empty() {
            return Err(format!(
                "以下应用的端点没能还原：{}。它们仍指向本机反代，请重试或手工检查。",
                endpoint_failures.join("；")
            ));
        }

        let mut s = accounts::load_settings(&dir);
        s.takeover_enabled = false;
        accounts::save_settings(&dir, &s)?;
        if !restarted.is_empty() {
            crate::journal::append(
                &dir,
                "restart_trae",
                &format!(
                    "为恢复官方直连已重启：{}（端点改写与免证书补丁一并还原）",
                    restarted.join("、")
                ),
            );
        }
        Ok(build_status(&dir, &s))
    })
    .await
    .map_err(|e| format!("关闭接管任务异常：{e}"))?
}

/// 改「接管哪些应用」——**运行中也能改**（这是本命令存在的全部理由）。
///
/// 语义与界面一致：
/// - `ids` 为空 = **全部接管**（与「参与扣费的账号」同一套语义：空 = 没配置 = 全选）；
/// - 非空 = 只接管这些（本机不存在的 id 会被丢掉，不会写进设置）。
///
/// 行为：
/// - **未开启接管**时只记设置，一个文件都不动；
/// - **已开启**时做**增量协调**：新勾上的补丁+改道并重启它，取消勾选的还原+还补丁并重启它，
///   没变的应用**一个字节都不碰、也不重启**。
///
/// 失败时不静默：新勾上的那个如果版本不认识 / 目录不可写，命令直接返回它自己那句
/// （`patch.rs` 探针的话术，自带下一步），并且设置保持用户的选择 —— 于是接管页会把它
/// 那条问题显示出来，而不是让「勾上了却没生效」变成一个看不见的状态。
#[tauri::command]
pub async fn takeover_set_apps(
    app: tauri::AppHandle,
    ids: Vec<String>,
) -> Result<TakeoverStatus, String> {
    let dir = try_data_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || set_apps(dir, ids))
        .await
        .map_err(|e| format!("保存接管应用失败：{e}"))?
}

fn set_apps(dir: PathBuf, ids: Vec<String>) -> Result<TakeoverStatus, String> {
    let mut s = accounts::load_settings(&dir);

    // 只接受本机**真实存在**的 id（界面上勾的就是这些），并排序让设置文件稳定。
    let all = crate::target::discover();
    let mut ids: Vec<String> = ids
        .into_iter()
        .filter(|i| all.iter().any(|t| &t.id == i))
        .collect();
    ids.sort();
    ids.dedup();

    let before = crate::target::select(&s.takeover_apps);
    s.takeover_apps = ids.clone();
    accounts::save_settings(&dir, &s)?;
    let after = crate::target::select(&s.takeover_apps);

    if !s.takeover_enabled {
        // 没开接管：选择只记在设置里，等下次开启时生效
        return Ok(build_status(&dir, &accounts::load_settings(&dir)));
    }

    let added: Vec<crate::target::AppTarget> = after
        .iter()
        .filter(|t| !before.iter().any(|b| b.id == t.id))
        .cloned()
        .collect();
    let dropped: Vec<crate::target::AppTarget> = before
        .iter()
        .filter(|t| !after.iter().any(|a| a.id == t.id))
        .cloned()
        .collect();

    let endpoint_base = crate::endpoint::base_url(s.takeover_port);

    // ① 放下的：端点回官方 + 补丁还回去（次序见 `endpoint::sweep`）
    if !dropped.is_empty() {
        crate::endpoint::sweep(&dir, &dropped, &[]);
    }

    // ② 新勾上的：只读检查 → 打补丁 → 端点预检
    if !added.is_empty() {
        for t in &added {
            let p = crate::patch::status(t);
            if !p.recognized || !p.writable {
                return Err(p.message);
            }
        }
        let mut freshly: Vec<crate::target::AppTarget> = Vec::new();
        for t in &added {
            if crate::patch::is_patched(t) {
                continue;
            }
            match crate::patch::apply(&dir, t) {
                Ok(p) if p.patched => freshly.push(t.clone()),
                Ok(p) => {
                    rollback_patches(&dir, &freshly);
                    return Err(p.message);
                }
                Err(e) => {
                    rollback_patches(&dir, &freshly);
                    return Err(e);
                }
            }
        }
        for t in &added {
            if let Err(e) = crate::endpoint::preflight(t, &endpoint_base) {
                rollback_patches(&dir, &freshly);
                return Err(e);
            }
        }
        if !crate::proxy::status().active {
            rollback_patches(&dir, &freshly);
            return Err(
                "本地反代没有在监听，新勾选的应用暂时改不了道。先关闭再重新开启一次接管即可。"
                    .to_string(),
            );
        }
    }

    // ③ 只重启**受影响**的应用（新勾上的 + 放下的），其余一个都不碰
    let mut affected: Vec<crate::target::AppTarget> = added.clone();
    affected.extend(dropped.iter().cloned());
    if !affected.is_empty() {
        let dir2 = dir.clone();
        let added2 = added.clone();
        let (_, restarted) = crate::target::with_restart(&affected, || {
            for t in &added2 {
                crate::endpoint::install(t, &dir2, &endpoint_base)?;
            }
            Ok(())
        })?;
        if !restarted.is_empty() {
            let names: Vec<&str> = added
                .iter()
                .map(|t| t.id.as_str())
                .chain(dropped.iter().map(|t| t.id.as_str()))
                .collect();
            crate::journal::append(
                &dir,
                "restart_trae",
                &format!(
                    "接管名单已变更，已重启受影响的应用：{}；未变更的应用保持不动。",
                    names.join("、")
                ),
            );
        }
    }

    Ok(build_status(&dir, &accounts::load_settings(&dir)))
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
    /// 发现了哪些应用、每个应用的端点基址/闸门判定/补丁状态。
    ///
    /// 跑法：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture dump_takeover_enable_on_this_machine
    /// ```
    ///
    /// **默认只读**：不动 `product.json`、不动开关、不重启任何应用。要在本机真跑一遍
    /// 完整路径（会改写端点并重启被接管的应用），必须显式加环境变量：
    /// ```text
    /// TWA_REAL_TAKEOVER=1 cargo test --lib -- --ignored --nocapture dump_takeover_enable_on_this_machine
    /// ```
    #[test]
    #[ignore = "真机诊断：读真实配置；只有设 TWA_REAL_TAKEOVER=1 才会真的改写端点并重启应用"]
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
        println!("接管名单  : {:?}（空 = 全部）", s.takeover_apps);
        println!("端点基址  : {endpoint_base}");

        println!("\n--- ① 本机发现到的应用 ---");
        let all = crate::target::discover();
        if all.is_empty() {
            println!("（一个都没有 —— 接管无从谈起）");
        }
        for t in &all {
            println!(
                "  {:<16} bundle={} running={}",
                t.id,
                t.bundle.display(),
                t.running()
            );
            println!("      product.json = {}", t.product_path().display());
            // 明文端点能不能过闸，取决于**这个目标**的补丁状态（那条双向不变量）。
            let gate = crate::endpoint::preflight(t, &endpoint_base);
            println!("      preflight(当前端点) = {gate:?}");
            let p = crate::patch::status(t);
            println!(
                "      补丁: supported={} patched={} recognized={} writable={} gates={} identity_patterns={}",
                p.supported, p.patched, p.recognized, p.writable, p.gates, p.identity_patterns
            );
            println!("      补丁说明: {}", p.message);
        }

        println!("\n--- ② 当前状态（界面看到的那一份）---");
        let st = build_status(&dir, &s);
        println!(
            "proxy_active={} error={:?} lease_fresh={}",
            st.proxy_active, st.proxy_error, st.lease_fresh
        );
        for a in &st.apps {
            println!(
                "  {:<16} selected={} installed={} ours={} writable={} running={}",
                a.id, a.selected, a.installed, a.ours, a.writable, a.running
            );
            if !a.message.is_empty() {
                println!("      要说的话: {}", a.message);
            }
        }
        if !st.missing_apps.is_empty() {
            println!("名单里本机不存在的: {:?}", st.missing_apps);
        }
        println!("顶层那句话: {}", st.message);

        if std::env::var("TWA_REAL_TAKEOVER").as_deref() != Ok("1") {
            println!("\n（只读模式：未开启接管。要真跑一遍请设 TWA_REAL_TAKEOVER=1）");
            return;
        }

        // ── 真跑：会改写 product.json 并重启被接管的应用 ────────────────
        println!("\n--- ③ 真跑 enable_endpoint（会改写端点 + 重启被接管的应用）---");
        match enable_endpoint(dir.clone()) {
            Ok(st) => println!("成功：{}", st.message),
            Err(e) => println!("返回错误：{e}"),
        }
        let after = build_status(&dir, &accounts::load_settings(&dir));
        println!("收尾：enabled={} proxy_active={}", after.enabled, after.proxy_active);
        for a in &after.apps {
            println!("  {:<16} installed={} ours={}", a.id, a.installed, a.ours);
        }
        println!("（要还原请调用 takeover_disable，或重启助手让它自愈）");
    }

    // -----------------------------------------------------------------------
    // 免证书模式（A′）真机驱动：不依赖界面，直接走生产代码路径
    // -----------------------------------------------------------------------

    fn real_data_dir() -> Option<PathBuf> {
        dirs::data_dir().map(|b| b.join("cn.traework.assistant"))
    }

    /// 只读报告：每个应用的补丁状态 + 端点模式 + 接管状态。**不动任何文件。**
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
        let st = build_status(&dir, &s);
        println!("\n===== 接管 / 补丁 现状 =====");
        println!("数据目录    : {}", dir.display());
        println!(
            "开关        : enabled={} port={}",
            s.takeover_enabled, s.takeover_port
        );
        println!("接管名单    : {:?}（空 = 全部）", s.takeover_apps);
        println!("扣费白名单  : {:?}", s.billing_account_ids);
        println!("端点基址    : {}", st.endpoint_base);
        println!("反代        : active={} error={:?}", st.proxy_active, st.proxy_error);
        println!("租约新鲜    : {}", st.lease_fresh);
        for a in &st.apps {
            println!(
                "\n--- {} ---\n  路径      : {}",
                a.id, a.bundle
            );
            println!(
                "  选中={} 运行={} 端点改写={} 我们的={} 可写={}",
                a.selected, a.running, a.installed, a.ours, a.writable
            );
            println!(
                "  补丁      : supported={} patched={} recognized={} writable={} gates={} identity_patterns={}",
                a.patch.supported, a.patch.patched, a.patch.recognized,
                a.patch.writable, a.patch.gates, a.patch.identity_patterns
            );
            println!("  补丁说明  : {}", a.patch.message);
            println!("  上游      : http={:?} ws={:?}", a.upstream_http, a.upstream_ws);
            if !a.message.is_empty() {
                println!("  要说的话  : {}", a.message);
            }
        }
        if !st.missing_apps.is_empty() {
            println!("\n名单里本机不存在的: {:?}", st.missing_apps);
        }
        println!("\n界面那句话  : {}", st.message);
    }

    /// 真机：给**本机发现的每个**应用打「免证书补丁」。**只改各自的 `out/main.js`**，
    /// 不碰开关、不重启进程。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_patch_apply
    /// ```
    ///
    /// 故意写成「直接调 `patch::apply`」而不是走界面：这是**生产用的同一段代码**，
    /// 而且没有 Tauri 上下文也能跑，便于在真机上把补丁这一步单独验证干净。
    #[test]
    #[ignore = "真机：会修改各应用的 out/main.js（可逐字节还原）"]
    fn live_patch_apply() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let targets = crate::target::discover();
        assert!(!targets.is_empty(), "本机没有发现可接管的 Trae 应用");
        for t in &targets {
            let before = crate::patch::status(t);
            println!("\n===== 「{}」打补丁前 =====", t.id);
            println!("{}", before.message);
            assert!(before.supported, "{}", before.message);
            // fail-safe：版本不认识就到此为止，绝不写盘
            assert!(before.recognized, "{}", before.message);
            if !before.writable {
                // 正常现象：`cargo test` 跑在**终端/工具**的进程里，macOS 的「App 管理」TCC
                // 只授权给过用户点头的 App。要真打补丁请从助手本体走
                // （免证书模式下「开启接管」会自动调用同一段 `patch::apply`）。
                eprintln!("[skip] 当前进程无权重写「{}」：{}", t.id, before.message);
                eprintln!("       这不是 bug —— 用助手本体开接管即可（它走的正是这段代码）。");
                continue;
            }
            if before.patched {
                println!("已经打过补丁，跳过（幂等）");
                continue;
            }
            let after = crate::patch::apply(&dir, t).expect("打补丁失败");
            assert!(after.patched, "打完补丁后状态仍不是 patched：{}", after.message);
            println!("===== 打补丁后 =====\n{}", after.message);
            println!(
                "指纹记录 : {}",
                dir.join(format!("patch_main_js.{}.json", t.id)).display()
            );
        }
    }

    /// 真机：还原**每个**应用的补丁（会先确保接管已关闭，因为明文端点遇上未打补丁的应用
    /// 会让它启动即崩）。
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture live_patch_revert
    /// ```
    #[test]
    #[ignore = "真机：还原各应用 out/main.js；若接管开着会先还原端点并重启应用"]
    fn live_patch_revert() {
        let Some(dir) = real_data_dir() else {
            eprintln!("[skip] 无法定位数据目录");
            return;
        };
        let mut s = accounts::load_settings(&dir);
        let targets = crate::target::discover();
        if s.takeover_enabled {
            println!("接管开着，先还原端点并重启受影响的应用");
            let dir2 = dir.clone();
            let touched: Vec<crate::target::AppTarget> = targets
                .iter()
                .filter(|t| crate::endpoint::is_ours(t))
                .cloned()
                .collect();
            crate::target::with_restart(&touched, || {
                for t in &touched {
                    crate::endpoint::uninstall(t, &dir2)?;
                }
                Ok(())
            })
            .expect("还原端点失败");
            s.takeover_enabled = false;
            accounts::save_settings(&dir, &s).expect("回写设置失败");
        }
        for t in &targets {
            let changed = crate::patch::revert(&dir, t).expect("还原补丁失败");
            let st = crate::patch::status(t);
            println!(
                "\n「{}」还原动作 : {}",
                t.id,
                if changed { "已还原" } else { "无需还原" }
            );
            println!("当前形态 : patched={} recognized={}", st.patched, st.recognized);
            println!("{}", st.message);
        }
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
