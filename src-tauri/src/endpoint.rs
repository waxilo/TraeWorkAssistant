//! 「智能接管」的 TraeWork 端点覆盖层：把 TraeWork 的模型/会话 HTTP 请求整体重定向到本机反代。
//!
//! ## 原理（逆向 `TRAE SOLO CN` 的 `out/main.js` 得到）
//!
//! 主进程在产品配置装配的最后一步，会**深合并**安装目录下的
//! `resources/app/product.desktop.local.json`（若存在）到 `_VSCODE_PRODUCT_JSON`：
//!
//! ```js
//! if (fs.existsSync(join(import.meta.dirname, "../product.desktop.local.json"))) {
//!   globalThis._VSCODE_PRODUCT_JSON = Kc(_VSCODE_PRODUCT_JSON, Yc("../product.desktop.local.json"));
//! }
//! ```
//!
//! 而 bootConfig 组装 `{ remote:{domain:Bs(u)}, agent:{domain:y}, ws:{domain:w}, ... }` 读取的正是
//! 该产品配置里按区域选出的 `remote.trae.normal` / `agent.trae.normal` / `ws.trae.normal`。
//! 其中 `Bs = t => t.indexOf("://")>=0 ? t : "https://"+t` —— **不强制 https**，
//! 因此 `http://127.0.0.1:PORT` 会被原样采用：**无需自签证书、无需 TLS MITM**。
//!
//! ## 覆盖范围
//!
//! 逆向 `@byted-icube/solo-lite` 的 214 条 REST 端点表确认，模型/会话/流式全在 `remote.domain`：
//! - `GET  /api/remote/v1/models`（模型列表，只读，服务端权威）
//! - `POST /api/remote/v1/chat_sessions`（建会话）
//! - `POST /api/remote/v1/chat_sessions/:id/messages`（**发消息 → 触发推理**）
//! - `GET  /api/remote/v1/chat_sessions/:id/events`（**SSE 流式回包**）
//!
//! ⇒ 覆盖 `remote.domain` 即可让整条模型/对话链路落到本机反代（反代再按账号池换凭据回源）。
//!
//! ## 为什么**不**覆盖 `ws.domain`
//!
//! `ws.trae.normal = wss://trae-ws-cn.mchost.guru/custom_model` 是 icube RPC 通道（模型增删等）。
//! **刻意不覆盖它**：深合并会保留原始值，RPC 仍直连官方、用用户自己的登录态，避免把
//! 模型管理/会话协商一并打断。仅 HTTP 面走池化。
//!
//! ## 安全设计（必须遵守，否则会造成 TraeWork 全量失败）
//!
//! 覆盖残留 + 反代未运行 = TraeWork 所有请求打到死端口，用户界面直接不可用。为此：
//! 1. 覆盖文件里写入**自识别标记**（顶层 `__traeWorkAssistant`）——只清理/恢复自己写的文件，
//!    用户自己的 `product.desktop.local.json` 绝不触碰；
//! 2. **租约（lease）心跳**：反代运行期间每 30s 刷新助手数据目录下的 `endpoint_lease.json`，
//!    作为「助手仍在掌管该文件」的证据供界面展示；
//! 3. 启动 `sweep()`：只有确认**本地反代不可用**时才删除覆盖恢复官方直连（见函数文档）；
//! 4. `uninstall()` 幂等，且只在文件确为本助手所写时才删除。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// TraeWork 官方约定的本地产品配置覆盖文件名。
const OVERRIDE_FILE: &str = "product.desktop.local.json";
/// 自识别标记键（顶层）。
const MARKER_KEY: &str = "__traeWorkAssistant";
/// 租约文件名（落在助手自己的数据目录，不进 TraeWork）。
const LEASE_FILE: &str = "endpoint_lease.json";
/// 租约有效期：网关每 30s 刷新，超过该时长视为助手已死。
const LEASE_TTL_MS: u128 = 90_000;

/// 候选 TraeWork 应用显示名（Windows 安装目录名 / macOS bundle 名）。
fn app_names() -> &'static [&'static str] {
    &["TRAE SOLO CN", "TRAE", "Trae TRAE", "TRAE CN", "Trae CN"]
}

