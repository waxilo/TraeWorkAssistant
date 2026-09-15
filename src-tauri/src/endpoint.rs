//! 「智能接管」的 TraeWork 端点改写层：把它的模型/会话 HTTP 请求改道到本机反代。
//!
//! ## 为什么不是写 `product.desktop.local.json`
//!
//! 早期实现照抄了「产品配置覆盖文件」的约定，往 `resources/app/product.desktop.local.json`
//! 写一份深合并文档。**实测在 TraeWork 1.107.1（release_solo_cn）上完全不生效**，
//! 因为那段读取逻辑在 `out/bootstrap-fork.js` / `out/main.js` 里是：
//!
//! ```js
//! var S = createRequire(import.meta.url);
//! if ((process.env.ELECTRON_RUN_AS_NODE || process.versions.electron) && register(loader),
//!     globalThis._VSCODE_PRODUCT_JSON = { ...R },   // R = require("../product.json")
//!     process.env.VSCODE_DEV)                        // ← 逗号表达式的最终条件
//! {
//!   try { require("../product.desktop.json"); ... }  // ① 该文件不存在 → 先抛错
//!   ... require("../out/config/cn/boot/boot.cn.js")  // ② out/config/ 是 dev 产物，也不存在
//!   if (existsSync("<app>/product.desktop.local.json")) merge(...)   // ③ 我们的覆盖在这
//! }
//! ```
//!
//! 两道独立的门把它挡死：整块被 `VSCODE_DEV` 守卫，且块内第一步 `require` 就会抛错
//! （`out/config/**` 只存在于 dev 构建里）。生产启动时 `_VSCODE_PRODUCT_JSON` **就是
//! `{...product.json}` 一份**，任何覆盖文件都不会被读。
//!
//! ## 所以改成「原地改写 `product.json`」
//!
//! 真正决定域名的是 `main.js` 的 `BootService.build()`：
//!
//! ```js
//! const o = this.f;                       // = _VSCODE_PRODUCT_JSON.bootConfig
//! const [.., u, ..] = await Promise.all([ E0(o.remote, REMOTE, region, …) ]);
//! return { … remote:{ domain: Bs(u) }, agent:{domain:Bs(l)}, ckg:{…}, cue:{…}, hub:{…}, ws:{…} }
//!     // Bs = t => t.indexOf("://") >= 0 ? t : "https://" + t
//! ```
//!
//! `product.json` 里 `w.BUILD_INSERT_PRODUCT_CONFIGURATION` 是**字面量字符串（恒真）**，
//! 所以这个文件一定会被 `require`。于是只要把它 `bootConfig.<服务>.trae.normal`
//! 改成 `http://127.0.0.1:PORT`，`remote.domain` 等就会跟着变成本地地址。
//!
//! **只改这 5 个 `trae.normal`**（`remote` 模型/会话、`agent`/`ckg`/`cue`/`hub` 同上游），
//! 且**刻意不动**：
//! - `bootConfig.ws`（icube RPC 通道，模型管理等要它直连官方）；
//! - `bootConfig.iCube` / `account` / `market`（登录、token 续签、市场都要直连官方）。
//!
//! ## ⛔ 硬约束：这些值**必须**是 `https://`（`http://` 会让 TraeWork 启动即崩）
//!
//! 2026-09-14 实测踩中：把 `trae.normal` 写成 `http://127.0.0.1:8788` 后，TraeWork
//! 启动时直接退出，`main.log` 只留一行：
//!
//! ```text
//! [error] TypeError: Invalid url pattern https://http://127.0.0.1:8788/*: Invalid port.
//!     at OSe (out/main.js:179:13344)      ← session.webRequest.onBeforeSendHeaders({urls})
//!     at new Nw (out/main.js:181:8655)    ← 创建窗口
//!     at $w.open … ES.startup             ← 启动流程
//! [info] Lifecycle#kill()                 ← 整个应用退出
//! ```
//!
//! 出处是 `out/main.js` 的 `solo-lite-response-cors` 规则（HSe 函数）：
//!
//! ```js
//! const s = Hp(i.bootConfig?.remote), n = Hp(i.bootConfig?.agent), o = Hp(i.bootConfig?.ug?.trae);
//! [...s, ...n, ...o, ...Hp(i.bootConfig?.consoleHost || "")].forEach(c => {
//!   const l = c.startsWith("https://") ? `${c}/*` : `https://${c}/*`;
//!   r.includes(l) || r.push(l);
//! });
//! Wp({ name: "solo-lite-response-cors", urlPatterns: r, … });
//! ```
//!
//! `Hp()` 会**递归收集对象里每一个非空字符串**，所以 `bootConfig.{remote,agent}` 下的
//! **任何**字符串（含 `trae.normal`、`bytedance.*`、`appId` 之外的字段）都会被拿去拼
//! Electron `webRequest` 的 match pattern；而拼接规则「不是 `https://` 开头就补 `https://`」
//! 会让 `http://…` 变成 `https://http://…/*` → 非法 → `onBeforeSendHeaders` 抛错 →
//! 窗口建不出来 → 应用退出。
//!
//! 因此本模块在写回前**必须**用 [`first_bad_pattern_mode`] 逐条模拟这条规则，任何一条非法就
//! **一个字节都不写**（fail-closed）。这不是历史遗留校验：它是唯一能挡住「启动即崩」的东西。
//!
//! ## 端点只有一种形态：免证书（明文回环）
//!
//! | 端点 | 前提 | 闸门规则 |
//! |---|---|---|
//! | `http://127.0.0.1:PORT`（[`base_url`]） | **必须先打 [`crate::patch`] 的补丁** | `includes("://")` |
//!
//! 曾经还有两种形态（2026-09-15 按用户要求**整体移除**，代码一并删除）：
//! `https://127.0.0.1:PORT`（本地反代自讲 TLS，需把自签 CA 装进登录钥匙串）、
//! 以及「经系统代理接管」（写 TraeWork 的 `User/settings.json`，靠 TLS 中间人解密）。
//! 两者都必须动系统信任库，而免证书这一条不必 —— 于是它们只剩风险面。
//!
//! 边界仍由 [`gate_mode_for`] 钉死：端点协议与补丁状态**不一致就地拒绝**，
//! 所以「忘记打补丁却写了明文端点」这条最危险的路径在结构上不可能发生。
//! 它同时（有意）保留对 `https://` 的放行：那不只是历史兼容，TraeWork 自己的闸门规则
//! 就在这里被复刻，`https` 分支是**验证这份复刻是否忠实**的对照组。
//!
//! ## 安全设计（必须遵守，否则会让 TraeWork 全量失败）
//!
//! 改写 `product.json` 是把「指向死端口」的风险直接压到了 TraeWork 的启动路径上，所以：
//! 1. **写前校验**：`validate_doc()` 复刻 TraeWork 的 pattern 拼接规则，任何一条非法就
//!    **不落盘**直接报错（见上节；这是唯一能挡住「启动即崩」的闸门）；
//! 2. **原子写**：先写同目录临时文件再 `rename`，绝不出现「写一半」的 product.json；
//! 3. **精确还原**：顶层写入自识别标记 `__traeWorkAssistant`，其中记录**每个被改键的原值**
//!    （原键不存在则记 `null`，还原时删掉），关闭接管时逐字段写回，不依赖任何猜测；
//! 4. **人工兜底**：首次改写前把整份 `product.json` 备份为 `product.json.twa-orig`
//!    （已存在则不覆盖），万一程序还原失效也能手动恢复；
//! 5. **绝不碰别人的改动**：没有标记就不认为是我们改的，`uninstall` 直接返回 `Ok(false)`；
//! 6. 启动 `sweep()`：只有确认本地反代不可用时才还原（见函数文档）；
//! 7. `repair()`：TraeWork 升级会把 `product.json` 换掉、我们的改动随之丢失，
//!    定期检查并补写（写回后需要重启 TraeWork 才生效）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// TraeWork 的产品配置文件（与 `product.json` 同目录）。
const PRODUCT_FILE: &str = "product.json";
/// 自识别标记键（`product.json` 顶层）。
const MARKER_KEY: &str = "__traeWorkAssistant";
/// 首次改写前的整份备份（人工兜底用，程序**不**读它）。
const BACKUP_FILE: &str = "product.json.twa-orig";
/// 原子写的临时文件名。
const TMP_FILE: &str = "product.json.twa-tmp";
/// 租约文件名（落在助手自己的数据目录，不进 TraeWork）。
const LEASE_FILE: &str = "endpoint_lease.json";
/// 租约有效期：网关每 30s 刷新，超过该时长视为助手已死。
const LEASE_TTL_MS: u128 = 90_000;

