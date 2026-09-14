//! 智能接管的本地反代：把 TraeWork 原生 API 请求透传到官方上游，仅替换鉴权凭据。
//!
//! 开启「智能接管」时，`endpoint.rs` 会写入 TraeWork 的
//! `resources/app/product.desktop.local.json`，把 `remote.domain` 指向
//! `http://127.0.0.1:{port}`。于是 TraeWork 的模型列表 / 会话创建 / 消息发送 / SSE 流式回包
//! 等整条 HTTP 链路都落到本机：
//!
//!     `{remote.domain}/api/remote/v1/*`  →  官方上游同路径（原样透传）
//!
//! 本模块只做两件事：
//! 1. 把 `Authorization` 换成账号池里某个账号的 `Cloud-IDE-JWT {token}`；
//! 2. 路径 / 查询串 / 请求体 / 响应体（含 SSE 流）原样透传。
//!
//! 路由逻辑：
//! 1. **会话粘滞**：`/chat_sessions/:id/*` 固定复用同一账号（会话属于账号，换号会丢上下文）；
//! 2. **积分优先轮换**：新会话按「**到期最早优先 → 无到期数据靠后 → 积分多者优先**」挑账号，
//!    先把快到期的额度用掉（见 [`pick_index`]；TraeWork 当前不返回到期时间，故实际等价于
//!    「积分多者优先」）。积分快照 10 分钟 TTL 并落盘，避免每次新会话都打接口；
//! 3. **限流无感切换**：某账号返回 429 ⇒ 打入 10 分钟冷却、解绑会话，换下一个账号重发
//!    （上限 2 次）；全部失败则原样透传最后一个 429。冷却中的账号路由优先跳过；
//! 4. **白名单**：设置里勾选的账号才有资格被扣费；空 = 全部。
//!
//! 以上每一步都会写一条「接管动态」（[`crate::journal`]），供界面回看「刚才发生了什么」。
//!
//! 响应一律 chunked 流式下发（对话是 SSE，缓冲成一次 body 会报 Empty stream）。
//!
//! ⚠️ 本进程**只**在 `takeover_enabled` 为真时监听。一旦停止监听，TraeWork 发往本端口的请求
//! 将全部失败——所以端点覆盖的写入前必须先确认反代已在监听（见 `commands::takeover_enable`），
//! 并由 `endpoint::sweep` 在启动时兜底恢复。

use crate::accounts;
use crate::commands;
use crate::endpoint;
use crate::journal;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// accept 空轮询间隔。它直接等于「请求到达 → 被 accept」的额外延迟，
/// 所以给得比配置轮询小一个量级。
const ACCEPT_POLL: Duration = Duration::from_millis(30);
/// 配置（接管开关 / 端口）轮询间隔。比 accept 轮询慢得多：每轮 accept 都读一次
/// settings.json 纯属磁盘浪费，而开关变更晚 0.5s 生效完全无感。
const CONFIG_POLL: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const STICKY_TTL: Duration = Duration::from_secs(30 * 60);
const COOLDOWN_TTL: Duration = Duration::from_secs(10 * 60);
/// 积分快照缓存时长：选号不必每次都打资源接口。
const SNAPSHOT_TTL: Duration = Duration::from_secs(600);
const FAILOVER_MAX_TRIES: usize = 3;
/// 端点覆盖租约续约间隔。
const LEASE_HEARTBEAT: Duration = Duration::from_secs(30);
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;
/// 客户端请求头读取时限（读空闲）：连上了却迟迟不发完整请求就放弃。
/// 注意它只在**阻塞** socket 上生效（`SO_RCVTIMEO` 对非阻塞 socket 无效）——见 `handle_conn`。
const HEAD_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 下游写超时：SSE 长对话可能持续数分钟，写超时要给得足够宽。
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(600);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("构建 HTTP 客户端失败")
});

type Sticky = Mutex<HashMap<String, (Instant, String)>>;
fn sticky_conv() -> &'static Sticky {
    static S: OnceLock<Sticky> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

type Cooldown = Mutex<HashMap<String, Instant>>;
fn cooldown_table() -> &'static Cooldown {
    static C: OnceLock<Cooldown> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// 运行状态（供前端反馈：开启后能立刻看到是否真的在监听）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProxyStatus {
    pub active: bool,
    pub port: u16,
    pub error: Option<String>,
}