/// TraeWork 的 `resources/app` 目录（内含 `product.json`）。
pub fn app_dir() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let bases: Vec<PathBuf> = [dirs::data_local_dir(), dirs::data_dir()]
            .into_iter()
            .flatten()
            .collect();
        for base in &bases {
            for app in app_names() {
                // 用户级安装（默认）：%LOCALAPPDATA%\Programs\<app>\resources\app
                cands.push(base.join("Programs").join(app).join("resources").join("app"));
                // 少数安装器直接落在 %LOCALAPPDATA%\<app>
                cands.push(base.join(app).join("resources").join("app"));
            }
        }
        // 机器级安装
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(key) {
                for app in app_names() {
                    cands.push(PathBuf::from(&pf).join(app).join("resources").join("app"));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for app in app_names() {
            cands.push(
                PathBuf::from("/Applications")
                    .join(format!("{app}.app"))
                    .join("Contents")
                    .join("Resources")
                    .join("app"),
            );
        }
        if let Some(home) = dirs::home_dir() {
            for app in app_names() {
                cands.push(
                    home.join("Applications")
                        .join(format!("{app}.app"))
                        .join("Contents")
                        .join("Resources")
                        .join("app"),
                );
            }
        }
    }
    cands.into_iter().find(|p| p.join("product.json").exists())
}

/// 覆盖文件路径（`resources/app/product.desktop.local.json`）。
pub fn override_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join(OVERRIDE_FILE))
}

// ---------------------------------------------------------------------------
// 上游域名解析（从原 `product.json` 读取，绝不硬编码）
// ---------------------------------------------------------------------------

/// 在 JSON 子树上按前缀深度优先找第一个字符串（区域键名不稳定，故按值前缀匹配）。
fn first_prefixed(v: &Value, prefix: &str) -> Option<String> {
    match v {
        Value::String(s) if s.starts_with(prefix) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(|x| first_prefixed(x, prefix)),
        Value::Object(o) => o.values().find_map(|x| first_prefixed(x, prefix)),
        _ => None,
    }
}

fn exact_path<'a>(node: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = node;
    for k in path {
        cur = cur.get(*k)?;
    }
    Some(cur)
}

/// 取区域内域名。
///
/// ⚠️ `serde_json::Map` 默认是 `BTreeMap`，键按**字母序**遍历——直接深搜会先撞上
/// `overseas`（o < t），从而错误地取到海外域名。因此这里**显式优先 `trae.*`**（CN 版），
/// 再退化为确定性深搜。
fn pick_domain(node: &Value, prefix: &str) -> Option<String> {
    for path in [&["trae", "normal"][..], &["trae", "cn"][..], &["trae"][..], &["normal"][..], &["cn"][..]] {
        if let Some(found) = exact_path(node, path).and_then(|v| first_prefixed(v, prefix)) {
            return Some(found);
        }
    }
    first_prefixed(node, prefix)
}

/// 从 `product.json` 读取原始上游域名。
///
/// 返回 `(http 上游, ws 上游)`；`remote.domain` 的原始值即 HTTP 上游，
/// `ws.domain` 的原始值即 WS 上游。两者都取自产品配置，升级换域名后自动跟随。
pub fn read_upstreams() -> (Option<String>, Option<String>) {
    let Some(app) = app_dir() else {
        return (None, None);
    };
    let Ok(text) = std::fs::read_to_string(app.join("product.json")) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (None, None);
    };
    let boot = v.get("bootConfig");
    let http = boot
        .and_then(|b| b.get("remote"))
        .and_then(|r| pick_domain(r, "http"));
    let ws = boot
        .and_then(|b| b.get("ws"))
        .and_then(|w| pick_domain(w, "ws"));
    (http, ws)
}

// ---------------------------------------------------------------------------
// 覆盖文档构造与识别（纯函数，便于单测）
// ---------------------------------------------------------------------------

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 构造覆盖文档。
///
/// - 顶层写入 `__traeWorkAssistant` 标记，供后续只清理自己写的文件；
/// - `remote/agent/ckg/cue/hub` 指向本机 HTTP 网关（含 `domain` 与区域键 `trae.normal` 双写，
///   以兼容「合并发生在 bootConfig 组装之前 / 之后」两种时序）；
/// - `ws` 仅在 `ws_base` 非空时覆盖（默认不覆盖，避免打断 icube RPC）。
pub fn build_override_doc(http_base: &str, ws_base: Option<&str>) -> Value {
    let region = json!({ "trae": { "normal": http_base } });
    let mut boot = json!({
        "remote": { "domain": http_base, "trae": { "normal": http_base } },
        "agent":  { "domain": http_base, "trae": { "normal": http_base } },
        "ckg":    { "trae": { "normal": http_base } },
        "cue":    { "trae": { "normal": http_base } },
        "hub":    { "trae": { "normal": http_base } },
    });
    let _ = region; // 保留可读性；区域键已内联
    if let Some(ws) = ws_base.filter(|s| !s.is_empty()) {
        boot["ws"] = json!({ "domain": ws, "trae": { "normal": ws } });
    }
    json!({
        MARKER_KEY: {
            "installed_at": now_string(),
            "http_base": http_base,
            "ws_base": ws_base.unwrap_or(""),
        },
        "bootConfig": boot,
    })
}

