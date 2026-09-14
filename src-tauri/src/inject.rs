//! 池化网关「自动接管」逻辑（用户手动添加模型 + 助手自动选中）。
//!
//! ## 重要：SOLO CN 版模型列表由服务端权威下发
//!
//! 逆向 `TRAE SOLO CN` 安装包确认：模型选择器**只读取内存中的 `domain.model`**，该内存域由
//! `model_list_by_function`（经 `TransportManager` → `getAiAgentRpcClient()` 的 **WebSocket
//! JSON-RPC**，鉴权 `Cloud-IDE-JWT`）在每个时机**整体替换**：
//! - 启动（`force_refresh:true`）；
//! - 每 10 分钟轮询（`pollModelList`，`force_refresh:true`）；
//! - 任意模型变更事件（`onModelChange` → 重新拉取）。
//!
//! 本地 `state.vscdb` 的 `AI.agent.model.model_list_map` 仅是**缓存**，每次服务端刷新后被整行覆盖
//! （`[persist] model list saved {keyType:MODEL_LIST_MAP}`）。因此：
//! **外部写入 vscdb 的自定义模型条目，在首次服务端刷新（约 2 秒）后即被剔除，永不可见。**
//!
//! 用户手动「添加自定义模型」之所以能长期存活，是因为它走 `model/add_custom_model`
//! **服务端注册**（分配 `custom_model_id`），而后出现在 `model_list_by_function` 的返回里
//! —— 即模型必须服务端存在才能被渲染。
//!
//! ## 本助手职责（用户选定方案：用户手动添加 + 助手接管）
//!
//! 由于本地写库注入模型是一条已被证实无效的死路，本助手**不再写库注入模型条目**。正确链路：
//! 1. 用户在 TraeWork 里手动添加一次「自定义模型（OpenAI 兼容）」，指向本机网关
//!    `http://127.0.0.1:{port}/v1/chat/completions`（服务端注册、持久）。
//! 2. 本助手只负责：启停本地网关（见 `gateway.rs`）+ **自动选中**该模型。
//!
//! 选中通过写入 `AI.agent.model.recent_user_selection_by_agent_label` 实现，写入的 `modelId`
//! 格式与用户手动在 picker 点选写入的**完全一致**（逆向 `713.mjs` 选择匹配器得到），因此重启 /
//! 服务端刷新后依然有效——模型本身来自服务端列表，不会被覆盖。
//!
//! 模块只做「读缓存匹配 + 写选中记录」，不触碰 `model_list_map` 里的模型条目本身。

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// 自定义 OpenAI 兼容协议提供者 id（`CUSTOM_OPENAI_COMPATIBLE`）。
/// 用户手动添加的自定义模型其 `provider` 必须为该值，也是本助手匹配网关模型的依据之一。
pub const CUSTOM_PROVIDER: &str = "custom_openai_compatible";

/// 候选 TraeWork 应用显示名（macOS bundle 名 / Windows 目录名）。
fn app_names() -> &'static [&'static str] {
    &["TRAE SOLO CN", "TRAE", "Trae TRAE", "TRAE CN", "Trae CN"]
}

/// 候选 TraeWork 进程映像名（Windows 任务管理器中的 ImageName）。
/// 注意排除本助手自身 `traework-assistant.exe`，避免误判。
fn trae_process_names() -> &'static [&'static str] {
    &[
        "TRAE SOLO CN.exe",
        "TRAE CN.exe",
        "Trae CN.exe",
        "TRAE.exe",
        "Trae.exe",
    ]
}

// ---------------------------------------------------------------------------
// 进程检测
// ---------------------------------------------------------------------------

/// TraeWork 当前是否在运行。
///
/// - macOS：用 `pgrep -f` 匹配各候选 app 的 bundle 路径（`.app` 后缀），避免误匹配本助手。
/// - Windows：用 `tasklist` 列出进程，匹配已知的 TraeWork 映像名（排除本助手自身）。
pub fn is_trae_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/fo", "csv", "/nh"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(o) = out {
            let text = String::from_utf8_lossy(&o.stdout).to_lowercase();
            return trae_process_names()
                .iter()
                .any(|n| text.contains(&n.to_lowercase()));
        }
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        let pattern = app_names()
            .iter()
            .map(|n| format!("{n}.app"))
            .collect::<Vec<_>>()
            .join("|");
        let status = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(&pattern)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) => s.success(),
            Err(_) => false,
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        false
    }
}