/// 要改写的 `bootConfig` 子服务。只改 `trae.normal`：CN 版实际取用的就是它
/// （见 `BootService.build()` 的区域选择 `E0(o.remote, REMOTE, …, region)`）。
const PATCHED_SERVICES: &[&str] = &["remote", "agent", "ckg", "cue", "hub", "ws"];

/// 实时通道（WebSocket）。**它的值和别的服务不一样**，理由见 [`patched_value_for`]：
/// 别的服务写 `https://127.0.0.1:PORT`，它得写 `wss://127.0.0.1:PORT` + **原始路径**。
///
/// 为什么现在敢动它（2026-09-14 时是刻意排除的）：那时本地反代**完全不支持 WebSocket**，
/// 改了只会把实时通道打死。现在反代能透传 WS 升级（见 `proxy.rs::handle_websocket`），
/// 而且 `solo-lite-response-cors` 那条 pattern 规则**只收集 `remote`/`agent`/`ug`**，
/// 不含 `ws` ⇒ 改它不会触发「非法 URL pattern → 启动即崩」。
const WS_SERVICE: &str = "ws";

/// TraeWork 会把这些子树里的**每个非空字符串**拼成 webRequest 的 url pattern
/// （`Hp(i.bootConfig?.remote)` / `Hp(i.bootConfig?.agent)`，见模块文档）。
/// 写回前必须拿它们逐条模拟，否则 `http://…` 会把 TraeWork 拼成非法 pattern 并让它启动即崩。
const PATTERN_SERVICES: &[&str] = &["remote", "agent"];

/// 本机反代的端点基址 —— 也是写进 `product.json` 的那个值（**唯一来源**）。
///
/// TraeWork 会把 `bootConfig.remote/agent` 里每个字符串按「不是 `https://` 开头就补 `https://`」
/// 拼成 URL pattern，`http://…` 会被拼成 `https://http://…/*`（非法）并让它启动即崩。
/// 所以我们先给 TraeWork 打 [`crate::patch`] 的「免证书补丁」把这条约束放开，
/// 端点于是可以走**明文回环** —— 一个证书都不用装。
///
/// ⚠️ 只有 [`crate::patch::is_patched`] 为真时才允许写进 `product.json` ——
/// 这条不变量由 [`preflight`] / [`install`] 把关（见 [`gate_mode_for`]）。
///
/// 用 `127.0.0.1` 而不是 `localhost`：IP 不会受 hosts 文件 / DNS 搜索域之类的环境因素影响，
/// 改道风险更低。
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// 端点是否走明文（免证书模式）。
pub fn is_plain(endpoint_base: &str) -> bool {
    endpoint_base.starts_with("http://")
}

/// 把端点基址换成它对应的**实时通道** scheme：`http→ws`、`https→wss`，其余不动。
fn ws_base(endpoint_base: &str) -> String {
    match endpoint_base.split_once("://") {
        Some(("http", rest)) => format!("ws://{rest}"),
        Some((_, rest)) => format!("wss://{rest}"),
        None => format!("wss://{endpoint_base}"),
    }
}

/// 取 URL 的**路径**部分（含前导 `/`，去掉 query / fragment）。没有路径则返回空串。
///
/// WebSocket 端点必须**保留原始路径**：反代是按路径把 WS 请求路由回真正的 ws 上游的
/// （`wss://trae-ws-cn.mchost.guru/custom_model` 里的 `/custom_model`）。
/// 路径一丢，反代就分不清这条连接该转给谁。
fn url_path(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return String::new();
    };
    match rest.split_once('/') {
        Some((_, p)) => {
            let p = p.split(['?', '#']).next().unwrap_or("");
            if p.is_empty() {
                String::new()
            } else {
                format!("/{p}")
            }
        }
        None => String::new(),
    }
}

/// 某个服务「应该被写成什么值」。返回 `None` = **不归我们管，别碰**。
///
/// - 普通 HTTP 服务 → `endpoint_base`（`https://127.0.0.1:PORT` 或明文 `http://127.0.0.1:PORT`）；
/// - `ws` → 对应的 `wss://` / `ws://` + **原路径**，且**只在它原本就是一个非本机的 ws 地址时**才管
///   —— 键不存在就绝不发明一个出来（凭空造 `bootConfig.ws` 可能让 TraeWork 走上一条它本不该走的通道）。
fn patched_value_for(service: &str, endpoint_base: &str, original: Option<&Value>) -> Option<String> {
    if service == WS_SERVICE {
        let orig = original
            .and_then(Value::as_str)
            .filter(|s| !is_local(s) && s.starts_with("ws"))?;
        return Some(format!("{}{}", ws_base(endpoint_base), url_path(orig)));
    }
    Some(endpoint_base.to_string())
}

/// 复刻 TraeWork 的拼接规则。两种形态取决于**应用是否已打免证书补丁**：
///
/// ```js
/// // 原版（未打补丁）
/// c.startsWith("https://") ? `${c}/*` : `https://${c}/*`
/// // 打过补丁
/// c.includes("://")        ? `${c}/*` : `https://${c}/*`
/// ```
///
/// 单独抽出来是为了**能被测试直接钉住**——这条规则一旦理解错，代价是 TraeWork 起不来。
pub fn pattern_for_mode(value: &str, patched: bool) -> String {
    let already_has_scheme = if patched {
        value.contains("://")
    } else {
        value.starts_with("https://")
    };
    if already_has_scheme {
        format!("{value}/*")
    } else {
        format!("https://{value}/*")
    }
}

/// 未打补丁（原版应用）的拼接规则 —— 等价于 `pattern_for_mode(value, false)`。
///
/// 生产路径一律走 [`pattern_for_mode`]（它要跟着补丁状态走），这里只留给测试当参照物。
#[cfg(test)]
pub fn pattern_for(value: &str) -> String {
    pattern_for_mode(value, false)
}