/// 该文档是否为**本助手**所写（带自识别标记）。
pub fn doc_is_ours(doc: &Value) -> bool {
    doc.get(MARKER_KEY).is_some()
}

/// 指定路径的文件是否为本助手所写（读盘 + 校验标记）。
pub fn is_ours(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| doc_is_ours(&v))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 租约（心跳）
// ---------------------------------------------------------------------------

fn lease_path(dir: &Path) -> PathBuf {
    dir.join(LEASE_FILE)
}

/// 网关运行期间刷新租约。
pub fn touch_lease(dir: &Path) {
    let doc = json!({ "at": now_ms() });
    let _ = std::fs::write(lease_path(dir), doc.to_string());
}

/// 租约是否新鲜（助手/网关仍存活）。
pub fn lease_fresh(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(lease_path(dir)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let at = v.get("at").and_then(|x| x.as_u64()).unwrap_or(0) as u128;
    now_ms().saturating_sub(at) < LEASE_TTL_MS
}

fn clear_lease(dir: &Path) {
    let _ = std::fs::remove_file(lease_path(dir));
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct EndpointStatus {
    /// 是否找到 TraeWork 安装目录（找不到则本机不支持拦截模式）。
    pub supported: bool,
    pub app_dir: Option<String>,
    /// 覆盖文件当前是否存在。
    pub installed: bool,
    /// 覆盖文件是否由本助手写入（用户自己写的文件为 `false`，不会被清理）。
    pub ours: bool,
    /// 覆盖文件是否可写（安装目录权限）。
    pub writable: bool,
    /// 本机网关 HTTP 基址。
    pub http_base: String,
    /// 原始上游（取自 product.json，便于界面展示与排障）。
    pub upstream_http: Option<String>,
    pub upstream_ws: Option<String>,
    pub lease_fresh: bool,
    pub message: String,
}

/// 读取当前覆盖状态（只读，不修改任何文件）。
pub fn status(dir: &Path, http_base: &str) -> EndpointStatus {
    let app = app_dir();
    let (up_http, up_ws) = read_upstreams();
    let path = app.as_ref().map(|d| d.join(OVERRIDE_FILE));
    let installed = path.as_ref().map(|p| p.exists()).unwrap_or(false);
    let ours = path.as_ref().map(|p| is_ours(p)).unwrap_or(false);
    let writable = app
        .as_ref()
        .map(|d| {
            let probe = d.join(".twa_write_probe");
            let ok = std::fs::write(&probe, b"1").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        })
        .unwrap_or(false);
    let message = if app.is_none() {
        "未找到 TraeWork 安装目录，本机不支持「拦截模式」。".to_string()
    } else if installed && !ours {
        "检测到 product.desktop.local.json，但并非本助手写入——助手不会清理它，请自行确认内容。".to_string()
    } else if installed {
        "拦截已安装：TraeWork 下次启动将把模型/会话请求发往本机网关。".to_string()
    } else {
        "未安装拦截。".to_string()
    };
    EndpointStatus {
        supported: app.is_some(),
        app_dir: app.map(|d| d.display().to_string()),
        installed,
        ours,
        writable,
        http_base: http_base.to_string(),
        upstream_http: up_http,
        upstream_ws: up_ws,
        lease_fresh: lease_fresh(dir),
        message,
    }
}

// ---------------------------------------------------------------------------
// 安装 / 卸载 / 清扫
// ---------------------------------------------------------------------------

/// 安装覆盖文件。**调用方必须保证网关已在监听 `http_base`**，否则会造成 TraeWork 全量失败。
///
/// 返回安装后的状态。
pub fn install(dir: &Path, http_base: &str, ws_base: Option<&str>) -> Result<EndpointStatus, String> {
    let app = app_dir().ok_or_else(|| "未找到 TraeWork 安装目录".to_string())?;
    let path = app.join(OVERRIDE_FILE);

    // 已存在且非本助手所写 ⇒ 拒绝覆盖，保护用户自定义配置
    if path.exists() && !is_ours(&path) {
        return Err(format!(
            "{} 已存在且不是本助手写入的，为避免破坏你的自定义配置，已中止。请先手动处理该文件。",
            path.display()
        ));
    }

    let doc = build_override_doc(http_base, ws_base);
    let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("序列化覆盖配置失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    touch_lease(dir);
    crate::journal::append(
        dir,
        "install",
        &format!("已写入 TraeWork 端点覆盖，模型/会话请求改道 {http_base}"),
    );
    Ok(status(dir, http_base))
}

/// 卸载覆盖文件（幂等）。仅当文件确为本助手所写时才删除。
pub fn uninstall(dir: &Path) -> Result<bool, String> {
    let Some(path) = override_path() else {
        return Ok(false);
    };
    if !path.exists() {
        return Ok(false);
    }
    if !is_ours(&path) {
        return Err(format!(
            "{} 不是本助手写入的，不予删除。",
            path.display()
        ));
    }
    std::fs::remove_file(&path).map_err(|e| format!("删除 {} 失败：{e}", path.display()))?;
    crate::journal::append(dir, "uninstall", "已删除 TraeWork 端点覆盖，恢复官方直连");
    Ok(true)
}

/// 启动清扫：**只在本地反代确实不可用时**才删除自己的覆盖，恢复 TraeWork 官方直连。
///
/// `keep=true` 的条件是 `takeover_enabled && 反代已在监听`——此时覆盖指向的端口是活的，
/// TraeWork 启动后能正常走池化，覆盖应保留。除此之外一律恢复，避免把 TraeWork 指向死端口。
///
/// 仅处理**本助手写入**的覆盖文件；用户自己的 `product.desktop.local.json` 绝不触碰。
/// 返回是否发生了恢复删除。
pub fn sweep(dir: &Path, keep: bool) -> bool {
    let Some(path) = override_path() else {
        return false;
    };
    if !path.exists() || !is_ours(&path) {
        return false;
    }
    if keep {
        return false;
    }
    let ok = std::fs::remove_file(&path).is_ok();
    if ok {
        eprintln!("[接管] 已恢复 TraeWork 端点配置（本地反代不可用）");
        clear_lease(dir);
        crate::journal::append(
            dir,
            "sweep",
            "启动时发现端点覆盖残留且本地反代不可用，已自动恢复 TraeWork 官方直连",
        );
    }
    ok
}

// ---------------------------------------------------------------------------
// TraeWork 进程控制：端点覆盖只在 **启动时** 被读取，故改完必须重启才生效
// ---------------------------------------------------------------------------

/// 候选 TraeWork 进程映像名（Windows `tasklist` 中的 ImageName）。
/// 刻意不包含本助手自身，避免误判。
fn trae_process_names() -> &'static [&'static str] {
    &[
        "TRAE SOLO CN.exe",
        "TRAE CN.exe",
        "Trae CN.exe",
        "TRAE.exe",
        "Trae.exe",
    ]
}

/// TraeWork 当前是否在运行。
///
/// - Windows：`tasklist` 列举进程，匹配已知映像名；
/// - macOS：`pgrep -f` 匹配各候选 app 的 bundle 路径（`.app` 后缀），避免误匹配本助手。
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
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(&pattern)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        false
    }
}

