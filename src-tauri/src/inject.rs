//! 自动注入「自定义模型」条目到 TraeWork 的 state.vscdb，指向本地池化网关。
//!
//! 根因：TraeWork 是 Electron，运行时持有 state.vscdb 句柄并在内存缓存，退出时整行回写，
//! 因此「运行中外部写库 → 退出即被覆盖」。本模块**只允许在 TraeWork 未运行时写入**，
//! 与「用户手动添加自定义模型」的持久化路径一致，天然稳定；并叠加启动自愈兜底。

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 注入条目的存储 `name`，必须符合 TraeWork 自定义模型命名规范 `{provider}//{display}`，
/// 且 `provider` 必须为自定义 OpenAI 兼容协议提供者 `custom_openai_compatible`。
///
/// 依据逆向源码：TraeWork 只保留 `config_source === Personal(3)` 的自定义条目
/// （`E=t.filter(e=>e.config_source===oE.Lf.Personal)`），其余来源（Trae/Enterprise）会在
/// 重启合并服务端列表时被覆盖丢弃，导致注入不可见。因此 `config_source` 必须为 `3`。
pub const INJECT_NAME: &str = "custom_openai_compatible//TraePool-Gateway";
/// 自定义 OpenAI 兼容协议提供者 id（`CUSTOM_OPENAI_COMPATIBLE`）。
/// 自定义模型条目的 `provider` 与 `name` 前缀必须与此一致。
pub const CUSTOM_PROVIDER: &str = "custom_openai_compatible";
/// 注入条目在 TraeWork 模型选择器中的显示名（`display_name`）。
pub const INJECT_DISPLAY_NAME: &str = "TraePool · 账号池";

/// 候选 TraeWork 应用显示名（macOS bundle 名 / Windows 目录名）。
fn app_names() -> &'static [&'static str] {
    &["TRAE SOLO CN", "TRAE", "Trae TRAE", "TRAE CN"]
}

// ---------------------------------------------------------------------------
// 进程检测
// ---------------------------------------------------------------------------