/// 复刻 TraeWork 的 `Hp()`：递归收集所有**非空字符串**叶子。
fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Object(map) => map.values().for_each(|c| collect_strings(c, out)),
        Value::Array(items) => items.iter().for_each(|c| collect_strings(c, out)),
        _ => {}
    }
}

/// `http(s)://host[:port]/path` 形式的粗校验，足够覆盖 Electron 的 match pattern 要求：
/// 必须有 host、端口必须是数字、不许带用户名密码。
///
/// `http://` 也放行 —— 打过补丁的应用拼出来的明文 pattern 就是合法的
/// （Chromium 的 match pattern 本来就把 `http` 列为合法 scheme）。
fn pattern_is_valid(pattern: &str) -> bool {
    let Some(rest) = pattern
        .strip_prefix("https://")
        .or_else(|| pattern.strip_prefix("http://"))
    else {
        return false;
    };
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    if path.is_empty() || host_port.is_empty() || host_port.contains('@') {
        return false;
    }
    let host = match host_port.rsplit_once(':') {
        Some((h, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            h
        }
        None => host_port,
    };
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'*' | b'_'))
}

/// 模拟 TraeWork 的 `solo-lite-response-cors` 规则，返回第一处**会把它打挂**的值
/// `(原始值, 拼出来的 pattern)`。
///
/// 无问题时返回 `None`。注意范围是 `bootConfig.remote` / `bootConfig.agent` 下的**全部**
/// 非空字符串（不只是我们改的那几个），因为 TraeWork 就是这么收集的。
pub fn first_bad_pattern_mode(doc: &Value, patched: bool) -> Option<(String, String)> {
    let boot = doc.get("bootConfig")?;
    for service in PATTERN_SERVICES {
        let Some(node) = boot.get(*service) else {
            continue;
        };
        let mut strings = Vec::new();
        collect_strings(node, &mut strings);
        for raw in strings {
            let pattern = pattern_for_mode(&raw, patched);
            if !pattern_is_valid(&pattern) {
                return Some((raw, pattern));
            }
        }
    }
    None
}

/// 未打补丁（原版应用）视角下的闸门检查 —— 生产路径走 [`first_bad_pattern_mode`]。
#[cfg(test)]
pub fn first_bad_pattern(doc: &Value) -> Option<(String, String)> {
    first_bad_pattern_mode(doc, false)
}

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
    cands.into_iter().find(|p| p.join(PRODUCT_FILE).exists())
}

/// `product.json` 的路径。
pub fn product_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join(PRODUCT_FILE))
}

// ---------------------------------------------------------------------------
// JSON 路径工具（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// `["bootConfig", "remote", "trae", "normal"]`：被改写的键路径。
fn target_path(service: &str) -> [&str; 4] {
    ["bootConfig", service, "trae", "normal"]
}

fn get_path<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = root;
    for k in path {
        cur = cur.get(*k)?;
    }
    Some(cur)
}