/// 轮询等待 TraeWork 完全退出，直到 `timeout_ms` 毫秒。
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
/// Windows 用 `taskkill /im <exe> /t` 且**刻意不加 `/f`**：TraeWork 是编辑器，强杀可能丢失
/// 未保存内容；不带 `/f` 时系统向窗口投递关闭消息，由应用自行保存退出。全部退出返回 `true`。
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
/// ⚠️ `spawn()` 之后**必须立刻返回，绝不能 `wait()`**：`wait()` 会阻塞到 TraeWork 进程退出
/// 为止（可能数小时），一旦被同步命令调用就会把执行线程彻底占死——这正是历史「助手卡死」的根因。
pub fn relaunch() -> bool {
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = dirs::data_local_dir() {
            let programs = local.join("Programs");
            for app in app_names() {
                let exe = programs.join(app).join(format!("{app}.exe"));
                if exe.exists()
                    && std::process::Command::new(&exe)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .is_ok()
                {
                    return true;
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for name in app_names() {
            if std::process::Command::new("open")
                .arg("-a")
                .arg(name)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
            {
                return true;
            }
        }
    }
    false
}

/// 在「TraeWork 运行中则先退出」的前提下执行 `op`，执行完再把它拉起来。
///
/// 返回 `(op 结果, 是否曾运行)`；未运行则只执行 `op`，不触碰进程。
pub fn with_trae_restart<T>(op: impl FnOnce() -> Result<T, String>) -> Result<(T, bool), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_trae_region_domain_over_alphabetical() {
        // serde_json 的 Map 是按 key 排序的 BTreeMap，"overseas" 会排在 "trae" 之前；
        // 若不做显式优先，就会错误地把海外域名当成 CN 上游。
        let v: Value = serde_json::from_str(
            r#"{"overseas":{"normal":"https://trae-api-us.mchost.guru"},"trae":{"normal":"https://trae-api-cn.mchost.guru"}}"#,
        )
        .unwrap();
        assert_eq!(
            pick_domain(&v, "http").as_deref(),
            Some("https://trae-api-cn.mchost.guru")
        );
        assert_eq!(pick_domain(&v, "ws"), None);
    }

    #[test]
    fn pick_domain_falls_back_when_no_trae_key() {
        let v: Value = serde_json::from_str(r#"{"api":{"host":"https://x.example"}}"#).unwrap();
        assert_eq!(pick_domain(&v, "http").as_deref(), Some("https://x.example"));
    }

    #[test]
    fn first_prefixed_walks_nested() {
        let v: Value =
            serde_json::from_str(r#"{"deep":{"deeper":{"normal":"wss://ws.example/custom_model"}}}"#)
                .unwrap();
        assert_eq!(
            first_prefixed(&v, "ws").as_deref(),
            Some("wss://ws.example/custom_model")
        );
        assert_eq!(first_prefixed(&v, "http"), None);
    }

    #[test]
    fn override_doc_marks_itself_and_targets_localhost() {
        let doc = build_override_doc("http://127.0.0.1:8788", None);
        assert!(doc_is_ours(&doc), "必须带自识别标记");
        assert_eq!(doc["bootConfig"]["remote"]["domain"], "http://127.0.0.1:8788");
        assert_eq!(doc["bootConfig"]["remote"]["trae"]["normal"], "http://127.0.0.1:8788");
        assert_eq!(doc["bootConfig"]["agent"]["domain"], "http://127.0.0.1:8788");
        // 未指定 ws 时不覆盖 ws（保留官方 RPC 通道）
        assert!(doc["bootConfig"].get("ws").is_none(), "v1 不得覆盖 ws.domain");
    }

    #[test]
    fn override_doc_can_target_ws_when_asked() {
        let doc = build_override_doc("http://127.0.0.1:8788", Some("ws://127.0.0.1:8789/custom_model"));
        assert_eq!(
            doc["bootConfig"]["ws"]["domain"],
            "ws://127.0.0.1:8789/custom_model"
        );
    }

    #[test]
    fn foreign_doc_is_not_ours() {
        let foreign: Value = serde_json::from_str(r#"{"bootConfig":{"remote":{"domain":"https://x"}}}"#).unwrap();
        assert!(!doc_is_ours(&foreign));
    }

    /// 真实环境冒烟：定位本机 TraeWork 安装目录并解析原始上游域名。
    /// 需显式运行：`cargo test --lib -- --ignored`（无安装时应跳过而非失败）。
    #[test]
    #[ignore]
    fn smoke_reads_real_product_json() {
        let Some(app) = app_dir() else {
            eprintln!("[smoke] 本机未找到 TraeWork 安装目录，跳过");
            return;
        };
        eprintln!("[smoke] app_dir = {}", app.display());
        let (http, ws) = read_upstreams();
        eprintln!("[smoke] upstream http = {http:?}");
        eprintln!("[smoke] upstream ws   = {ws:?}");
        assert!(http.is_some(), "应能从 product.json 解析出 HTTP 上游");
        assert!(
            http.unwrap().starts_with("http"),
            "HTTP 上游必须是 http(s) 地址"
        );
        eprintln!("[smoke] override_path = {:?}", override_path());
        eprintln!("[smoke] 当前是否已安装覆盖 = {}", override_path().map(|p| p.exists()).unwrap_or(false));
    }
}