/// 轮询等待 TraeWork 完全退出，直到 `timeout`（毫秒）。
pub fn wait_for_exit(timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if !is_trae_running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    !is_trae_running()
}

/// 请求 TraeWork 退出并等待其结束。
///
/// Windows 使用 `taskkill /im <exe> /t`，**刻意不加 `/f`**：TraeWork 是编辑器，
/// 强杀可能丢失未保存内容；不带 `/f` 时系统会向窗口投递关闭消息，由应用自行保存退出。
/// macOS 使用 AppleScript `quit`。全部进程退出返回 `true`。
pub fn quit_graceful() -> bool {
    #[cfg(target_os = "windows")]
    {
        for name in trae_process_names() {
            let _ = std::process::Command::new("taskkill")
                .args(["/im", name, "/t"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    #[cfg(target_os = "macos")]
    {
        for name in app_names() {
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(format!("tell application {:?} to quit", name))
                .output();
        }
    }
    wait_for_exit(20_000)
}

/// 重新启动 TraeWork（macOS: `open -a`；Windows: 启动已知安装目录下的 exe）。
///
/// ⚠️ 关键：`spawn()` 之后**必须立刻返回，绝不能 `wait()`**。
/// `wait()` 会阻塞到 TraeWork 进程退出为止（可能数小时），一旦被同步 Tauri 命令调用，
/// 就会把执行线程彻底占死，前端弹出/按钮一直 pending —— 这正是「助手卡死」的根因。
pub fn relaunch() -> bool {
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = dirs::data_local_dir() {
            let programs = local.join("Programs");
            for app in app_names() {
                let exe = programs.join(app).join(format!("{app}.exe"));
                if exe.exists() {
                    let ok = std::process::Command::new(&exe)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .is_ok();
                    if ok {
                        return true;
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for name in app_names() {
            let ok = std::process::Command::new("open")
                .arg("-a")
                .arg(name)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok();
            if ok {
                return true;
            }
        }
    }
    false
}

/// 在 TraeWork 运行强制重启的前提下执行 `op`：运行中 → 退出 → 执行 → 重新启动；未运行 → 仅执行。
/// 返回 `(op 结果, 是否曾运行)`。
pub fn with_trae_restart<T>(
    op: impl FnOnce() -> Result<T, String>,
) -> Result<(T, bool), String> {
    let was_running = is_trae_running();
    if was_running && !quit_graceful() {
        return Err("已发起退出请求，但 TraeWork 未在限时内退出，请手动关闭后重试。".into());
    }
    let out = op()?;
    if was_running {
        let _ = relaunch();
    }
    Ok((out, was_running))
}

// ---------------------------------------------------------------------------
// DB 定位
// ---------------------------------------------------------------------------

/// 定位各登录态的 `globalStorage/state.vscdb`（去重）。
pub fn locate_db() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            let base = home.join("Library").join("Application Support");
            for app in app_names() {
                roots.push(base.join(app).join("User").join("globalStorage").join("state.vscdb"));
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // TraeWork 桌面端把 userData 落在 %APPDATA%（Roaming，即 data_dir()）；
        // 少数场景也可能出现在 %LOCALAPPDATA%（Local）。两个根都扫，避免漏掉 state.vscdb。
        let bases: Vec<PathBuf> = [dirs::data_dir(), dirs::data_local_dir()]
            .into_iter()
            .flatten()
            .collect();
        for base in bases {
            for app in app_names() {
                roots.push(base.join(app).join("User").join("globalStorage").join("state.vscdb"));
            }
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in roots {
        if !seen.contains(&p) && p.exists() {
            seen.push(p.clone());
            out.push(p);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SQLite 读写
// ---------------------------------------------------------------------------

fn open(db: &Path) -> Result<Connection, String> {
    let conn = Connection::open(db).map_err(|e| format!("打开 {db:?} 失败：{e}"))?;
    conn.execute_batch("PRAGMA busy_timeout=3000; PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("PRAGMA 失败：{e}"))?;
    Ok(conn)
}

/// 枚举 `model_list_map` 实际 key（后缀匹配，不假设 userId 前缀）。
fn model_list_keys(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM ItemTable WHERE key LIKE '%:AI.agent.model.model_list_map' \
             OR key LIKE '%_AI.agent.model.model_list_map'",
        )
        .map_err(|e| format!("查询 model_list_map 失败：{e}"))?;
    let keys: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| format!("遍历失败：{e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("转义失败：{e}"))?;
    Ok(keys)
}

fn read_map(conn: &Connection, key: &str) -> Result<serde_json::Value, String> {
    let text: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .map_err(|e| format!("读取 {key} 失败：{e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("解析 {key} JSON 失败：{e}"))
}

/// 非 remote 的本地 agent label 集合（接管只作用于本地构建，不碰 remote）。
fn local_labels(map: &serde_json::Value) -> Vec<String> {
    map.as_object()
        .map(|o| {
            o.keys()
                .filter(|k| !k.to_lowercase().contains("remote"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 本机网关的目标 base_url。
fn gateway_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1/chat/completions")
}

// ---------------------------------------------------------------------------
// 匹配用户在 TraeWork 手动添加的网关模型
// ---------------------------------------------------------------------------

/// 单个 db 中匹配到的、指向本机网关的自定义模型。
pub struct GatewayModel {
    pub db: String,
    pub key: String,
    pub label: String,
    pub name: String,
    pub provider: String,
    pub config_source: u8,
    pub custom_model_id: Option<String>,
    pub base_url: String,
}

/// 在给定 db 列表中扫描 `model_list_map`，找出「base_url 指向本机网关 + Personal 自定义」的模型。
/// 只读，可在 TraeWork 运行时安全调用（读 vscdb 快照）。
pub fn find_gateway_models_in(dbs: &[PathBuf], port: u16) -> Vec<GatewayModel> {
    let target = gateway_base_url(port);
    let mut out: Vec<GatewayModel> = Vec::new();
    for db in dbs {
        if let Ok(conn) = open(db) {
            if let Ok(keys) = model_list_keys(&conn) {
                for key in keys {
                    if let Ok(map) = read_map(&conn, &key) {
                        for label in local_labels(&map) {
                            if let Some(arr) = map.get(&label).and_then(|v| v.as_array()) {
                                for m in arr {
                                    let base =
                                        m.get("base_url").and_then(|b| b.as_str()).unwrap_or("");
                                    if base != target {
                                        continue;
                                    }
                                    let provider =
                                        m.get("provider").and_then(|p| p.as_str()).unwrap_or("");
                                    if provider != CUSTOM_PROVIDER {
                                        continue;
                                    }
                                    let cs =
                                        m.get("config_source").and_then(|c| c.as_u64()).unwrap_or(0)
                                            as u8;
                                    if cs != 3 {
                                        // 仅 Personal 自定义模型才会被服务端列表保留
                                        continue;
                                    }
                                    let name = match m.get("name").and_then(|n| n.as_str()) {
                                        Some(n) => n.to_string(),
                                        None => continue,
                                    };
                                    let cmid = m
                                        .get("custom_model_id")
                                        .and_then(|c| c.as_str())
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string());
                                    out.push(GatewayModel {
                                        db: db.display().to_string(),
                                        key: key.clone(),
                                        label: label.clone(),
                                        name,
                                        provider: provider.to_string(),
                                        config_source: cs,
                                        custom_model_id: cmid,
                                        base_url: target.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// 公开入口：扫描本机所有登录态。
pub fn find_gateway_models(port: u16) -> Vec<GatewayModel> {
    let dbs = locate_db();
    find_gateway_models_in(&dbs, port)
}

/// 按 TraeWork 选择匹配器构造 `modelId`：
/// `${label}_${config_source}_${provider}_${name}[_${custom_model_id}]`
/// （`custom_model_id` 存在时追加，与用户手动点选写入的格式一致）。
pub fn build_model_id(
    label: &str,
    config_source: u8,
    provider: &str,
    name: &str,
    custom_model_id: Option<&str>,
) -> String {
    let base = format!("{label}_{config_source}_{provider}_{name}");
    match custom_model_id {
        Some(id) if !id.is_empty() => format!("{base}_{id}"),
        _ => base,
    }
}

// ---------------------------------------------------------------------------
// 写入选中记录（接管）
// ---------------------------------------------------------------------------

/// 对任意 label 列表，把对应的 `modelId` 写入该 db 的 `recent_user_selection_by_agent_label`。
fn write_selections(db: &Path, items: &[(String, String)]) -> Result<(), String> {
    let conn = open(db)?;
    let mut stmt = conn
        .prepare("SELECT key, value FROM ItemTable WHERE key LIKE '%:AI.agent.model.recent_user_selection_by_agent_label'")
        .map_err(|e| format!("查询选中失败：{e}"))?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("遍历失败：{e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("转义失败：{e}"))?;
    drop(stmt);
    for (key, text) in rows {
        let mut sel: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Object(Default::default()));
        let obj = sel.as_object_mut().unwrap();
        for (label, mid) in items {
            obj.insert(
                label.clone(),
                serde_json::json!({ "modelId": mid, "mode": 0 }),
            );
        }
        conn.execute(
            "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
            rusqlite::params![serde_json::to_string(&sel).map_err(|e| e.to_string())?, key],
        )
        .map_err(|e| format!("写入选中失败：{e}"))?;
    }
    Ok(())
}

/// 主入口：在 TraeWork 未运行时，把用户已添加的网关模型自动选为各 agent 入口的当前模型。
/// 返回 `(匹配到的模型数, 受影响 label 列表)`。匹配不到返回 `(0, [])` 而不报错。
pub fn select_gateway_model_in(dbs: &[PathBuf], port: u16) -> Result<(usize, Vec<String>), String> {
    if is_trae_running() {
        return Err("TraeWork 正在运行，无法写入选中记录（运行时写入会在退出时被覆盖）。请先关闭 TraeWork 后重试。".into());
    }
    let models = find_gateway_models_in(dbs, port);
    if models.is_empty() {
        return Ok((0, Vec::new()));
    }
    // 按 db 归组：(label, modelId)
    let mut by_db: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for m in &models {
        let mid = build_model_id(
            &m.label,
            m.config_source,
            &m.provider,
            &m.name,
            m.custom_model_id.as_deref(),
        );
        by_db.entry(m.db.clone()).or_default().push((m.label.clone(), mid));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for (db, items) in &by_db {
        write_selections(Path::new(db), items)?;
        for (label, _) in items {
            seen.insert(label.clone());
        }
    }
    let labels: Vec<String> = seen.into_iter().collect();
    Ok((models.len(), labels))
}

/// 公开入口：扫描本机所有登录态后接管选中。
pub fn select_gateway_model(port: u16) -> Result<(usize, Vec<String>), String> {
    let dbs = locate_db();
    select_gateway_model_in(&dbs, port)
}

// ---------------------------------------------------------------------------
// 清除选中记录（解除接管）
// ---------------------------------------------------------------------------

/// 递归：若 `modelId` 命中目标集合，则置空（保留其余字段与兄弟节点）。
fn scrub_by_ids(v: &mut serde_json::Value, ids: &HashSet<String>) -> bool {
    let mut touched = false;
    match v {
        serde_json::Value::Object(map) => {
            if let Some(mid) = map.get("modelId").and_then(|m| m.as_str()) {
                if ids.contains(mid) {
                    map.insert(
                        "modelId".to_string(),
                        serde_json::Value::String(String::new()),
                    );
                    touched = true;
                }
            }
            for (_k, child) in map.iter_mut() {
                if scrub_by_ids(child, ids) {
                    touched = true;
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for child in arr.iter_mut() {
                if scrub_by_ids(child, ids) {
                    touched = true;
                }
            }
        }
        _ => {}
    }
    touched
}

/// 清除指向本机网关的自动选中记录（关闭接管时调用）。返回被清理的 db 数。
/// 仅清空「助手写入的选中」，绝不删除用户自己的自定义模型条目。
pub fn clear_gateway_selection_in(dbs: &[PathBuf], port: u16) -> Result<usize, String> {
    if is_trae_running() {
        return Err("TraeWork 正在运行，无法写入选中记录（运行时写入会被覆盖）。请先关闭 TraeWork 后重试。".into());
    }
    let models = find_gateway_models_in(dbs, port);
    let ids: HashSet<String> = models
        .iter()
        .map(|m| {
            build_model_id(
                &m.label,
                m.config_source,
                &m.provider,
                &m.name,
                m.custom_model_id.as_deref(),
            )
        })
        .collect();
    let mut cleared = 0usize;
    for db in dbs {
        if let Ok(conn) = open(db) {
            let mut stmt = conn
                .prepare(
                    "SELECT key, value FROM ItemTable \
                     WHERE key LIKE '%:AI.agent.model.recent_user_selection_by_agent_label' \
                        OR key LIKE '%:AI.agent.model.session_selected_model'",
                )
                .map_err(|e| format!("查询选中记录失败：{e}"))?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| format!("遍历选中记录失败：{e}"))?
                .collect::<Result<_, _>>()
                .map_err(|e| format!("转义失败：{e}"))?;
            drop(stmt);
            for (key, text) in rows {
                let mut sel: serde_json::Value = serde_json::from_str(&text)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                if scrub_by_ids(&mut sel, &ids) {
                    conn.execute(
                        "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
                        rusqlite::params![
                            serde_json::to_string(&sel).map_err(|e| e.to_string())?,
                            key
                        ],
                    )
                    .map_err(|e| format!("清理选中记录 {key} 失败：{e}"))?;
                    cleared += 1;
                }
            }
        }
    }
    Ok(cleared)
}

/// 公开入口：清除本机所有登录态的网关选中记录。
pub fn clear_gateway_selection(port: u16) -> Result<usize, String> {
    let dbs = locate_db();
    clear_gateway_selection_in(&dbs, port)
}

// ---------------------------------------------------------------------------
// 只读状态（供 UI / 启动自愈）
// ---------------------------------------------------------------------------

/// 该 db 的某 label 是否已选中指定 `modelId`。
fn is_selected(db: &str, label: &str, mid: &str) -> bool {
    let conn = match open(Path::new(db)) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut stmt = match conn
        .prepare("SELECT value FROM ItemTable WHERE key LIKE '%:AI.agent.model.recent_user_selection_by_agent_label'")
    {
        Ok(s) => s,
        Err(_) => return false,
    };
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map(|q| q.collect::<Result<Vec<_>, _>>())
        .unwrap_or_else(|_| Ok(Vec::new()))
        .unwrap_or_default();
    for text in rows {
        if let Ok(sel) = serde_json::from_str::<serde_json::Value>(&text) {
            if sel
                .get(label)
                .and_then(|v| v.get("modelId"))
                .and_then(|m| m.as_str())
                == Some(mid)
            {
                return true;
            }
        }
    }
    false
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TakeoverEntry {
    pub db: String,
    pub key: String,
    /// agent 入口（如 `solo_agent_lite`）
    pub label: String,
    /// 模型在 model_list_map 中的 `name`（`{provider}//{display}`）
    pub name: String,
    pub base_url: String,
    /// 该入口当前是否已选中本机网关模型
    pub selected: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TakeoverStatus {
    pub trae_running: bool,
    /// 本机是否已有指向网关的自定义模型（用户是否已手动添加）
    pub matched: bool,
    pub base_url: String,
    pub entries: Vec<TakeoverEntry>,
}

/// 只读汇总：本机是否已有指向网关的自定义模型、各入口是否已选中。
pub fn takeover_status(port: u16) -> TakeoverStatus {
    let target = gateway_base_url(port);
    let models = find_gateway_models(port);
    let mut entries = Vec::new();
    for m in &models {
        let mid = build_model_id(
            &m.label,
            m.config_source,
            &m.provider,
            &m.name,
            m.custom_model_id.as_deref(),
        );
        let selected = is_selected(&m.db, &m.label, &mid);
        entries.push(TakeoverEntry {
            db: m.db.clone(),
            key: m.key.clone(),
            label: m.label.clone(),
            name: m.name.clone(),
            base_url: m.base_url.clone(),
            selected,
        });
    }
    TakeoverStatus {
        trae_running: is_trae_running(),
        matched: !models.is_empty(),
        base_url: target,
        entries,
    }
}

// ---------------------------------------------------------------------------
// 启动自愈：若已开启接管且用户已添加网关模型，在 TraeWork 未运行时自动选中。
// ---------------------------------------------------------------------------

/// 返回值：本次成功接管的 label 数（0 表示未添加模型或 TraeWork 运行中）。
pub fn self_heal(enabled: bool, port: u16) -> usize {
    if !enabled || is_trae_running() {
        return 0;
    }
    match select_gateway_model(port) {
        Ok((_count, labels)) => labels.len(),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// 在临时目录建一个带 ItemTable 的 state.vscdb，播种 model_list_map 与选中记录。
    /// `with_gateway` 控制是否包含一个指向本机网关的 Personal 自定义模型。
    fn seed_db(prefix: &str, labels: &[&str], with_gateway: bool) -> (PathBuf, String, String) {
        let dir = std::env::temp_dir();
        let db = dir.join(format!("twa_takeover_test_{}.vscdb", prefix));
        let _ = std::fs::remove_file(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);")
            .unwrap();
        let key = format!("{prefix}:AI.agent.model.model_list_map");
        let mut map = serde_json::Map::new();
        for l in labels {
            let mut arr: Vec<serde_json::Value> = Vec::new();
            if with_gateway {
                arr.push(serde_json::json!({
                    "name": "custom_openai_compatible//MyPool",
                    "provider": CUSTOM_PROVIDER,
                    "config_source": 3,
                    "base_url": "http://127.0.0.1:8788/v1/chat/completions",
                    "custom_model_id": "2647612930",
                    "display_name": "MyPool"
                }));
            }
            // 一个干扰项：base_url 不同
            arr.push(serde_json::json!({
                "name": "custom_openai_compatible//Other",
                "provider": CUSTOM_PROVIDER,
                "config_source": 3,
                "base_url": "http://127.0.0.1:9999/v1/chat/completions",
                "custom_model_id": "1",
                "display_name": "Other"
            }));
            map.insert(l.to_string(), serde_json::Value::Array(arr));
        }
        conn.execute(
            "INSERT INTO ItemTable(key,value) VALUES(?1,?2)",
            rusqlite::params![&key, serde_json::to_string(&serde_json::Value::Object(map)).unwrap()],
        )
        .unwrap();
        let sel_key = format!("{prefix}:AI.agent.model.recent_user_selection_by_agent_label");
        let mut sel = serde_json::Map::new();
        for l in labels {
            sel.insert(
                l.to_string(),
                serde_json::json!({ "modelId": format!("{l}_1__X_null"), "mode": 0 }),
            );
        }
        conn.execute(
            "INSERT INTO ItemTable(key,value) VALUES(?1,?2)",
            rusqlite::params![&sel_key, serde_json::to_string(&serde_json::Value::Object(sel)).unwrap()],
        )
        .unwrap();
        drop(conn);
        (db, key, sel_key)
    }

    #[test]
    fn build_model_id_formats() {
        // 有 custom_model_id：追加后缀
        assert_eq!(
            build_model_id("solo_agent_lite", 3, CUSTOM_PROVIDER, "custom_openai_compatible//MyPool", Some("2647612930")),
            "solo_agent_lite_3_custom_openai_compatible_custom_openai_compatible//MyPool_2647612930"
        );
        // 无 custom_model_id：无后缀
        assert_eq!(
            build_model_id("assistant", 3, CUSTOM_PROVIDER, "custom_openai_compatible//X", None),
            "assistant_3_custom_openai_compatible_custom_openai_compatible//X"
        );
        // 空字符串视为无后缀
        assert_eq!(
            build_model_id("assistant", 3, CUSTOM_PROVIDER, "custom_openai_compatible//X", Some("")),
            "assistant_3_custom_openai_compatible_custom_openai_compatible//X"
        );
    }

    #[test]
    fn find_matches_gateway_model_only() {
        let (db, _key, _sel) = seed_db("u1", &["solo_agent_lite", "solo_agent_remote"], true);
        let models = find_gateway_models_in(&[db.clone()], 8788);
        // remote label 应被排除；仅 solo_agent_lite 命中网关（Other 的 base_url 不同）
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].label, "solo_agent_lite");
        assert_eq!(models[0].base_url, "http://127.0.0.1:8788/v1/chat/completions");
        assert_eq!(models[0].custom_model_id.as_deref(), Some("2647612930"));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn find_nothing_when_absent() {
        let (db, _key, _sel) = seed_db("u2", &["solo_agent_lite"], false);
        let models = find_gateway_models_in(&[db.clone()], 8788);
        assert!(models.is_empty());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn write_and_clear_selection_roundtrip() {
        let (db, _key, sel_key) = seed_db("u3", &["solo_agent_lite"], true);
        let models = find_gateway_models_in(&[db.clone()], 8788);
        assert_eq!(models.len(), 1);
        let m = &models[0];
        let mid = build_model_id(
            &m.label,
            m.config_source,
            &m.provider,
            &m.name,
            m.custom_model_id.as_deref(),
        );
        // 写入选中
        write_selections(&db, &[(m.label.clone(), mid.clone())]).unwrap();
        assert!(is_selected(&db.display().to_string(), &m.label, &mid));
        // 再次汇总应报告 selected=true
        let st = takeover_status_in(&[db.clone()], 8788);
        assert!(st.matched);
        assert!(st.entries.iter().any(|e| e.label == "solo_agent_lite" && e.selected));
        // 清除选中
        let ids: HashSet<String> = [mid.clone()].into_iter().collect();
        {
            let conn = open(&db).unwrap();
            let mut stmt = conn
                .prepare("SELECT key, value FROM ItemTable WHERE key LIKE '%:AI.agent.model.recent_user_selection_by_agent_label'")
                .unwrap();
            let rows: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for (k, text) in rows {
                let mut sel: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert!(scrub_by_ids(&mut sel, &ids));
                conn.execute(
                    "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
                    rusqlite::params![serde_json::to_string(&sel).unwrap(), k],
                )
                .unwrap();
            }
        }
        assert!(!is_selected(&db.display().to_string(), &m.label, &mid));
        let _ = sel_key;
        let _ = std::fs::remove_file(&db);
    }

    /// 测试用：仅对本函数可见的 status 包装（复用 takeover_status 逻辑但指定 db 列表）。
    fn takeover_status_in(dbs: &[PathBuf], port: u16) -> TakeoverStatus {
        let target = gateway_base_url(port);
        let models = find_gateway_models_in(dbs, port);
        let mut entries = Vec::new();
        for m in &models {
            let mid = build_model_id(
                &m.label,
                m.config_source,
                &m.provider,
                &m.name,
                m.custom_model_id.as_deref(),
            );
            let selected = is_selected(&m.db, &m.label, &mid);
            entries.push(TakeoverEntry {
                db: m.db.clone(),
                key: m.key.clone(),
                label: m.label.clone(),
                name: m.name.clone(),
                base_url: m.base_url.clone(),
                selected,
            });
        }
        TakeoverStatus {
            trae_running: is_trae_running(),
            matched: !models.is_empty(),
            base_url: target,
            entries,
        }
    }
}