/// 写入键值（父级不存在则创建）。返回 `true` 表示确实改变了。
fn set_path(root: &mut Value, path: &[&str], value: Value) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut cur = root;
    for k in parents {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        cur = cur
            .as_object_mut()
            .expect("上面刚保证是对象")
            .entry((*k).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cur.is_object() {
        *cur = Value::Object(Map::new());
    }
    let map = cur
        .as_object_mut()
        .expect("上面刚保证是对象");
    let changed = map.get(*last) != Some(&value);
    map.insert((*last).to_string(), value);
    changed
}

/// 删除键（还原「原本不存在」的键），并把因此变空的祖先对象一并剪掉 ——
/// 否则会留下 `{"hub":{"trae":{}}}` 这种空壳，还原就不等于原始文档了。
fn remove_path(root: &mut Value, path: &[&str]) -> bool {
    if !remove_leaf(root, path) {
        return false;
    }
    for cut in (1..path.len()).rev() {
        let empty = get_path(root, &path[..cut])
            .and_then(Value::as_object)
            .map(Map::is_empty)
            .unwrap_or(false);
        if empty {
            remove_leaf(root, &path[..cut]);
        }
    }
    true
}

/// 只摘掉最后一个键，不动祖先。
fn remove_leaf(root: &mut Value, path: &[&str]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut cur = root;
    for k in parents {
        match cur.get_mut(*k) {
            Some(v) => cur = v,
            None => return false,
        }
    }
    cur.as_object_mut()
        .map(|o| o.remove(*last).is_some())
        .unwrap_or(false)
}

/// 是不是本机地址（防止把「已改写过的值」当成原始上游，导致反代自环）。
fn is_local(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("127.0.0.1") || s.contains("localhost") || s.contains("0.0.0.0")
}

// ---------------------------------------------------------------------------
// 上游域名解析（优先取标记里记的原值，绝不硬编码）
// ---------------------------------------------------------------------------

/// 在 JSON 子树上按前缀深度优先找第一个字符串（区域键名不稳定，故按值前缀匹配）。
fn first_prefixed(v: &Value, prefix: &str) -> Option<String> {
    match v {
        Value::String(s) if s.starts_with(prefix) && !is_local(s) => Some(s.clone()),
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

/// 原始上游地址 `(http, ws)`。
///
/// **顺序很重要**：`product.json` 被我们改写过之后，这两个键已经是本地地址，
/// 直接读文件会解析出 `127.0.0.1`，反代就会把请求转给自己（自环）。
/// 所以先看标记里记录的原值，取不到才回落到读文件（此时 [`first_prefixed`] 会跳过本机地址）。
///
/// 返回的是**完整地址**（含路径）：`ws` 那条带 `/custom_model` 之类的路径，
/// 反代要靠它把 WS 连接路由回真正的上游。
pub fn read_upstreams() -> (Option<String>, Option<String>) {
    let Some(path) = product_path() else {
        return (None, None);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (None, None);
    };
    let recorded = |service: &str| -> Option<String> {
        v.get(MARKER_KEY)
            .and_then(|m| m.get("original"))
            .and_then(|o| o.get(service))
            .and_then(Value::as_str)
            .filter(|s| !is_local(s))
            .map(str::to_string)
    };
    let boot = v.get("bootConfig");
    let http = recorded("remote").or_else(|| {
        boot.and_then(|b| b.get("remote"))
            .and_then(|r| pick_domain(r, "http"))
    });
    let ws = recorded(WS_SERVICE).or_else(|| {
        boot.and_then(|b| b.get(WS_SERVICE))
            .and_then(|w| pick_domain(w, "ws"))
    });
    (http, ws)
}

// ---------------------------------------------------------------------------
// 改写与还原（纯函数部分，便于单测）
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

/// 该文档是否带本助手的标记。
pub fn doc_is_ours(doc: &Value) -> bool {
    doc.get(MARKER_KEY).is_some()
}

/// 文档里的目标键是否**都**已指向 `endpoint_base`（= 改写是否仍然生效）。
///
/// 逐个服务按 [`patched_value_for`] 算出期望值来比 —— `ws` 的期望值不是 `endpoint_base`
/// 而是 `wss://…` + 原路径，所以不能拿同一个字符串套所有服务。
/// 「不归我们管」的服务（例如本来就没有 `ws` 键）直接算通过。
pub fn doc_points_at(doc: &Value, endpoint_base: &str) -> bool {
    let marker = doc.get(MARKER_KEY);
    PATCHED_SERVICES.iter().all(|s| {
        let recorded = marker
            .and_then(|m| m.get("original"))
            .and_then(|o| o.get(*s));
        match patched_value_for(s, endpoint_base, recorded) {
            None => true,
            Some(want) => {
                get_path(doc, &target_path(s)).and_then(Value::as_str) == Some(want.as_str())
            }
        }
    })
}

/// 改写：把目标键指向本机反代，并写入带原值的标记。
///
/// 原值以 `null` 表示「该键原本不存在」，还原时据此删除。已经是我们的文档时**保留原值**，
/// 只更新 `endpoint_base`（改端口不该把原值记成上一次的本地地址）。
///
/// ⚠️ 记录里**没有**某个服务 = 「我们没碰过它」，还原时必须原样留着（见 [`restore_doc`]）。
fn patch_doc(doc: &mut Value, endpoint_base: &str) -> bool {
    let mut changed = false;
    let ours = doc_is_ours(doc);
    let mut original = Map::new();
    for s in PATCHED_SERVICES {
        let path = target_path(s);
        let prev = get_path(doc, &path).cloned();
        // 已是我们的文档 → 沿用记录里的原值；否则取文档当前值（本机地址视为「原本就没有」）
        let recorded = if ours {
            doc.get(MARKER_KEY)
                .and_then(|m| m.get("original"))
                .and_then(|o| o.get(*s))
                .cloned()
        } else {
            prev.filter(|v| v.is_string() && !is_local(v.as_str().unwrap_or("")))
        };
        let Some(want) = patched_value_for(s, endpoint_base, recorded.as_ref()) else {
            continue; // 不归我们管（例如本来就没有 ws 键）
        };
        original.insert((*s).to_string(), recorded.unwrap_or(Value::Null));
        changed |= set_path(doc, &path, Value::String(want));
    }
    doc[MARKER_KEY] = json!({
        "installed_at": now_string(),
        "endpoint_base": endpoint_base,
        "original": Value::Object(original),
    });
    changed
}

/// 还原：按标记里记录的原值逐字段写回（`null` = 删掉），并移除标记。
///
/// 返回 `true` 表示文档内容发生了变化。
///
/// **只处理记录里出现过的服务** —— 没记过 = 我们没碰过它。这条不只为了 `ws`：
/// 老版本写下的标记里没有 `ws` 这一项，若按「缺了就删」处理，升级助手后一次还原
/// 就会把用户原本的 `bootConfig.ws` 删掉。
fn restore_doc(doc: &mut Value) -> bool {
    let Some(marker) = doc.get(MARKER_KEY).cloned() else {
        return false;
    };
    let Some(orig) = marker.get("original").and_then(Value::as_object) else {
        return false;
    };
    for s in PATCHED_SERVICES {
        let Some(recorded) = orig.get(*s) else {
            continue;
        };
        match recorded {
            Value::String(v) => {
                set_path(doc, &target_path(s), Value::String(v.clone()));
            }
            _ => {
                remove_path(doc, &target_path(s));
            }
        }
    }
    doc.as_object_mut()
        .map(|o| o.remove(MARKER_KEY).is_some())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 读写（原子写 + 备份）
// ---------------------------------------------------------------------------

fn read_product(path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("解析 {} 失败：{e}", path.display()))
}

/// 原子写：同目录临时文件 → `rename`。任何一步失败都不会留下半个 JSON。
fn write_product(path: &Path, doc: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(doc).map_err(|e| format!("序列化失败：{e}"))?;
    let tmp = path.with_file_name(TMP_FILE);
    std::fs::write(&tmp, text).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换 {} 失败：{e}", path.display()))?;
    Ok(())
}

/// 首次改写前留一份整文件备份（已存在则不动，永不覆盖）。
fn backup_once(app: &Path, path: &Path) {
    let bak = app.join(BACKUP_FILE);
    if !bak.exists() {
        let _ = std::fs::copy(path, &bak);
    }
}

/// 真写一次探针，判断安装目录**实际可写**。
///
/// 只查权限位是不够的：macOS 的「App 管理」TCC 会对写别的 app 包返回 `EPERM`，
/// 只读卷（DMG / 只读镜像）同理——都必须靠真写才能发现。
fn writable_probe(app: &Path) -> bool {
    let probe = app.join(".twa_write_probe");
    let ok = std::fs::write(&probe, b"1").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// 某个文件**所在目录**是否实际可写。
///
/// 给 [`crate::patch`] 用：补丁是「同目录写临时文件再 rename」，所以判据是目录而不是文件本身。
/// 同样必须真写一次 —— macOS「App 管理」TCC 只有落到写操作上才会回 `EPERM`。
pub fn is_writable_file(path: &Path) -> bool {
    path.parent().map(writable_probe).unwrap_or(false)
}

/// 从可执行文件路径里找出它所属的 `.app` 包（开发模式没有，返回 `None`）。
///
/// 形态是固定的 `<X>.app/Contents/MacOS/<bin>`，所以只看路径里的 `.app` 那一段。
fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    let mut cur = exe;
    loop {
        let parent = cur.parent()?;
        if parent
            .extension()
            .map_or(false, |e| e.eq_ignore_ascii_case("app"))
        {
            return Some(parent.to_path_buf());
        }
        cur = parent;
    }
}

/// 「App 管理」里**到底该授权哪一项** —— 由本进程自己的形态决定。
///
/// 这一段必须说，因为 TCC 是按请求进程的**代码身份**判定的，而本助手的指定要求
/// 就是一串 `cdhash`（`codesign -d -r-` 可查），于是：
///
/// - **应用包形态**：列表里找得到对应的 App，照它授权、重启本助手即可；
/// - **开发模式（裸二进制）**：列表里**没有**对应项 —— 用户在「App 管理」里开的任何开关
///   都落不到它身上。不说这句，用户就会以为「照提示开了权限还是不行」（真机踩过）。
///
/// 还有一条两者共通的铁律：临时(adhoc)签名的身份**每次重新编译都会变**
/// （`cdhash` 变 ⇒ 授权失效），所以授权后别再编译，或准备再授权一次。
fn authorize_hint() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "请在「系统设置 → 隐私与安全性 → App 管理」中允许本助手。".to_string();
    };
    match app_bundle_of(&exe) {
        Some(app) => {
            let name = app
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "本助手".to_string());
            format!(
                "请在「系统设置 → 隐私与安全性 → App 管理」中允许「{name}」（{}）；\
                 授权后必须**重启本助手**才生效，且**别在授权后再编译** —— \
                 助手是临时(adhoc)签名，二进制一变（cdhash 变）授权即失效。",
                app.display()
            )
        }
        None => format!(
            "⚠️ 本次运行的是**开发模式**的裸二进制（{}）：它没有应用包，\
             「App 管理」列表里没有对应项 —— 在那里开的授权落不到它身上。\
             请改用应用包形态运行，或参照 `docs/免证书接管方案.md` 给二进制固定签名身份。",
            exe.display()
        ),
    }
}

/// 「写不进去」的**唯一**话术源 —— [`status`] 与 [`crate::patch`] 共用一份，免得两处说法分叉。
///
/// 成因只有一个：macOS 的「App 管理」(TCC) 会拦住对**已签名**应用包的修改，
/// 按请求进程的代码身份判定，**绕开 sandbox / 加 sudo 都没用**。判据也只有一个：真写一次。
pub fn unwritable_hint(path: &Path) -> String {
    format!(
        "macOS 的「App 管理」拦住了对已签名应用包的修改（{}）。{}",
        path.display(),
        authorize_hint()
    )
}