/// TraeWork 当前是否在运行。
///
/// 用 `pgrep -f` 匹配各候选 app 的 bundle 路径（`.app` 后缀），避免误匹配本助手
/// （traework-assistant / TraeCode 等不含这些 bundle 路径）。
pub fn is_trae_running() -> bool {
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

/// 优雅退出 TraeWork（AppleScript `quit` + 等待完全退出）。成功返回 `true`。
pub fn quit_graceful() -> bool {
    #[cfg(target_os = "macos")]
    {
        for name in app_names() {
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(format!("tell application {:?} to quit", name))
                .output();
        }
    }
    wait_for_exit(15_000)
}

/// 重新启动 TraeWork（`open -a`）。成功返回 `true`。
pub fn relaunch() -> bool {
    #[cfg(target_os = "macos")]
    {
        for name in app_names() {
            if std::process::Command::new("open")
                .arg("-a")
                .arg(name)
                .spawn()
                .map(|mut c| {
                    let _ = c.wait();
                    true
                })
                .unwrap_or(false)
            {
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
        if let Some(local) = dirs::data_local_dir() {
            for app in app_names() {
                roots.push(local.join(app).join("User").join("globalStorage").join("state.vscdb"));
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

fn write_map(conn: &Connection, key: &str, map: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string(map).map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
        rusqlite::params![&text, key],
    )
    .map_err(|e| format!("写入 {key} 失败：{e}"))?;
    Ok(())
}

/// 判定注入条目在某个 key 的某 label 下是否缺失（供自愈校验）。
pub fn entry_missing(db: &Path, key: &str, label: &str, name: &str) -> Result<bool, String> {
    let conn = open(db)?;
    let map = read_map(&conn, key)?;
    Ok(map
        .get(label)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().all(|m| m.get("name").and_then(|n| n.as_str()) != Some(name)))
        .unwrap_or(true))
}

// ---------------------------------------------------------------------------
// 注入
// ---------------------------------------------------------------------------

/// 生成注入的模型条目对象。
///
/// 完全复刻 TraeWork「用户手动添加自定义模型」后写入 `model_list_map` 的真实 schema
/// （以数据库实证的 `custom_openai_compatible//xxx` 条目为准），仅替换身份字段：
/// - `config_source` 必须为 `3`（Personal，用户自定义），否则重启被服务端列表覆盖
/// - `provider` 必须为 `custom_openai_compatible`
/// - `name` 遵循 `{provider}//{display_name}` 规范
fn build_entry(port: u16) -> serde_json::Value {
    // 拆分深层子结构，避免单个 json! 宏嵌套过深触发递归上限
    let icon = serde_json::json!({
        "dark": "https://lf-cdn.trae.com.cn/obj/trae-com-cn/model/default-custom-dark.svg",
        "light": "https://lf-cdn.trae.com.cn/obj/trae-com-cn/model/default-custom-light.svg"
    });
    let features = serde_json::json!({
        "provider": { "enable": true, "data": { "provider_name": CUSTOM_PROVIDER } },
        "context_windows": {
            "enable": true,
            "data": {
                "dev_context": null,
                "max_context": null,
                "max_context_list": null,
                "dev_turns": null,
                "max_turns": null
            }
        }
    });
    serde_json::json!({
        "name": INJECT_NAME,
        "multimodal": true,
        "is_default": false,
        "custom_config": "",
        "display_name": INJECT_DISPLAY_NAME,
        "prompt_max_tokens": null,
        "context_window_size": { "max": null, "default": null },
        "is_new": null,
        "is_beta": null,
        "core_memory_enable_type": null,
        "model_type": "chat_model",
        "builder": true,
        "is_preset": false,
        "client_connect": true,
        "provider": CUSTOM_PROVIDER,
        "icon": icon,
        "ak": "twa",
        "base_url": format!("http://127.0.0.1:{port}/v1/chat/completions"),
        "is_custom_base_url": true,
        "status": true,
        "custom_model_id": "trae-pool-gateway",
        "fee_model_level": null,
        "max_mode": null,
        "is_dollar_max": null,
        "is_max_default": null,
        "config_source": 3,
        "selectable": true,
        "commercial_info": null,
        "tob_commercial_info": null,
        "max_turns": { "default": null, "max": null },
        "tags": [],
        "sk": null,
        "auth_type": 0,
        "region": null,
        "thinking_enable": 0,
        "temperature": null,
        "top_p": null,
        "top_k": null,
        "saas_usage": { "max": null, "default": null },
        "features": features,
        "feedback_group_link": null,
        "is_internal_usage_limit": null,
        "is_l4_repo_restricted": null,
        "hot_info": null,
        "max_tokens": null,
        "max_turn": 500,
        "custom_model_type": "",
        "enable_show_widget": null,
        "enable_todo_thought_extract": null,
        "todo_thought_extract_threshold": null,
        "reasoning_effort_options": null
    })
}

/// 非 remote 的本地 agent label 集合（注入只作用于本地构建，不碰 remote）。
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

/// 备份 db（仅首次创建，保留注入前原始库供还原）。
fn ensure_backup(db: &Path) -> Result<PathBuf, String> {
    let backup = PathBuf::from(format!("{}.twa.bak", db.display()));
    if !backup.exists() {
        std::fs::copy(db, &backup).map_err(|e| format!("备份 {db:?} 失败：{e}"))?;
    }
    Ok(backup)
}

/// 对单个 db 注入条目（幂等：存在同名则更新），返回受影响 label 列表。
fn inject_db(db: &Path, port: u16) -> Result<Vec<String>, String> {
    ensure_backup(db)?;
    let conn = open(db)?;
    let keys = model_list_keys(&conn)?;
    if keys.is_empty() {
        return Err("state.vscdb 中未找到 model_list_map 记录".into());
    }
    let entry = build_entry(port);
    let entry_name = entry["name"].as_str().unwrap_or(INJECT_NAME).to_string();
    let mut touched: Vec<String> = Vec::new();
    for key in &keys {
        let mut map = read_map(&conn, key)?;
        for label in local_labels(&map) {
            let arr = map
                .as_object_mut()
                .and_then(|o| o.get_mut(&label))
                .and_then(|v| v.as_array_mut());
            let Some(arr) = arr else { continue };
            arr.retain(|m| m.get("name").and_then(|n| n.as_str()) != Some(entry_name.as_str()));
            arr.push(entry.clone());
            if !touched.contains(&label) {
                touched.push(label.clone());
            }
        }
        write_map(&conn, key, &map)?;
    }
    // 写入后 readback 校验
    for key in &keys {
        let stored: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .map_err(|e| format!("校验读取失败：{e}"))?;
        let verified = stored.contains(entry_name.as_str());
        if !verified {
            return Err(format!("校验失败：{key} 未包含注入条目"));
        }
    }
    Ok(touched)
}

/// 主入口：校验 TraeWork 未运行后，对所有本机 state.vscdb 注入并切换选中模型。
pub fn inject_all(port: u16) -> Result<usize, String> {
    if is_trae_running() {
        return Err("TraeWork 正在运行，无法写入 state.vscdb（退出时会被覆盖）。请先关闭 TraeWork 后重试。".into());
    }
    let dbs = locate_db();
    if dbs.is_empty() {
        return Err("未发现 TraeWork 的 state.vscdb".into());
    }
    let mut count = 0usize;
    for db in &dbs {
        let labels = inject_db(db, port)?;
        switch_selected(db, &labels)?;
        count += 1;
    }
    Ok(count)
}

/// 把 `recent_user_selection_by_agent_label` 中各 label 的选中切到注入条目。
fn switch_selected(db: &Path, labels: &[String]) -> Result<(), String> {
    let conn = open(db)?;
    let mut stmt = conn
        .prepare("SELECT key, value FROM ItemTable WHERE key LIKE '%:AI.agent.model.recent_user_selection_by_agent_label'")
        .map_err(|e| format!("查询选中失败：{e}"))?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("遍历失败：{e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("转义失败：{e}"))?;
    for (key, text) in rows {
        let mut sel: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Object(Default::default()));
        let obj = sel.as_object_mut().unwrap();
        for label in labels {
            let model_id = format!("{label}_1__{INJECT_NAME}_null");
            obj.insert(
                label.clone(),
                serde_json::json!({ "modelId": model_id, "mode": 0 }),
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

// ---------------------------------------------------------------------------
// 还原 / 自愈
// ---------------------------------------------------------------------------

/// 还原所有 db 到注入前的备份，并删除备份文件。
pub fn revert_all() -> Result<usize, String> {
    let dbs = locate_db();
    let mut count = 0usize;
    for db in dbs {
        let backup = PathBuf::from(format!("{}.twa.bak", db.display()));
        if backup.exists() {
            std::fs::copy(&backup, &db).map_err(|e| format!("还原 {db:?} 失败：{e}"))?;
            let _ = std::fs::remove_file(&backup);
            count += 1;
        }
    }
    Ok(count)
}

/// 启动自愈：对每个 db，若设置要求注入但条目缺失，则（在 TraeWork 未运行时）重新注入。
pub fn self_heal(enabled: bool, port: u16) -> Result<usize, String> {
    if !enabled {
        return Ok(0);
    }
    if is_trae_running() {
        return Ok(0);
    }
    let dbs = locate_db();
    let mut healed = 0usize;
    for db in &dbs {
        let conn = match open(db) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let keys = match model_list_keys(&conn) {
            Ok(k) if !k.is_empty() => k,
            _ => continue,
        };
        let any_missing = keys
            .iter()
            .any(|k| match read_map(&conn, k) {
                Ok(map) => local_labels(&map)
                    .iter()
                    .any(|l| {
                        map.get(l)
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .all(|m| m.get("name").and_then(|n| n.as_str()) != Some(INJECT_NAME))
                            })
                            .unwrap_or(true)
                    }),
                Err(_) => false,
            });
        drop(conn);
        if any_missing {
            match inject_db(db, port) {
                Ok(_) => healed += 1,
                Err(_) => {}
            }
        }
    }
    Ok(healed)
}

// ---------------------------------------------------------------------------
// 只读状态（供 UI / 自愈日志）
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InjectionInfo {
    pub db: String,
    pub key: String,
    /// 注入条目所在的 label 列表（各自道已含条目则视为注入成功）
    pub labels: Vec<String>,
    pub base_url: String,
}

/// 汇总当前注入状态（只读，不落盘）。
pub fn injection_status(port: u16) -> Vec<InjectionInfo> {
    let mut out = Vec::new();
    for db in locate_db() {
        if let Ok(conn) = open(&db) {
            if let Ok(keys) = model_list_keys(&conn) {
                for key in keys {
                    if let Ok(map) = read_map(&conn, &key) {
                        let labels: Vec<String> = local_labels(&map)
                            .into_iter()
                            .filter(|l| {
                                map.get(l)
                                    .and_then(|v| v.as_array())
                                    .map(|arr| {
                                        arr.iter().any(|m| {
                                            m.get("name").and_then(|n| n.as_str()) == Some(INJECT_NAME)
                                        })
                                    })
                                    .unwrap_or(false)
                            })
                            .collect();
                        if !labels.is_empty() {
                            out.push(InjectionInfo {
                                db: db.display().to_string(),
                                key,
                                labels,
                                base_url: format!("http://127.0.0.1:{port}/v1/chat/completions"),
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// 在临时目录建一个带 ItemTable 的 state.vscdb 副本并播种 model_list_map。
    fn seed_db(prefix: &str, labels: &[&str]) -> (PathBuf, String) {
        let dir = std::env::temp_dir();
        let db = dir.join(format!("twa_inject_test_{}.vscdb", prefix));
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(format!("{}.twa.bak", db.display()));
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);")
            .unwrap();
        let key = format!("{prefix}:AI.agent.model.model_list_map");
        let mut map = serde_json::Map::new();
        for l in labels {
            map.insert(l.to_string(), serde_json::json!([]));
        }
        conn.execute(
            "INSERT INTO ItemTable(key,value) VALUES(?1,?2)",
            rusqlite::params![&key, serde_json::to_string(&serde_json::Value::Object(map)).unwrap()],
        )
        .unwrap();
        // 播种选中记录
        let sel_key = format!("{prefix}:AI.agent.model.recent_user_selection_by_agent_label");
        let mut sel = serde_json::Map::new();
        for l in labels {
            sel.insert(l.to_string(), serde_json::json!({ "modelId": format!("{l}_1__X_null"), "mode": 0 }));
        }
        conn.execute(
            "INSERT INTO ItemTable(key,value) VALUES(?1,?2)",
            rusqlite::params![&sel_key, serde_json::to_string(&serde_json::Value::Object(sel)).unwrap()],
        )
        .unwrap();
        drop(conn);
        (db, key)
    }

    #[test]
    fn builds_entry_points_to_local_gateway() {
        let e = build_entry(8788);
        assert_eq!(e["base_url"], "http://127.0.0.1:8788/v1/chat/completions");
        assert_eq!(e["name"], INJECT_NAME);
        assert_eq!(e["is_preset"], false);
        // 关键：TraeWork 只保留 Personal 自定义模型，否则重启被覆盖
        assert_eq!(e["config_source"], 3);
        assert_eq!(e["provider"], CUSTOM_PROVIDER);
        assert_eq!(e["name"], format!("{CUSTOM_PROVIDER}//TraePool-Gateway"));
        assert_eq!(e["selectable"], true);
        assert_eq!(e["status"], true);
    }

    #[test]
    fn local_labels_excludes_remote() {
        let map = serde_json::json!({"solo_agent_lite":[], "solo_agent_remote":[], "assistant":[]});
        let mut labels = local_labels(&map);
        labels.sort();
        assert_eq!(labels, vec!["assistant", "solo_agent_lite"]);
    }

    #[test]
    fn inject_db_roundtrips_and_marks_entry() {
        let (db, key) = seed_db("u1", &["solo_agent_lite", "solo_agent_remote"]);
        let labels = inject_db(&db, 8788).unwrap();
        assert!(labels.contains(&"solo_agent_lite".to_string()));
        assert!(!labels.contains(&"solo_agent_remote".to_string()), "remote label 不应被注入");
        // 校验读回
        assert!(!entry_missing(&db, &key, "solo_agent_lite", INJECT_NAME).unwrap());
        // 删除后应判定缺失
        let conn = open(&db).unwrap();
        let mut map = read_map(&conn, &key).unwrap();
        map["solo_agent_lite"]
            .as_array_mut()
            .unwrap()
            .retain(|m| m["name"] != INJECT_NAME);
        write_map(&conn, &key, &map).unwrap();
        drop(conn);
        assert!(entry_missing(&db, &key, "solo_agent_lite", INJECT_NAME).unwrap());
        // 清理
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(format!("{}.twa.bak", db.display()));
    }

    #[test]
    fn switch_selected_updates_model_id() {
        let (db, key) = seed_db("u2", &["solo_agent_lite"]);
        inject_db(&db, 8788).unwrap();
        switch_selected(&db, &vec!["solo_agent_lite".to_string()]).unwrap();
        let conn = open(&db).unwrap();
        let sel_key = "u2:AI.agent.model.recent_user_selection_by_agent_label";
        let text: String = conn
            .query_row("SELECT value FROM ItemTable WHERE key=?1", [sel_key], |r| r.get(0))
            .unwrap();
        let sel: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mid = sel["solo_agent_lite"]["modelId"].as_str().unwrap();
        assert_eq!(mid, format!("solo_agent_lite_1__{INJECT_NAME}_null"));
        drop(conn);
        let _ = key;
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(format!("{}.twa.bak", db.display()));
    }
}