static PROXY_STATUS: OnceLock<Mutex<ProxyStatus>> = OnceLock::new();
fn proxy_status_cell() -> &'static Mutex<ProxyStatus> {
    PROXY_STATUS.get_or_init(Default::default)
}

fn set_status(v: ProxyStatus) {
    if let Ok(mut g) = proxy_status_cell().lock() {
        *g = v;
    }
}

/// 供 Tauri 命令读取反代当前运行状态。
pub fn status() -> ProxyStatus {
    proxy_status_cell()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// 账号是否在冷却期（限流后跳过）
fn cooling(id: &str) -> bool {
    cooldown_table()
        .lock()
        .ok()
        .and_then(|m| m.get(id).copied())
        .map(|t| t.elapsed() < COOLDOWN_TTL)
        .unwrap_or(false)
}

/// 会话粘滞命中：返回账号 id，刷新时间戳；顺带清理过期项。
fn sticky_hit(conv: &str) -> Option<String> {
    let mut map = sticky_conv().lock().ok()?;
    map.retain(|_, (at, _)| at.elapsed() < STICKY_TTL);
    let (at, id) = map.get_mut(conv)?;
    *at = Instant::now();
    Some(id.clone())
}

/// 写入/刷新会话粘滞。返回 `true` 表示该会话**换到了新账号**（首次上代理或被切换），
/// 调用方据此写「开始使用账号」事件；同一会话的后续请求返回 `false`，避免刷屏。
fn sticky_put(conv: &str, id: String) -> bool {
    if let Ok(mut map) = sticky_conv().lock() {
        let changed = map
            .get(conv)
            .map(|(_, cur)| cur != &id)
            .unwrap_or(true);
        map.insert(conv.to_string(), (Instant::now(), id));
        return changed;
    }
    false
}

/// 解绑会话粘滞（限流换号时调用）。
fn sticky_remove(conv: &str) {
    if let Ok(mut map) = sticky_conv().lock() {
        map.remove(conv);
    }
}

// ---------------------------------------------------------------------------
// HTTP 解析（纯函数，便于单测）
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body_len: usize,
    head_end: usize,
}

fn find_slice(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn parse_request(buf: &[u8]) -> Option<Request> {
    let end = find_slice(buf, b"\r\n\r\n")?;
    if end + 4 > MAX_HEAD + 4 {
        return None;
    }
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    if parts.next().is_none() {
        return None;
    }
    let mut headers = Vec::new();
    let mut body_len = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                body_len = value.parse().unwrap_or(0);
            }
            headers.push((name.trim().to_string(), value));
        }
    }
    Some(Request { method, target, headers, body_len, head_end: end + 4 })
}

fn header_value<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 去掉绝对 URI 里的 scheme + host，只留 path+query。
fn normalize_target(target: &str) -> &str {
    match target.find("://") {
        Some(i) => {
            let rest = &target[i + 3..];
            match rest.find('/') {
                Some(p) => &rest[p..],
                None => "/",
            }
        }
        None => target,
    }
}

fn hop_by_hop(name: &str) -> bool {
    [
        "host", "authorization", "content-length", "transfer-encoding", "connection",
        "keep-alive", "accept-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

fn req_body<'a>(buf: &'a [u8], req: &Request) -> &'a [u8] {
    buf.get(req.head_end..req.head_end + req.body_len).unwrap_or(&[])
}

// ---------------------------------------------------------------------------
// 账号选择
// ---------------------------------------------------------------------------

/// 白名单过滤（空 = 全部）
fn billing_candidates(all: &[accounts::Account], selected: &[String]) -> Vec<accounts::Account> {
    if selected.is_empty() {
        return all.to_vec();
    }
    let picked: Vec<_> = all
        .iter()
        .filter(|a| selected.iter().any(|s| s == &a.id))
        .cloned()
        .collect();
    if picked.is_empty() { all.to_vec() } else { picked }
}

/// 一个账号的积分画像（选号时的排序依据）。
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct CreditInfo {
    /// 还有余量的额度里最早的到期时间（毫秒）；未知为 `None`
    expiry_ms: Option<i64>,
    /// 剩余积分；未知为 `None`
    credits: Option<i64>,
    /// 不限量（entitlement 里存在 `credits_limit = -1` 的包）
    unlimited: bool,
}