// ---------------------------------------------------------------------------
// 租约（心跳）
// ---------------------------------------------------------------------------

fn lease_path(dir: &Path) -> PathBuf {
    dir.join(LEASE_FILE)
}

/// 网关运行期间刷新租约。
pub fn touch_lease(dir: &Path) {
    let _ = std::fs::write(lease_path(dir), json!({ "at": now_ms() }).to_string());
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
    /// `product.json` 里的端点是否**已指向本机反代**（= 改写仍生效）。
    pub installed: bool,
    /// `product.json` 是否带本助手标记（带标记才能一键精确还原）。
    pub ours: bool,
    /// 安装目录是否可写（权限）。
    pub writable: bool,
    /// 本机网关端点基址（`https://127.0.0.1:PORT`，即改写写入 `remote.trae.normal` 的值）。
    pub endpoint_base: String,
    /// 原始上游（取自标记记录或 product.json，便于界面展示与排障）。
    pub upstream_http: Option<String>,
    pub upstream_ws: Option<String>,
    pub lease_fresh: bool,
    pub message: String,
}

/// 读取当前状态（只读，不修改任何文件）。
pub fn status(dir: &Path, endpoint_base: &str) -> EndpointStatus {
    let app = app_dir();
    let (up_http, up_ws) = read_upstreams();
    let doc = product_path().and_then(|p| read_product(&p).ok());
    let installed = doc.as_ref().map(|d| doc_points_at(d, endpoint_base)).unwrap_or(false);
    let ours = doc.as_ref().map(doc_is_ours).unwrap_or(false);
    let writable = app.as_ref().map(|d| writable_probe(d)).unwrap_or(false);
    let message = if app.is_none() {
        "未找到 TraeWork 安装目录，本机不支持「智能接管」。".to_string()
    } else if !writable {
        // 「不可写」的成因与处置**只有一份话术**（与 `crate::patch` 共用），这里只补上下文。
        format!(
            "TraeWork 安装目录不可写，无法改写端点配置。{}",
            unwritable_hint(app.as_ref().map(|p| p.as_path()).unwrap_or(Path::new("")))
        )
    } else if installed && !ours {
        "TraeWork 的 product.json 已指向本机反代，但没带本助手的标记——助手不会改动也不会还原它。"
            .to_string()
    } else if installed {
        "接管已生效：TraeWork 的模型/会话请求将经本机反代按账号池转发。".to_string()
    } else if ours {
        "product.json 带本助手标记，但端点未指向本机反代（端口或版本变了），重新开启接管可修复。"
            .to_string()
    } else {
        "未接管：TraeWork 仍直连官方。".to_string()
    };
    EndpointStatus {
        supported: app.is_some(),
        app_dir: app.map(|d| d.display().to_string()),
        installed,
        ours,
        writable,
        endpoint_base: endpoint_base.to_string(),
        upstream_http: up_http,
        upstream_ws: up_ws,
        lease_fresh: lease_fresh(dir),
        message,
    }
}

// ---------------------------------------------------------------------------
// 改写 / 还原 / 清扫 / 修复
// ---------------------------------------------------------------------------

/// 值会被 TraeWork 拼成非法 pattern 时的统一措辞（预检与写盘共用一套话）。
fn blocked_message(raw: &str, pattern: &str) -> String {
    let hint = if is_plain(raw) {
        "\n想用明文端点，就得先给 TraeWork 打「免证书补丁」\
         （接管页一键完成，可整份还原、不需要备份文件）。"
    } else {
        ""
    };
    format!(
        "已阻止改写，TraeWork 未被改动。\n\
         TraeWork 会把 product.json 里 bootConfig.remote/agent 的每个地址拼成 URL pattern，\
         而这个值会被拼成非法串（{pattern}），导致它启动即崩。\n\
         当前值：{raw}。{hint}"
    )
}

/// 判定该用哪一套闸门规则，并把「端点协议 ↔ 补丁状态」这条不变量钉在这里。
///
/// - 端点 `http://`（**现在唯一会写的形态**）→ **必须**已打补丁，否则就地拒绝；
/// - 端点 `https://` → 原版闸门即可（已无写入方，保留是当对照组）。
///
/// 返回 `patched`：TraeWork 当前真实的闸门形态（打过补丁的应用两种 scheme 都接受）。
fn gate_mode_for(endpoint_base: &str) -> Result<bool, String> {
    let patched = crate::patch::is_patched();
    if is_plain(endpoint_base) && !patched {
        return Err(format!(
            "端点 {endpoint_base} 需要先给 TraeWork 打「免证书补丁」。\n\
             未打补丁时它的闸门只接受 https://，会把 http://… 拼成 https://http://…/*（非法 port），\
             让 TraeWork 启动即崩。\n\
             在接管页点一次「打免证书补丁」即可 —— 补丁可逐字节还原，之后再开接管。"
        ));
    }
    Ok(patched)
}

/// **只读**预检：在内存里模拟一次改写，跑 TraeWork 的 pattern 规则，一个字节都不写。
///
/// `enable_blocking` 在动任何东西（开关、反代、TraeWork 进程）**之前**先调它——
/// 不通过就直接报错，避免留下「开关开着 / 反代占着端口 / TraeWork 被关掉」的半开状态。
pub fn preflight(endpoint_base: &str) -> Result<(), String> {
    let patched = gate_mode_for(endpoint_base)?;
    let Some(app) = app_dir() else {
        return Ok(()); // 不支持改写的机器交给上层分支去解释
    };
    let path = app.join(PRODUCT_FILE);
    if !path.exists() {
        return Ok(());
    }
    let mut doc = read_product(&path)?;
    patch_doc(&mut doc, endpoint_base);
    match first_bad_pattern_mode(&doc, patched) {
        Some((raw, pattern)) => Err(blocked_message(&raw, &pattern)),
        None => Ok(()),
    }
}

/// 改写 `product.json`，把 5 个端点指向 `endpoint_base`。
///
/// **调用方必须保证反代已在监听 `endpoint_base`**，否则会让 TraeWork 全量失败。
/// 幂等：已经是我们的文档时只做必要的更新（改端口）。
///
/// **fail-closed**：先在内存里改完并跑 `first_bad_pattern()`（模拟 TraeWork 的
/// webRequest pattern 规则），过不了就**一个字节都不写**，直接带原因返回。
pub fn install(dir: &Path, endpoint_base: &str) -> Result<EndpointStatus, String> {
    let patched = gate_mode_for(endpoint_base)?;
    let app = app_dir().ok_or_else(|| "未找到 TraeWork 安装目录".to_string())?;
    let path = app.join(PRODUCT_FILE);
    let mut doc = read_product(&path)?;
    let ours = doc_is_ours(&doc);
    let changed = patch_doc(&mut doc, endpoint_base);

    // ① 闸门：写成 `http://…` 会让**未打补丁**的 TraeWork 启动即崩（见模块文档），
    //    所以这里必须先模拟它自己的 pattern 拼接规则，不通过就绝不落盘。
    if changed {
        if let Some((raw, pattern)) = first_bad_pattern_mode(&doc, patched) {
            crate::journal::append(
                dir,
                "install_blocked",
                &format!("已阻止改写：{raw} 会被 TraeWork 拼成非法 URL pattern（{pattern}），会导致它启动即崩"),
            );
            return Err(blocked_message(&raw, &pattern));
        }
    }

    // ② 先探针再动手：只读卷 / macOS「App 管理」TCC 会让写入直接 EPERM，
    //    与其写到一半失败，不如带着可操作的提示提前返回。
    if !writable_probe(&app) {
        return Err(format!(
            "TraeWork 安装目录不可写，无法改写端点配置。{}",
            unwritable_hint(&app)
        ));
    }

    if changed && !ours {
        backup_once(&app, &path);
    }
    if changed {
        write_product(&path, &doc)?;
    }
    touch_lease(dir);
    if changed {
        crate::journal::append(
            dir,
            "install",
            &format!(
                "已改写 TraeWork 端点配置（bootConfig 的模型/会话地址 + 实时通道），请求改道 {endpoint_base}"
            ),
        );
    }
    Ok(status(dir, endpoint_base))
}