/// 路由排序键：**到期最早者优先** → 查不到到期时间的靠后 → 剩余积分多者优先。
fn score(info: CreditInfo) -> (i64, i64) {
    (info.expiry_ms.unwrap_or(i64::MAX), -(info.credits.unwrap_or(0)))
}

/// 从候选里选出该用的账号下标。`infos` 与 `ids` 一一对应。
///
/// 规则：剩余积分为 0 的账号直接跳过（除非全员为 0 / 全未知）；
/// 全员为 0 时退化为取第一个——让上游自己报错，比代理直接 503 更有信息量。
fn pick_index(ids: &[String], infos: &[CreditInfo]) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score: Option<(i64, i64)> = None;
    for (i, info) in infos.iter().enumerate() {
        if info.credits == Some(0) {
            continue;
        }
        let s = score(*info);
        if best_score.map_or(true, |cur| s < cur) {
            best_score = Some(s);
            best = Some(i);
        }
    }
    best.or(if ids.is_empty() { None } else { Some(0) })
}

/// 候选软过滤（纯函数，便于单测）：优先剔除冷却中的账号；
/// 若剔完为空（全员都在冷却）则原样返回——让上游裁决也比代理直接 503 有信息量。
fn available_candidates<T: Clone>(candidates: &[T], is_cooling: impl Fn(&T) -> bool) -> Vec<T> {
    let usable: Vec<T> = candidates
        .iter()
        .filter(|a| !is_cooling(a))
        .cloned()
        .collect();
    if usable.is_empty() {
        candidates.to_vec()
    } else {
        usable
    }
}

/// 持久化的积分快照是否已过期（决定新会话要不要重新打接口）。
fn snapshot_stale(snap: Option<&accounts::CreditSnapshot>) -> bool {
    let Some(snap) = snap else { return true };
    match chrono::NaiveDateTime::parse_from_str(&snap.fetched_at, "%Y-%m-%d %H:%M:%S") {
        Ok(t) => (chrono::Local::now().naive_local() - t).num_seconds() >= SNAPSHOT_TTL.as_secs() as i64,
        Err(_) => true,
    }
}

/// 积分展示文案（供接管动态）。
fn credits_text(info: CreditInfo) -> String {
    if info.unlimited {
        return "积分不限量".into();
    }
    match info.credits {
        Some(c) => format!("剩 {c} 积分"),
        None => "积分未知".into(),
    }
}

/// 会话短标识（接管动态里只展示前 8 位，避免刷屏）。
fn short_conv(conv: &str) -> String {
    conv.chars().take(8).collect()
}