/// 还原 `product.json`（幂等）。**只处理带本助手标记的文档**，别人的改动绝不触碰。
///
/// 返回是否发生了还原。
pub fn uninstall(dir: &Path) -> Result<bool, String> {
    let Some(app) = app_dir() else {
        return Ok(false);
    };
    let path = app.join(PRODUCT_FILE);
    if !path.exists() {
        return Ok(false);
    }
    let mut doc = read_product(&path)?;
    if !doc_is_ours(&doc) {
        return Ok(false);
    }
    if restore_doc(&mut doc) {
        write_product(&path, &doc)?;
    }
    // 备份只服务于「被改写」这段时期，还原后一并清掉，保持安装目录干净
    let _ = std::fs::remove_file(app.join(BACKUP_FILE));
    clear_lease(dir);
    crate::journal::append(dir, "uninstall", "已还原 TraeWork 端点配置，恢复官方直连（重启后生效）");
    Ok(true)
}

/// 启动清扫：**只在本地反代确实不可用时**才还原，恢复 TraeWork 官方直连。
///
/// `keep=true` 的条件是 `takeover_enabled && 反代已在监听`——此时端点指向的端口是活的，
/// TraeWork 启动后能正常走池化，配置应保留。除此之外一律还原，避免把 TraeWork 指向死端口。
///
/// 返回是否发生了还原。
pub fn sweep(dir: &Path, keep: bool) -> bool {
    if keep {
        return false;
    }
    let Some(path) = product_path() else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    let Ok(mut doc) = read_product(&path) else {
        return false;
    };
    if !doc_is_ours(&doc) {
        return false;
    }
    if !restore_doc(&mut doc) || write_product(&path, &doc).is_err() {
        return false;
    }
    if let Some(app) = app_dir() {
        let _ = std::fs::remove_file(app.join(BACKUP_FILE));
    }
    eprintln!("[接管] 已还原 TraeWork 端点配置（本地反代不可用）");
    clear_lease(dir);
    crate::journal::append(
        dir,
        "sweep",
        "启动时发现端点改写残留且本地反代不可用，已自动还原 TraeWork 官方直连",
    );
    true
}

/// 自愈：TraeWork 升级会整份替换 `product.json`，我们的端点改写随之丢失。
///
/// 返回**当前是否已就绪**（端点确已指向 `endpoint_base`）：已经就绪就不写盘；
/// 需要时补写并记一条动态（此时必须重启 TraeWork 才生效）。调用方据此决定下次检查的间隔。
pub fn repair(dir: &Path, endpoint_base: &str) -> bool {
    let Some(path) = product_path() else {
        return false;
    };
    let Ok(doc) = read_product(&path) else {
        return false;
    };
    if doc_points_at(&doc, endpoint_base) {
        return true;
    }
    let was_ours = doc_is_ours(&doc);
    if install(dir, endpoint_base).is_err() {
        return false;
    }
    if !was_ours {
        // 升级后配置被整份换掉：明确告诉用户「要重启 TraeWork 才生效」
        crate::journal::append(
            dir,
            "install",
            "TraeWork 升级后端点配置被覆盖，已自动重新改写（重启 TraeWork 后生效）",
        );
    }
    true
}

// ---------------------------------------------------------------------------
// TraeWork 进程控制：端点配置只在 **启动时** 被读取，故改完必须重启才生效
// ---------------------------------------------------------------------------