/// 选出一个该用的账号。
///
/// 候选集 = 设置里勾选的扣费账号（未勾选的不允许扣费；全不勾 = 全部可用），再做两层过滤：
/// - **禁用（严格）**：`ban` 里的账号是本轮请求已试败的限流账号，直接剔除；剔完为空返回 None；
/// - **冷却（软）**：近 10 分钟触发过限流的账号优先跳过，全员冷却则照常用。
///
/// 候选集内的优先级从高到低：
/// 1. **会话粘滞**——一次对话中途换账号会丢上下文；粘滞账号若已被移出候选集 / 在冷却则视为未命中；
/// 2. **积分优先轮换**——快照过期就重拉，按 [`pick_index`] 挑（到期最早优先，无到期数据则积分多者优先）。
///
/// 会话首次落到某个账号（或被切换到新账号）时写一条 `route_start` 接管动态。
async fn choose_account(
    dir: &std::path::Path,
    conv: Option<&str>,
    ban: &[String],
) -> Option<accounts::Account> {
    let settings = accounts::load_settings(dir);
    let mut all = accounts::load_accounts(dir);
    if all.is_empty() {
        return None;
    }
    let candidates = billing_candidates(&all, &settings.billing_account_ids);
    // 限流重试时已试败的账号严格剔除：再试一次只会再吃一个 429
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|a| !ban.iter().any(|b| b == &a.id))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let usable = available_candidates(&candidates, |a| cooling(&a.id));

    // 1) 已在进行的会话：继续用同一个账号（除非它已被移出可用集）
    if let Some(conv) = conv {
        if let Some(id) = sticky_hit(conv) {
            if let Some(a) = usable.iter().find(|a| a.id == id) {
                return Some(a.clone());
            }
        }
    }

    // 2) 新会话：快照缺失/过期就从接口重拉并落盘，否则直接用持久化的积分快照
    let ids: Vec<String> = usable.iter().map(|a| a.id.clone()).collect();
    let mut infos: Vec<CreditInfo> = Vec::with_capacity(usable.len());
    let mut need_persist = false;
    for acct in &usable {
        let mut info = acct
            .credit_snapshot
            .as_ref()
            .map(|s| CreditInfo {
                expiry_ms: s.earliest_expiry_ms,
                credits: s.credits,
                unlimited: s.unlimited,
            })
            .unwrap_or_default();
        if snapshot_stale(acct.credit_snapshot.as_ref()) {
            let mut snap = crate::checkin::fetch_credit_snapshot(&CLIENT, acct).await;
            // 拉不到就保留上次已知的数字（只把 fetched_at 推新，避免每轮狂打接口），
            // 否则一次限流就会把已积累的额度信息抹成「未知」，选号随之失去依据
            if snap.credits.is_none() && !snap.unlimited {
                if let Some(prev) = acct.credit_snapshot.as_ref() {
                    snap.credits = prev.credits;
                    snap.unlimited = prev.unlimited;
                    snap.earliest_expiry_ms = prev.earliest_expiry_ms;
                }
            }
            info = CreditInfo {
                expiry_ms: snap.earliest_expiry_ms,
                credits: snap.credits,
                unlimited: snap.unlimited,
            };
            // 回写持久化快照（含 fetched_at），下次新会话直接读、不必再打接口
            if let Some(a) = all.iter_mut().find(|a| a.id == acct.id) {
                a.credit_snapshot = Some(snap);
            }
            need_persist = true;
        }
        infos.push(info);
    }
    if need_persist {
        let _ = accounts::save_accounts(dir, &all);
    }

    let idx = pick_index(&ids, &infos)?;
    let account = usable[idx].clone();
    if let Some(conv) = conv {
        if sticky_put(conv, account.id.clone()) {
            // 该会话第一次走上代理，或被切到了新账号 —— 记一条「开始使用」动态
            journal::append(
                dir,
                "route_start",
                &format!(
                    "会话 {} 开始使用账号「{}」（{}，备选 {} 个）",
                    short_conv(conv),
                    account.name,
                    credits_text(infos[idx]),
                    usable.len()
                ),
            );
        }
    }
    Some(account)
}

// ---------------------------------------------------------------------------
// 连接处理
// ---------------------------------------------------------------------------

fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        let dir = match commands::try_data_dir(&app) {
            Ok(d) => d,
            Err(_) => {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let settings = accounts::load_settings(&dir);
        if !settings.takeover_enabled {
            set_status(ProxyStatus::default());
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let port = settings.takeover_port;
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                set_status(ProxyStatus {
                    active: true,
                    port,
                    error: None,
                });
                eprintln!("[接管] 反代监听 127.0.0.1:{port}");
                let _ = listener.set_nonblocking(true);
                let mut last_lease = Instant::now();
                let mut last_cfg = Instant::now();
                loop {
                    // 配置轮询与 accept 解耦：每轮 accept 都读一次 settings.json 纯属磁盘浪费，
                    // 而开关变更晚 0.5s 生效完全无感。
                    if last_cfg.elapsed() >= CONFIG_POLL {
                        let cur = accounts::load_settings(&dir);
                        if !cur.takeover_enabled || cur.takeover_port != port {
                            break;
                        }
                        last_cfg = Instant::now();
                    }
                    // 端点覆盖租约心跳：接管开启且反代在跑时持续续约，
                    // 让「助手异常退出」在下次启动时能被识别。
                    if last_lease.elapsed() >= LEASE_HEARTBEAT {
                        endpoint::touch_lease(&dir);
                        last_lease = Instant::now();
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let app2 = app.clone();
                            std::thread::spawn(move || handle_conn(stream, app2));
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL);
                        }
                        Err(_) => std::thread::sleep(ACCEPT_POLL),
                    }
                }
                set_status(ProxyStatus::default());
            }
            Err(e) => {
                set_status(ProxyStatus {
                    active: false,
                    port,
                    error: Some(format!("端口 {port} 无法监听：{e}")),
                });
                eprintln!("[接管] 无法监听 127.0.0.1:{port}：{e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

fn handle_conn(mut stream: TcpStream, app: tauri::AppHandle) {
    // 数据目录先取：选账号、写接管动态都要用；取不到直接 500，不再读请求
    let dir = match commands::try_data_dir(&app) {
        Ok(d) => d,
        Err(_) => {
            respond(&mut stream, 500, "text/plain", b"internal error", &[]);
            return;
        }
    };

    // ⚠️ 必须先复位成**阻塞**模式再读。
    //
    // 监听 socket 为了能在 accept 之余轮询配置开关，必须是**非阻塞**的；而 Windows 上
    // `accept()` 返回的 socket 会**继承**监听 socket 的非阻塞状态（Linux 不继承，所以这个
    // 坑在 Linux 上永远测不出来）。于是每个连接天生非阻塞：只要第一次 `read()` 时请求字节
    // 还没到齐，就立刻返回 `WouldBlock`，被读循环当成「请求非法」→ 400。
    // 触发完全取决于客户端「先连后发」的时序——正是这类 bug 最难复现的形态。
    if stream.set_nonblocking(false).is_err() {
        journal::append(
            &dir,
            "proxy_error",
            "无法把连接复位成阻塞模式，已放弃该请求（可能是系统 socket 异常）",
        );
        return;
    }
    let _ = stream.set_read_timeout(Some(HEAD_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));

    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8192];
    let req = loop {
        match stream.read(&mut tmp) {
            Ok(0) => break None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(req) = parse_request(&buf) {
                    if buf.len() >= req.head_end + req.body_len {
                        break Some(req);
                    }
                }
                if buf.len() > MAX_HEAD + MAX_BODY {
                    break None;
                }
            }
            Err(_) => break None,
        }
    };
    let Some(req) = req else {
        // 读到一半断开（最常见是 0 字节：客户端连上但还没发请求就断开）≠「请求非法」，
        // 所以把字节数写进动态，让这种事一眼可辨。
        journal::append(
            &dir,
            "proxy_bad_request",
            &format!(
                "请求未读完或不合法，回 400（已收 {} 字节）：{}",
                buf.len(),
                head_prefix(&buf)
            ),
        );
        respond(&mut stream, 400, "text/plain", b"bad request", &[]);
        return;
    };

    // 接管语义：一律透明反代到官方上游，仅替换鉴权凭据。
    handle_transparent(&mut stream, &buf, &req, &dir);
}

/// 诊断用：把已收到的字节截成可读前缀（最多 512 字节）。
fn head_prefix(buf: &[u8]) -> String {
    String::from_utf8_lossy(&buf[..buf.len().min(512)]).to_string()
}

// ---------------------------------------------------------------------------
// 响应写出
// ---------------------------------------------------------------------------

fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    stream.write_all(format!("{:X}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn write_end(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// 透明反代核心
// ---------------------------------------------------------------------------

/// 从路径里抠出 `chat_session_id`：会话是账号维度的，同一会话必须始终落在同一账号上。
fn session_id_from_path(target: &str) -> Option<String> {
    let bare = normalize_target(target);
    let path = bare.split('?').next().unwrap_or("");
    let idx = path.find("/chat_sessions/")?;
    let rest = &path[idx + "/chat_sessions/".len()..];
    let seg = rest.split('/').next().unwrap_or("");
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

/// 原始上游 HTTP 主机（取自 TraeWork `product.json`，不硬编码；进程内缓存一次）。
fn upstream_host() -> Option<String> {
    static UP: OnceLock<Option<String>> = OnceLock::new();
    UP.get_or_init(|| endpoint::read_upstreams().0).clone()
}

/// 构造跨域响应头；请求带 `Origin` 时回显（配合 credentials），否则用 `*`。
fn cors_headers(req: &Request) -> Vec<(String, String)> {
    let origin = header_value(req, "origin").unwrap_or("*").to_string();
    let allow_headers = header_value(req, "access-control-request-headers")
        .map(str::to_string)
        .unwrap_or_else(|| "*".to_string());
    vec![
        ("access-control-allow-origin".into(), origin),
        ("access-control-allow-credentials".into(), "true".into()),
        (
            "access-control-allow-methods".into(),
            "GET,POST,PUT,PATCH,DELETE,OPTIONS".into(),
        ),
        ("access-control-allow-headers".into(), allow_headers),
        ("access-control-expose-headers".into(), "*".into()),
        ("access-control-max-age".into(), "600".into()),
        ("vary".into(), "Origin".into()),
    ]
}

/// 只写响应头的空响应（用于 `OPTIONS` 预检）。
fn respond_head_only(stream: &mut TcpStream, status: u16, extra: &[(String, String)]) {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: 0\r\nConnection: close\r\n",
        reason(status)
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
}

/// 响应头里不应原样转发的项（长度/编码由本层重算）。
fn response_hop_headers(name: &str) -> bool {
    [
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "content-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// 透传响应：保留上游状态码/响应头（剔除长度与编码），叠加 CORS，边收边转。
fn stream_response_passthrough(
    stream: &mut TcpStream,
    mut resp: reqwest::Response,
    cors: &[(String, String)],
    dir: &std::path::Path,
    path: &str,
) {
    let status = resp.status().as_u16();
    // 诊断用：上游返回 4xx/5xx 时落动态，便于区分「代理自己回的 400」与「上游 400 透传」
    if status >= 400 {
        journal::append(
            dir,
            "proxy_upstream_status",
            &format!("上游对 {path} 返回 {status}"),
        );
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in resp.headers().iter() {
        let name = k.as_str();
        if response_hop_headers(name) {
            continue;
        }
        if let Ok(val) = v.to_str() {
            headers.push((name.to_string(), val.to_string()));
        }
    }
    headers.extend_from_slice(cors);

    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        reason(status)
    );
    for (k, v) in &headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    let mut bytes = 0usize;
    let mut read_error: Option<String> = None;
    tauri::async_runtime::block_on(async {
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    if write_chunk(stream, &c).is_err() {
                        break; // 下游断了，不是上游的错
                    }
                    bytes += c.len();
                }
                Ok(None) => break,
                // 上游读失败绝不能静默：吞掉的话客户端只会看到一个「干净」的空流，
                // 报 Empty stream 却查不到原因。这里留痕到接管动态。
                Err(e) => {
                    read_error = Some(format!("上游读流失败（已转发 {bytes} 字节）：{e}"));
                    break;
                }
            }
        }
    });
    if let Some(msg) = read_error {
        journal::append(dir, "proxy_stream_error", &format!("[{path}] {msg}"));
    }
    let _ = write_end(stream);
}

/// 接管核心：把 TraeWork 原生请求透传到原始上游，只替换鉴权凭据。
///
/// - 路径 / 查询 / 请求体原样透传；
/// - `Authorization` 换成池化账号的 `Cloud-IDE-JWT`；
/// - 按 `chat_session_id` 会话粘滞（同一会话固定同一账号，避免账号错配）；
/// - 429 时并入冷却、解绑粘滞、换账号重试（上限 `FAILOVER_MAX_TRIES`）；
/// - 响应（含 SSE）边收边转，并叠加 CORS 头。
fn handle_transparent(stream: &mut TcpStream, buf: &[u8], req: &Request, dir: &std::path::Path) {
    let cors = cors_headers(req);

    if req.method == "OPTIONS" {
        respond_head_only(stream, 204, &cors);
        return;
    }

    let Some(up) = upstream_host() else {
        journal::append_dedup(
            dir,
            "proxy_error",
            "无法从 TraeWork 的 product.json 解析出原始上游域名，接管无法回源",
        );
        respond(
            stream,
            502,
            "text/plain",
            b"cannot resolve upstream from product.json",
            &[],
        );
        return;
    };

    let conv = session_id_from_path(&req.target)
        .or_else(|| header_value(req, "x-conversation-id").map(str::to_string))
        .filter(|s| !s.is_empty());
    let body_bytes = req_body(buf, req).to_vec();
    let bare = normalize_target(&req.target).to_string();
    let url = format!("{}{}", up.trim_end_matches('/'), bare);

    let mut ban: Vec<String> = Vec::new();
    loop {
        let account = match tauri::async_runtime::block_on(choose_account(dir, conv.as_deref(), &ban)) {
            Some(a) => a,
            None => {
                journal::append_dedup(
                    dir,
                    "proxy_error",
                    "已开启智能接管，但账号池里没有被扣费资格的账号——请到「账号与签到」添账号，或在设置里调整扣费账号白名单",
                );
                respond(
                    stream,
                    503,
                    "text/plain",
                    b"no account available for takeover",
                    &[],
                );
                return;
            }
        };

        use reqwest::Method;
        let upstream = tauri::async_runtime::block_on(async {
            let mut r = CLIENT
                .request(
                    Method::from_bytes(req.method.as_bytes()).unwrap_or(Method::GET),
                    &url,
                )
                .header("Authorization", format!("Cloud-IDE-JWT {}", account.token));
            for (k, v) in &req.headers {
                if !hop_by_hop(k) {
                    r = r.header(k.as_str(), v.as_str());
                }
            }
            if !body_bytes.is_empty() {
                r = r.body(body_bytes.clone());
            }
            r.send().await
        });

        match upstream {
            // 429 发生在流式输出开始前、响应头还没写给下游，正好有重试窗口：
            // 把限流账号打入冷却、解绑会话粘滞，换下一个账号重发同一请求，对 TraeWork 完全无感。
            Ok(resp) if resp.status() == 429 => {
                if let Ok(mut m) = cooldown_table().lock() {
                    m.insert(account.id.clone(), Instant::now());
                }
                if let Some(c) = &conv {
                    sticky_remove(c);
                }
                if ban.len() + 1 < FAILOVER_MAX_TRIES {
                    ban.push(account.id.clone());
                    journal::append(
                        dir,
                        "failover",
                        &format!(
                            "账号「{}」触发限流（429，{bare}），已无感切换备用账号继续服务",
                            account.name
                        ),
                    );
                    continue;
                }
                journal::append(
                    dir,
                    "failover",
                    &format!(
                        "账号「{}」触发限流（429，{bare}），已无更多备用账号，限流响应原样透传",
                        account.name
                    ),
                );
                stream_response_passthrough(stream, resp, &cors, dir, &bare);
                return;
            }
            Ok(resp) => {
                stream_response_passthrough(stream, resp, &cors, dir, &bare);
                return;
            }
            Err(e) => {
                let msg = format!("upstream error: {e}");
                journal::append(dir, "proxy_error", &format!("[{bare}] 上游请求失败：{msg}"));
                respond(stream, 502, "text/plain", msg.as_bytes(), &[]);
                return;
            }
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8], extra: &[(&str, &str)]) {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reason(status),
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// 由 `lib.rs` 驱动的对外入口：`proxy::spawn_proxy(app.handle().clone())`
pub fn spawn_proxy(app: tauri::AppHandle) {
    spawn(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head() {
        let raw = b"POST /api/remote/v1/chat_sessions/c1/messages HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nX-Conversation-Id: c1\r\n\r\n{}";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/api/remote/v1/chat_sessions/c1/messages");
        assert_eq!(req.body_len, 2);
        assert_eq!(header_value(&req, "x-conversation-id"), Some("c1"));
    }

    #[test]
    fn rejects_incomplete() {
        assert_eq!(parse_request(b"GET /x HTTP/1.1\r\n"), None);
        assert_eq!(parse_request(b"garbage"), None);
    }

    #[test]
    fn hop_by_hop_filters_token() {
        assert!(hop_by_hop("Authorization"));
        assert!(!hop_by_hop("Content-Type"));
    }

    #[test]
    fn normalize_strips_absolute_uri() {
        assert_eq!(
            normalize_target("http://127.0.0.1:8788/api/remote/v1/models"),
            "/api/remote/v1/models"
        );
        assert_eq!(normalize_target("/api/remote/v1/models"), "/api/remote/v1/models");
    }

    fn acct(id: &str) -> accounts::Account {
        accounts::Account {
            id: id.into(),
            name: id.into(),
            phone: None,
            region: None,
            user_id: None,
            token: "t".into(),
            refresh_token: None,
            host: None,
            expires_at: None,
            refresh_expires_at: None,
            device_id: None,
            machine_id: None,
            created_at: String::new(),
            credit_snapshot: None,
        }
    }

    #[test]
    fn billing_candidates_restricts() {
        let all = vec![acct("a"), acct("b")];
        assert_eq!(billing_candidates(&all, &[]).len(), 2);
        assert_eq!(billing_candidates(&all, &["b".into()]).len(), 1);
        assert_eq!(billing_candidates(&all, &["zz".into()]).len(), 2, "勾选失效退回全部");
    }

    #[test]
    fn available_candidates_skips_cooling_unless_all_cooling() {
        let all = vec![acct("a"), acct("b"), acct("c")];
        let ids = |v: &[accounts::Account]| -> Vec<String> { v.iter().map(|a| a.id.clone()).collect() };
        // 无人冷却 → 原样返回
        assert_eq!(ids(&available_candidates(&all, |a| a.id == "x")).len(), 3);
        // b 在冷却 → 跳过 b
        assert_eq!(
            ids(&available_candidates(&all, |a| a.id == "b")),
            vec!["a".to_string(), "c".to_string()]
        );
        // 全员冷却 → 软过滤退回全部：让上游裁决也比代理直接 503 有信息量
        assert_eq!(ids(&available_candidates(&all, |_| true)).len(), 3);
    }

    /// 造一个积分画像：`(到期毫秒, 剩余积分)`。
    fn info(expiry_ms: Option<i64>, credits: Option<i64>) -> CreditInfo {
        CreditInfo { expiry_ms, credits, unlimited: false }
    }

    #[test]
    fn routing_prefers_earliest_expiry_then_most_credits() {
        let ids: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let infos = vec![
            info(Some(2000), Some(100)),
            info(Some(1000), Some(10)), // 最早过期 → 胜出
            info(None, Some(99999)),    // 未知到期 → 靠后
            info(Some(500), Some(0)),   // 积分为 0 → 跳过
        ];
        assert_eq!(pick_index(&ids, &infos), Some(1));

        // 到期时间相同 → 剩余积分多者优先
        let ids2: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let infos2 = vec![info(Some(1000), Some(10)), info(Some(1000), Some(500))];
        assert_eq!(pick_index(&ids2, &infos2), Some(1));
    }

    /// 到期信息缺失（接口失败 / 未登录）时排序退化为「积分多者优先」。
    #[test]
    fn routing_falls_back_to_most_credits_when_expiry_unknown() {
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let infos = vec![
            info(None, Some(150)),
            info(None, Some(900)), // 积分最多 → 胜出
            info(None, Some(400)),
        ];
        assert_eq!(pick_index(&ids, &infos), Some(1));

        // 未知积分（可能还有余量）应优先于已知为 0 的账号
        let ids2: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let infos2 = vec![info(Some(100), Some(0)), CreditInfo::default()];
        assert_eq!(pick_index(&ids2, &infos2), Some(1));

        // 不限量账号：没有到期时间、也没有数字 → 与「未知」同档，不会插队
        let unlimited = CreditInfo { unlimited: true, ..Default::default() };
        let infos_u = vec![info(Some(100), Some(5)), unlimited];
        assert_eq!(pick_index(&ids2, &infos_u), Some(0));

        // 全员已知为 0 → 谁都不入选，退化为第一个（让上游报错，比代理 503 更有信息量）
        let infos3 = vec![info(Some(100), Some(0)), info(Some(200), Some(0))];
        assert_eq!(pick_index(&ids2, &infos3), Some(0));
        assert_eq!(pick_index(&[], &[]), None, "没有账号就没有下标");
    }

    #[test]
    fn snapshot_staleness_drives_refetch() {
        assert!(snapshot_stale(None), "没有快照必须重取");
        let fresh = accounts::CreditSnapshot::now(Some(150), false, None);
        assert!(!snapshot_stale(Some(&fresh)));
        let old = accounts::CreditSnapshot {
            credits: Some(1),
            unlimited: false,
            earliest_expiry_ms: None,
            fetched_at: "2000-01-01 00:00:00".into(),
        };
        assert!(snapshot_stale(Some(&old)), "过期快照必须重取");
        let malformed = accounts::CreditSnapshot {
            credits: None,
            unlimited: false,
            earliest_expiry_ms: None,
            fetched_at: "not a time".into(),
        };
        assert!(snapshot_stale(Some(&malformed)), "时间戳无法解析时按过期处理");
    }

    #[test]
    fn extracts_session_id_from_path() {
        assert_eq!(
            session_id_from_path("/api/remote/v1/chat_sessions/abc123/messages").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            session_id_from_path("/api/remote/v1/chat_sessions/abc123/events?x=1").as_deref(),
            Some("abc123")
        );
        assert_eq!(session_id_from_path("/api/remote/v1/models"), None);
    }
}