/// 候选 TraeWork 进程映像名（Windows `tasklist` 中的 ImageName）。
/// 刻意不包含本助手自身，避免误判。
#[cfg(target_os = "windows")]
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
    let out = op();
    // 不管 op 成败都要把它拉回来：`op` 被闸门挡住时 TraeWork 已经被我们关掉了，
    // 不能因为返回 Err 就把它留在关闭状态（2026-09-14 实测踩过这个坑）。
    if was_running {
        let _ = relaunch();
    }
    Ok((out?, was_running))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product_stub() -> Value {
        serde_json::from_str(
            r#"{
              "name": "TRAE SOLO CN",
              "bootConfig": {
                "remote": { "trae": { "normal": "https://trae-api-cn.mchost.guru" }, "bytedance": { "normal": "" } },
                "agent":  { "trae": { "normal": "https://trae-api-cn.mchost.guru" }, "appId": "a" },
                "ckg":    { "trae": { "normal": "https://trae-api-cn.mchost.guru" } },
                "cue":    { "trae": { "normal": "https://trae-api-cn.mchost.guru" } },
                "hub":    { "trae": { "normal": "https://trae-api-cn.mchost.guru" } },
                "ws":     { "trae": { "normal": "wss://trae-ws-cn.mchost.guru/custom_model" } },
                "account":{ "trae": { "normal": "https://api.trae.cn" } }
              }
            }"#,
        )
        .unwrap()
    }

    /// 我们**实际会写进** `product.json` 的端点值（本地反代讲 TLS，端点必须是 https）。
    const LOCAL: &str = "https://127.0.0.1:8788";
    /// 反面样本：纯 HTTP 端点 —— 2026-09-14「开启接管后 Trae 打不开」的事故现场。
    const LOCAL_PLAIN: &str = "http://127.0.0.1:8788";

    #[test]
    fn writable_probe_reflects_reality() {
        let dir = std::env::temp_dir().join(format!("twa-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(writable_probe(&dir), "临时目录应当可写");
        // 探针必须自清理，别在人家安装目录里留垃圾
        assert!(!dir.join(".twa_write_probe").exists(), "探针文件应当被删掉");

        let missing = dir.join("no-such-dir");
        assert!(!writable_probe(&missing), "不存在的目录不可写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「该授权给谁」完全取决于本进程的形态，所以先单测这个判据本身。
    #[test]
    fn app_bundle_is_found_only_for_a_bundled_binary() {
        assert_eq!(
            app_bundle_of(Path::new("/Applications/Foo.app/Contents/MacOS/foo")).as_deref(),
            Some(Path::new("/Applications/Foo.app")),
            "应用包形态应当能回溯出 .app 目录"
        );
        assert!(
            app_bundle_of(Path::new("/x/src-tauri/target/debug/traework-assistant")).is_none(),
            "开发模式的裸二进制没有应用包"
        );
        assert!(app_bundle_of(Path::new("")).is_none(), "空路径不应 panic");
    }

    /// 话术必须点名「被拦的是哪个文件」，否则用户拿着它无法判断自己开对了开关没有。
    #[test]
    fn unwritable_hint_names_the_blocked_path() {
        let h = unwritable_hint(Path::new("/Applications/X.app/Contents/Resources/app/out/main.js"));
        assert!(h.contains("out/main.js"), "应当点名被拦的文件：{h}");
        assert!(h.contains("App 管理"), "应当给出授权入口：{h}");
    }

    #[test]
    fn patch_points_every_service_at_local_and_keeps_originals() {
        let mut doc = product_stub();
        assert!(!doc_points_at(&doc, LOCAL));
        assert!(patch_doc(&mut doc, LOCAL));
        assert!(doc_points_at(&doc, LOCAL), "所有受管服务都要指到本机");
        assert!(doc_is_ours(&doc), "必须带自识别标记");
        let orig = &doc[MARKER_KEY]["original"];
        for s in PATCHED_SERVICES {
            // ws 的原值和别人不一样（独立主机 + 独立路径），逐个服务按期望值比
            let want = if *s == WS_SERVICE {
                "wss://trae-ws-cn.mchost.guru/custom_model"
            } else {
                "https://trae-api-cn.mchost.guru"
            };
            assert_eq!(orig[*s], want, "{s} 的原值必须被记录下来，才能精确还原");
        }
        // ws 写进去的必须是 wss:// 且**保留原路径** —— 反代靠这个路径把 WS 路由回真正的上游
        assert_eq!(
            doc["bootConfig"]["ws"]["trae"]["normal"],
            "wss://127.0.0.1:8788/custom_model"
        );
    }

    /// `ws` 会被改写（实时通道也纳入接管），但 `account` 绝不能碰 ——
    /// 它承担登录 / token 续签，动了就断官方链路。
    #[test]
    fn patch_rewrites_ws_but_still_leaves_account_alone() {
        let mut doc = product_stub();
        patch_doc(&mut doc, LOCAL);
        assert_eq!(
            doc["bootConfig"]["ws"]["trae"]["normal"], "wss://127.0.0.1:8788/custom_model",
            "ws 应当被改写到本机，且路径原样保留"
        );
        assert_eq!(doc["bootConfig"]["account"]["trae"]["normal"], "https://api.trae.cn");
    }

    /// 文档里**没有** `ws` 键时：不发明、也不在还原时误删别的东西。
    #[test]
    fn patch_never_invents_a_ws_key() {
        let mut doc = product_stub();
        doc["bootConfig"].as_object_mut().unwrap().remove("ws");
        let before = doc.clone();
        patch_doc(&mut doc, LOCAL);
        assert!(
            doc["bootConfig"].get("ws").is_none(),
            "原本没有 ws 键就不该凭空造一个出来"
        );
        assert!(
            doc[MARKER_KEY]["original"].get("ws").is_none(),
            "没碰过的服务不该进 original 记录"
        );
        restore_doc(&mut doc);
        assert_eq!(doc, before, "还原后应当与改写前完全一致");
    }

    /// 老版本留下的标记里没有 `ws` 项：还原时**必须原样保留** ws（不能按「缺了就删」处理）。
    #[test]
    fn restore_leaves_ws_alone_when_the_marker_predates_it() {
        let mut doc = product_stub();
        patch_doc(&mut doc, LOCAL);
        // 模拟老标记：把 ws 这一项从记录里摘掉，并把 ws 恢复成官方值
        doc[MARKER_KEY]["original"]
            .as_object_mut()
            .unwrap()
            .remove(WS_SERVICE);
        doc["bootConfig"]["ws"]["trae"]["normal"] =
            serde_json::json!("wss://trae-ws-cn.mchost.guru/custom_model");
        restore_doc(&mut doc);
        assert_eq!(
            doc["bootConfig"]["ws"]["trae"]["normal"], "wss://trae-ws-cn.mchost.guru/custom_model",
            "记录里没有 ws = 我们没碰过它，还原必须留着它"
        );
        assert!(!doc_is_ours(&doc));
    }

    #[test]
    fn restore_brings_back_exact_originals() {
        let mut doc = product_stub();
        let before = doc.clone();
        patch_doc(&mut doc, LOCAL);
        assert!(restore_doc(&mut doc));
        assert_eq!(doc, before, "还原必须逐字节回到改写前的样子");
        assert!(!doc_is_ours(&doc), "标记也要清掉");
        assert!(!restore_doc(&mut doc), "再还原一次应当无事发生（幂等）");
    }

    #[test]
    fn restore_removes_keys_that_did_not_exist() {
        // 原文档没有 hub 这个服务：还原时应当把它删掉，而不是留一个空壳
        let mut doc = product_stub();
        doc["bootConfig"].as_object_mut().unwrap().remove("hub");
        patch_doc(&mut doc, LOCAL);
        assert!(doc_points_at(&doc, LOCAL));
        restore_doc(&mut doc);
        assert!(doc["bootConfig"].get("hub").is_none(), "原本不存在就应删干净");
        assert_eq!(doc["bootConfig"]["remote"]["trae"]["normal"], "https://trae-api-cn.mchost.guru");
    }

    #[test]
    fn reinstall_keeps_original_values_and_tracks_new_port() {
        let mut doc = product_stub();
        patch_doc(&mut doc, LOCAL);
        patch_doc(&mut doc, "http://127.0.0.1:9999");
        assert!(doc_points_at(&doc, "http://127.0.0.1:9999"));
        // 改端口不能把原值污染成上一次的本地地址，否则还原就回不去了
        assert_eq!(
            doc[MARKER_KEY]["original"]["remote"], "https://trae-api-cn.mchost.guru"
        );
        assert!(!patch_doc(&mut doc, "http://127.0.0.1:9999"), "重复改写不应反复写盘");
    }

    #[test]
    fn foreign_doc_is_never_restored() {
        let mut foreign: Value = serde_json::from_str(
            r#"{"bootConfig":{"remote":{"trae":{"normal":"http://127.0.0.1:1234"}}}}"#,
        )
        .unwrap();
        let before = foreign.clone();
        assert!(!doc_is_ours(&foreign));
        assert!(!restore_doc(&mut foreign), "没有标记就不认为是自己改的");
        assert_eq!(foreign, before);
    }

    #[test]
    fn upstream_never_resolves_to_localhost() {
        // 自环防护：改写之后 remote.trae.normal 已是本地地址，深搜必须跳过它
        let v: Value = serde_json::from_str(
            r#"{"trae":{"normal":"http://127.0.0.1:8788"},"overseas":{"normal":"https://trae-api-us.mchost.guru"}}"#,
        )
        .unwrap();
        assert_eq!(
            pick_domain(&v, "http").as_deref(),
            Some("https://trae-api-us.mchost.guru")
        );
    }

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
        eprintln!("[smoke] product.json = {:?}", product_path());
    }

    // -----------------------------------------------------------------------
    // URL pattern 闸门：2026-09-14「开启接管后 Trae 打不开」事故的回归测试
    // -----------------------------------------------------------------------

    #[test]
    fn reproduces_traework_pattern_rule() {
        // 原版：`c.startsWith("https://") ? `${c}/*` : `https://${c}/*``
        assert_eq!(pattern_for("https://a.example"), "https://a.example/*");
        assert_eq!(pattern_for("http://127.0.0.1:8788"), "https://http://127.0.0.1:8788/*");
        assert_eq!(pattern_for("wss://x/y"), "https://wss://x/y/*");

        // 打过免证书补丁：`c.includes("://") ? `${c}/*` : `https://${c}/*``
        assert_eq!(pattern_for_mode("https://a.example", true), "https://a.example/*");
        assert_eq!(
            pattern_for_mode("http://127.0.0.1:8788", true),
            "http://127.0.0.1:8788/*",
            "补丁后明文端点必须能过闸"
        );
        assert_eq!(pattern_for_mode("bare.example", true), "https://bare.example/*");

        assert!(pattern_is_valid("https://trae-api-cn.mchost.guru/*"));
        assert!(pattern_is_valid("https://127.0.0.1:8788/*"));
        assert!(pattern_is_valid("http://127.0.0.1:8788/*"), "补丁后明文 pattern 合法");
        assert!(pattern_is_valid("https://*.example.com/*"), "通配符 host 合法");
        // 事故现场那条 pattern
        assert!(!pattern_is_valid("https://http://127.0.0.1:8788/*"));
        assert!(!pattern_is_valid("https://127.0.0.1:/*"), "空端口非法");
        assert!(!pattern_is_valid("http://127.0.0.1:/*"), "明文也一样：空端口非法");
        assert!(!pattern_is_valid("ftp://h/*"), "其它 scheme 不在白名单");
        assert!(!pattern_is_valid("https://user:pw@h/*"), "带用户名密码非法");
    }

    /// 免证书模式的核心不变量：**明文端点只有在补丁视角下才合法**。
    #[test]
    fn plain_endpoint_is_only_legal_under_the_patched_gate() {
        let mut doc = product_stub();
        patch_doc(&mut doc, LOCAL_PLAIN);
        assert!(
            first_bad_pattern_mode(&doc, false).is_some(),
            "原版闸门必须拦下明文端点"
        );
        assert!(
            first_bad_pattern_mode(&doc, true).is_none(),
            "补丁后闸门必须放行明文端点"
        );
        // 打过补丁的应用对 https 端点依然放行 ⇒ 还原补丁不会弄坏既有接管
        let mut https_doc = product_stub();
        patch_doc(&mut https_doc, LOCAL);
        assert!(first_bad_pattern_mode(&https_doc, true).is_none());
    }

    /// `ws` 的 scheme 必须跟着端点走：https→wss，http→ws。
    #[test]
    fn ws_scheme_follows_the_endpoint() {
        let wss = json!("wss://trae-ws-cn.mchost.guru/custom_model");
        assert_eq!(
            patched_value_for(WS_SERVICE, &base_url(8788), Some(&wss)).unwrap(),
            "ws://127.0.0.1:8788/custom_model",
            "免证书端点走明文，实时通道必须跟着变成 ws://"
        );
        // `https` 端点已不再被写入，但 scheme 映射必须双向都正确 ——
        // 它是「闸门规则复刻得对不对」的对照组（见模块文档）。
        assert_eq!(
            patched_value_for(WS_SERVICE, LOCAL, Some(&wss)).unwrap(),
            "wss://127.0.0.1:8788/custom_model"
        );
        assert!(is_plain(&base_url(1)));
        assert!(!is_plain(LOCAL));
    }

    #[test]
    fn http_local_endpoint_is_rejected_exactly_like_traework_would() {
        assert!(
            first_bad_pattern(&product_stub()).is_none(),
            "官方配置本身必须过闸（否则会在别人的机器上误伤）"
        );
        let mut doc = product_stub();
        patch_doc(&mut doc, LOCAL_PLAIN);
        let (raw, pattern) = first_bad_pattern(&doc).expect("http:// 本地端点必须被拦下");
        assert_eq!(raw, LOCAL_PLAIN);
        assert_eq!(pattern, "https://http://127.0.0.1:8788/*");
    }

    /// ⚠️ 这个端点**已经不会被写入**（证书模式已移除）。保留它不为兼容，而是当作
    /// 闸门复刻忠实性的**对照组**：原版 TraeWork 只接受 `https://`，这条断言就是
    /// 「我们确实复刻了它的规则」的证据 —— 复刻一旦漂了，明文那侧的拦截会跟着一起失真。
    #[test]
    fn https_local_endpoint_passes_the_gate() {
        let mut doc = product_stub();
        patch_doc(&mut doc, LOCAL);
        assert!(
            first_bad_pattern(&doc).is_none(),
            "https 本地端点必须是合法 pattern"
        );
        assert!(doc_points_at(&doc, LOCAL));
    }

    /// [`base_url`] 是所有写入值的唯一来源，它必须自洽地过闸 ——
    /// 这条规则一破，代价是 TraeWork 启动即崩，所以钉死在测试里。
    #[test]
    fn base_url_is_plain_and_needs_the_patch() {
        assert_eq!(base_url(8788), "http://127.0.0.1:8788");
        assert!(
            base_url(1).starts_with("http://127.0.0.1:"),
            "一定要明文 + 回环 IP"
        );
        let mut doc = product_stub();
        patch_doc(&mut doc, &base_url(8788));
        assert!(
            first_bad_pattern_mode(&doc, true).is_none(),
            "打过补丁后明文端点必须过闸"
        );
        assert!(
            first_bad_pattern_mode(&doc, false).is_some(),
            "未打补丁时明文端点必须被拦下（它是启动即崩的成因）"
        );
    }

    #[test]
    fn gate_scans_every_string_under_remote_and_agent() {
        // TraeWork 用 Hp() 递归收集 remote/agent 下的**全部**非空字符串，
        // 所以旁支字段里的脏值同样会打挂它，闸门必须一起拦。
        let mut doc = product_stub();
        doc["bootConfig"]["remote"]["bytedance"]["normal"] = json!("ftp://x.example");
        let (raw, _) = first_bad_pattern(&doc).expect("旁支字段也要拦");
        assert_eq!(raw, "ftp://x.example");
    }

    #[test]
    fn preflight_is_read_only_on_this_machine() {
        let Some(app) = app_dir() else {
            return; // 没装 TraeWork 的机器直接跳过
        };
        let path = app.join(PRODUCT_FILE);
        let Ok(before) = std::fs::read(&path) else {
            return;
        };
        let backup_before = app.join(BACKUP_FILE).exists();

        // 预检必须是**只读**的：允许通过，但绝不允许碰 product.json，
        // 也不允许留下/删掉备份文件。
        let https = preflight(LOCAL);
        let plain = preflight(&base_url(8788));

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "预检必须是只读的，绝不允许碰 product.json"
        );
        assert_eq!(
            app.join(BACKUP_FILE).exists(),
            backup_before,
            "预检不该留下/删掉备份文件"
        );
        assert!(
            https.is_ok(),
            "https:// 端点应当始终能过预检（它不受补丁状态约束）：{:?}",
            https.err()
        );
        // 明文端点（我们现在唯一会写的那个）能不能过，取决于**当前**补丁状态 ——
        // 这正是那条双向不变量
        if crate::patch::is_patched() {
            assert!(
                plain.is_ok(),
                "已打免证书补丁的机器上，明文端点必须能过预检：{:?}",
                plain.err()
            );
        } else {
            let err = plain.expect_err("未打补丁时必须拦下明文端点");
            assert!(err.contains("免证书补丁"), "错误信息要自带出路：{err}");
        }
    }
}
