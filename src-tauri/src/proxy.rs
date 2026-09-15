//! 智能接管的本地反代：把 TraeWork 原生 API 请求透传到官方上游，仅替换鉴权凭据。
//!
//! 开启「智能接管」时，`endpoint.rs` 会把 TraeWork 的 `product.json` 里
//! `bootConfig.{remote,agent,ckg,cue,hub,ws}` 的 `trae.normal` 一并指向本机端点
//! `http://127.0.0.1:{port}`（**明文回环**）。于是 TraeWork 的模型列表 / 会话创建 /
//! 消息发送 / SSE 流式回包 / 实时通道等整条链路都落到本机：
//!
//!     `{端点}/api/remote/v1/*`  →  官方上游同路径（原样透传）
//!
//! ⚠️ 端点走明文不是偷懒，而是**先用补丁松开 TraeWork 的 scheme 闸门**换来的：
//! 代价是给 TraeWork 的 `out/main.js` 打一个可逐字节还原的补丁（[`crate::patch`]），
//! 收益是本机一个证书都不用装 —— 不必动系统信任库。取证见 `endpoint.rs` 模块文档。
//!
//! 因此本进程**只讲明文 HTTP**：下游不需要 TLS，也就不存在「证书准备不出来就不监听」
//! 那个 fail-closed 分支 —— 这里唯一的启动前提是端口绑得上。
//! ⚠️ 上游那一侧**仍然要讲 TLS**（TraeWork 的官方接口是 https），见 [`Upstream`]。
//!
//! 本模块只做两件事：
//! 1. 把 `Authorization` 换成账号池里某个账号的 `Cloud-IDE-JWT {token}`；
//! 2. 路径 / 查询串 / 请求体 / 响应体（含 SSE 流）原样透传。
//!
//! 路由逻辑：
//! 1. **会话粘滞**：`/chat_sessions/:id/*` 固定复用同一账号（会话属于账号，换号会丢上下文）；
//! 2. **额度到期优先轮换**：新会话按「**到期最早优先 → 无到期数据靠后 → 积分多者优先**」
//!    挑账号，先把快到期的额度用掉（见 [`pick_index`]）。排序依据来自
//!    `ide_user_ent_usage` 的额度包到期时间 `expire_time`（秒级）—— **该字段实测确实返回**
//!    （2026-09-14 用真实账号 dump 过；旧注释曾写「TraeWork 不返回到期时间」，是错的），
//!    只统计「还有余量」的包（已用光的包到期再早也不该把排序带偏）。
//!    快照 10 分钟 TTL 并落盘，避免每次新会话都打接口；
//! 3. **限流无感切换**：某账号返回 429 ⇒ 打入 10 分钟冷却、解绑会话，换下一个账号重发
//!    （上限 2 次）；全部失败则原样透传最后一个 429。冷却中的账号路由优先跳过；
//! 4. **白名单**：设置里勾选的账号才有资格被扣费（未勾选的一律不参与，见
//!    [`billing_candidates`]）；空列表 = 全部（未配置过的默认状态）；
//! 5. **只给扣费路径换凭据**：只有 `/api/remote/v1/{chat_sessions,models,file_converts}`
//!    会换成池化账号的 token（见 [`needs_pooled_token`]），其余（技能市场 / git / 设置…）
//!    **原样沿用 TraeWork 自己的凭据** —— 换了对用户意味着「看到别人账号的数据」，
//!    而扣费一分钱也省不下。
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
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
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
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body_len: usize,
    pub(crate) head_end: usize,
}

pub(crate) fn find_slice(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

pub(crate) fn parse_request(buf: &[u8]) -> Option<Request> {
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

pub(crate) fn header_value<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 去掉绝对 URI 里的 scheme + host，只留 path+query。
pub(crate) fn normalize_target(target: &str) -> &str {
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

/// 内置换号前缀表 —— 也是 `proxy-rules.json` 里 `swap_http_prefixes` 为空时的默认值。
///
/// **这份表必须按实测维护**：路径没命中就静默走透传（沿用 TraeWork 自己的 token），
/// 账号池一分不扣，而界面上完全看不出异常（`route_start` 照常会写，见 [`note_path`] 的注释）。
///
/// ## 🔴 这份内置表**不足以省额度**（2026-09-15 二轮实测定案）
///
/// 它是**保守基线**，不是「正确的扣费表」。实测证据（`takeover-journal.jsonl` 全量 246 条）：
/// 命中本表的**只有** `/api/remote/v1/chat_sessions/{pinned,repo_groups}` —— 而那是 **GET 列表**，
/// 一分额度都不消耗；真正发起任务的 `/api/agent/v3/{workflow/start,create_agent_task,llm_utils_chat}`、
/// `/api/ide/v1/super_completion_query`、`/api/solo_hub/v1/*` **全部落在表外走透传**。
/// ⇒ 症状就是「接管开着、池账号不涨、界面全绿」。**别把这个症状当成代理没生效。**
///
/// ## ⚠️ 关于 `/api/agent/v3/`：曾经加过、坏了、已回滚 —— 但当时的前提是错的
///
/// 旧记录说「推理根本不走 HTTP 反代（走 `bootConfig.ws` 的 WebSocket）」。**这条已被证伪**：
/// 整晚日志里 **0 条 `ws_*` 事件**，`bootConfig.ws` 那条通道压根没接到过流量，
/// 而 `lite.send_message` / `subscribe_events`（`adapter: local`）走的就是
/// **Rust adapter → HTTP `/api/agent/v3/*`**。所以 agent 路径**是**该换号的。
///
/// 真正让整段换号失败的原因更可能是**归属不一致**：把整段前缀一次性打开时，
/// 会话 id 只从 `/chat_sessions/<id>` **路径**认（见旧版 [`session_id_from_path`]），
/// `/api/agent/v3/*` 的 id 在 body 里 ⇒ 同会话的不同请求各自独立选号，上游看到
/// 「用 B 的凭据动 A 名下的会话」。
/// ⇒ 现在已经补上 [`conversation_key`]（路径 + 查询 + body）与 `unbound_session` 日志。
///
/// **正确做法**：用 `proxy-rules.json`（热加载、不用重编译）显式给出 `swap_http_prefixes`，
/// 把整段 `/api/agent/v3/` 与 `chat_sessions` **一起**放进同一个身份域（要么全换、要么全不换），
/// 且**从新建会话开始测**（旧会话的归属早已写死）。逐条加路径只会造成不一致。
const BILLING_PREFIXES: &[&str] = &[
    "/api/remote/v1/chat_sessions",
    "/api/remote/v1/models",
    "/api/remote/v1/file_converts",
];

/// 会话 id 藏在 **body** 里（URL 上看不出来）的路径域。
///
/// 列在这里的域，一旦被划进换号域，就必须能靠 [`conversation_key`] 从 body 认出会话；
/// 认不出就会「同会话串号」。所以这里是**唯一**会写 `unbound_session` 日志的地方 ——
/// 它是「能不能安全扩大换号域」的探针，而不是给列表接口报的噪音。
const BODY_BORNE_ID_PREFIXES: &[&str] = &["/api/agent/v3/"];

/// 已记过日志的请求路径（进程内去重 + 上限，避免无界增长）。
///
/// 存在的理由很具体：**「接管开着、池子账号却一分没扣」几乎总是因为判错了扣费路径** ——
/// TraeWork 换了接口名，或某条子资源路径只是长得像会话路径，
/// [`Rules::should_swap`](crate::rules::Rules::should_swap) 就会把它当透传放过去
/// （`/api/remote/v1/chat_sessions/repo_groups` 这种「列表页」路径就是活例子：它匹配扣费前缀、
/// 被换了凭据，却根本不消耗额度）。后果是账号池**静默失效**，界面上完全看不出异常。
/// 把「接管到底见过哪些路径、各判成什么」记进接管动态，这类问题一眼可查。
type SeenPaths = Mutex<HashSet<String>>;
fn seen_paths() -> &'static SeenPaths {
    static S: OnceLock<SeenPaths> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 已经报过「认不出会话」的路径（进程内一次，避免刷屏）。
///
/// 用独立的集合而不是 `journal::append_dedup`：后者只压**连续**重复，而 `route_start`
/// 会插在中间，等于压不住。这里一条路径只提醒一次就够 —— 这是配置型问题，不是事件。
type UnboundWarned = Mutex<HashSet<String>>;
fn unbound_warned() -> &'static UnboundWarned {
    static S: OnceLock<UnboundWarned> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 记一条「接管见过这条路」。`host` 与 **HTTP 方法**一并记进去。
///
/// 理由有两层，第二层是 2026-09-15 才补上的：
///
/// 1. 代理模式下会同时见到多个域名，只记 path 分不清请求去了哪（同一段 `/api/v1/...`
///    在不同域名下含义完全不同）；
/// 2. **方法才是「这条请求有没有资格决定扣费归属」的关键**。只有 `POST`（建会话 / 建任务）
///    会把归属写死，`GET .../chat_sessions/pinned` 这类列表路径哪怕被换了凭据也一分不扣 ——
///    而它恰好是旧日志里唯一带 `[扣费]` 标记的东西，于是「接管开着、池账号不涨」看上去
///    像是代理没生效，实际是**判据本身选错了**。方法进日志后，这类误判一眼可查。
fn note_path(dir: &std::path::Path, method: &str, host: &str, bare: &str) {
    const MAX_TRACKED: usize = 400;
    let Ok(mut set) = seen_paths().lock() else {
        return;
    };
    let key = format!("{method} {host} {bare}");
    if set.len() >= MAX_TRACKED || !set.insert(key.clone()) {
        return;
    }
    let rules = crate::rules::Rules::load(dir);
    let tag = if rules.observe_only {
        "观察"
    } else if rules.should_swap(bare, BILLING_PREFIXES) {
        "换号"
    } else {
        "透传"
    };
    journal::append(dir, "proxy_path", &format!("接管收到 [{tag}] {key}"));
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

/// 积分展示文案（供接管动态）：**带上到期时间** —— 选号的第一排序键就是它，
/// 不写出来的话用户没法核对「是不是真的先扣快到期的那个」。
fn credits_text(info: CreditInfo) -> String {
    if info.unlimited {
        return "积分不限量".into();
    }
    let base = match info.credits {
        Some(c) => format!("剩 {c} 积分"),
        None => "积分未知".into(),
    };
    match info
        .expiry_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(t) => format!(
            "{} · {} 到期",
            base,
            t.with_timezone(&chrono::Local).format("%m-%d")
        ),
        None => base,
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
        let endpoint_base = endpoint::base_url(port);

        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                set_status(ProxyStatus {
                    active: true,
                    port,
                    error: None,
                });
                eprintln!("[接管] 反代监听 {endpoint_base}（明文回环，免证书）");
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
                // 端口被占是最常见也最没用的一句报错：只说「被占了」，不说谁占的、也不说换哪个。
                // 这里补成一句自带下一步动作的话（`portcheck` 探测失败时自动退化为原样）。
                let msg = if e.kind() == std::io::ErrorKind::AddrInUse {
                    format!("端口 {port} {}", crate::portcheck::busy_hint(port))
                } else {
                    format!("端口 {port} 无法监听：{e}")
                };
                set_status(ProxyStatus {
                    active: false,
                    port,
                    error: Some(msg.clone()),
                });
                eprintln!("[接管] 无法监听 127.0.0.1:{port}：{e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

fn handle_conn(mut tcp: TcpStream, app: tauri::AppHandle) {
    // 数据目录先取：选账号、写接管动态都要用；取不到直接 500，不再读请求
    let dir = match commands::try_data_dir(&app) {
        Ok(d) => d,
        Err(_) => {
            respond(&mut tcp, 500, "text/plain", b"internal error", &[]);
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
    if tcp.set_nonblocking(false).is_err() {
        journal::append(
            &dir,
            "proxy_error",
            "无法把连接复位成阻塞模式，已放弃该请求（可能是系统 socket 异常）",
        );
        return;
    }
    let _ = tcp.set_read_timeout(Some(HEAD_READ_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));

    // 下游这条通道**恒为明文**：端点写的是 `http://127.0.0.1:{port}`（见模块文档）。
    // 曾经这里还有一段「先 peek 首字节判断是不是 TLS 握手」的排障逻辑 —— 它是证书模式
    // 专属的（那时端点只讲 https，客户端发错 scheme 会得到一句看不懂的 rustls 报错），
    // 随该模式一起删掉。现在唯一的连接类型就是裸 TCP。
    let mut stream = tcp;

    let (buf, req) = read_request(&mut stream);
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

    // ① 拒绝 CONNECT。本端点接的是「TraeWork 按改写后的端点发来的普通请求」，
    //    不是任何客户端的正向代理 —— CONNECT 属于**已移除**的「经系统代理接管」。
    //    留着不答会让对端一直等（表现为请求挂死），所以当场回一句 405 说清楚。
    if req.method == "CONNECT" {
        journal::append_dedup(
            &dir,
            "proxy_bad_request",
            "收到 CONNECT：本端点只服务改写后的 TraeWork 请求，不做正向代理隧道（已回 405）",
        );
        respond(
            &mut stream,
            405,
            "text/plain",
            b"this endpoint is not a forward proxy\n",
            &[],
        );
        return;
    }

    // ② 上游：恒为 `product.json` 里记录的原始域名。
    let Some(up) = upstream_host() else {
        journal::append_dedup(
            &dir,
            "proxy_error",
            "无法从 TraeWork 的 product.json 解析出原始上游域名，接管无法回源",
        );
        respond(
            &mut stream,
            502,
            "text/plain",
            b"cannot resolve upstream from product.json",
            &[],
        );
        return;
    };

    // 实时通道（WebSocket）单独走一条路：握手之后它就不再是 HTTP 了，
    // 不能进 handle_transparent 那套「按报文解析 + 按路径换凭据」的逻辑。
    if is_websocket_upgrade(&req) {
        let Some(up) = ws_upstream() else {
            journal::append_dedup(
                &dir,
                "proxy_error",
                "收到 WebSocket 升级，但无法从 TraeWork 的 product.json 解析出 ws 上游地址",
            );
            respond(
                &mut stream,
                502,
                "text/plain",
                b"cannot resolve websocket upstream",
                &[],
            );
            return;
        };
        // 路径必须落在 ws 上游的前缀下，否则说明这条升级请求不该走 ws 通道
        let bare = normalize_target(&req.target).to_string();
        if !up.path.is_empty() && !bare.starts_with(up.path.as_str()) {
            journal::append_dedup(
                &dir,
                "proxy_error",
                &format!(
                    "WebSocket 升级请求的路径 {bare} 不在 ws 上游前缀 {} 下，已拒绝（避免把非实时请求塞进实时通道）",
                    up.path
                ),
            );
            respond(&mut stream, 502, "text/plain", b"websocket path not routed", &[]);
            return;
        }
        handle_websocket(&mut stream, &buf, &req, &dir, &up, Some(up.path.as_str()));
        return;
    }

    // 接管语义：一律透明反代到官方上游，仅替换鉴权凭据。
    handle_transparent(&mut stream, &buf, &req, &dir, &up);
}

/// 读一条完整请求（头 + 声明长度的体），返回（已读字节，解析结果）。
pub(crate) fn read_request(stream: &mut TcpStream) -> (Vec<u8>, Option<Request>) {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return (buf, None),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(req) = parse_request(&buf) {
                    if buf.len() >= req.head_end + req.body_len {
                        return (buf, Some(req));
                    }
                }
                if buf.len() > MAX_HEAD + MAX_BODY {
                    return (buf, None);
                }
            }
            Err(_) => return (buf, None),
        }
    }
}

/// 诊断用：把已收到的字节截成可读前缀（最多 512 字节）。
pub(crate) fn head_prefix(buf: &[u8]) -> String {
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

/// 会话 id 也可能挂在查询串上（`?session_id=…`）。
fn session_id_from_query(target: &str) -> Option<String> {
    const KEYS: [&str; 4] = ["session_id", "sessionId", "conversation_id", "conversationId"];
    let q = normalize_target(target).split_once('?')?.1;
    q.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if !KEYS.iter().any(|want| k.eq_ignore_ascii_case(want)) {
            return None;
        }
        let v = v.trim();
        if v.is_empty() || v.len() > 128 {
            return None;
        }
        Some(v.to_string())
    })
}

/// body 里可能承载会话 id 的键名。两侧序列化风格不一致（下划线 / 驼峰），只认一种迟早踩空。
const SESSION_ID_KEYS: [&str; 6] = [
    "\"session_id\"",
    "\"sessionId\"",
    "\"conversation_id\"",
    "\"conversationId\"",
    "\"chat_session_id\"",
    "\"chatSessionId\"",
];

/// 只扫开头这么多字节：会话 id 永远在 body 前部，而推理请求的 body 可以很大。
const BODY_SCAN_MAX: usize = 64 * 1024;

/// 在 JSON body 里找会话 id。
///
/// **故意不做完整 JSON 解析**：body 可能带附件、几十 MB，解析的代价和失败面都比
/// 「找几个键名」大得多，而这里只需要一个稳定的短字符串。
fn session_id_from_body(body: &[u8]) -> Option<String> {
    let scan = &body[..body.len().min(BODY_SCAN_MAX)];
    for key in SESSION_ID_KEYS {
        let mut from = 0usize;
        while let Some(i) = find_slice(&scan[from..], key.as_bytes()) {
            let at = from + i + key.len();
            if let Some(v) = json_string_value(&scan[at..]) {
                return Some(v);
            }
            from = at;
        }
    }
    None
}

/// `: ""` → `true` —— 键**在**、值**是空串**。
fn empty_string_value(rest: &[u8]) -> bool {
    let mut i = 0;
    while rest.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    if rest.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    while rest.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    rest.get(i) == Some(&b'"') && rest.get(i + 1) == Some(&b'"')
}

/// body 里**显式**把会话 id 写成空串 —— 这是「新建会话」的形态，不是「认不出会话」。
///
/// 实测形态：新建会话那条请求的 body 是 `{"session_id":"", …}`（`[RustAdapter][CreateAgentTask]`
/// 打印出来就是 `route=chat.createSession session_id=`），服务端随后才分配 id；新会话归谁
/// 由**这条请求自己的 token** 决定（用谁的 token 建，就算谁的账），不存在串号风险。
///
/// 所以要区分「键在但为空」（新建，安全换号）与「键根本不在」（多半是会话内的动作，
/// 认不出就会串号）。不区分的话，每次新建会话都会误报一次「会话认不出」，
/// 而这条探针的价值全在于**它不响** —— 假警报多了就没人看了。
fn body_declares_empty_session(body: &[u8]) -> bool {
    let scan = &body[..body.len().min(BODY_SCAN_MAX)];
    for key in SESSION_ID_KEYS {
        let mut from = 0usize;
        while let Some(i) = find_slice(&scan[from..], key.as_bytes()) {
            let at = from + i + key.len();
            if empty_string_value(&scan[at..]) {
                return true;
            }
            from = at;
        }
    }
    false
}

/// `: "value"` → `value`。不是紧跟的字符串字面量就返回 `None`（免得把数字/对象当 id）。
fn json_string_value(rest: &[u8]) -> Option<String> {
    let mut i = 0;
    while rest.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    if rest.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while rest.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    if rest.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let start = i;
    while let Some(&b) = rest.get(i) {
        if b == b'"' {
            break;
        }
        if b == b'\\' || b < 0x20 {
            // 带转义 / 控制字符 ⇒ 不是我们想要的 id，别猜
            return None;
        }
        i += 1;
    }
    let raw = std::str::from_utf8(rest.get(start..i)?).ok()?;
    if raw.is_empty() || raw.len() > 128 {
        return None;
    }
    Some(raw.to_string())
}

/// 认出「这条请求属于哪个会话」——**所有**取 id 的途径收在这一个入口里。
///
/// ⚠️ 只从路径取（旧实现）远远不够：`/api/agent/v3/*` 那族把会话 id 放在 **body** 里，
/// 于是它们即使被划进换号域也**互相认不出**，同一会话的不同请求各自独立选号 ⇒ 上游看到
/// 「用 B 的凭据动 A 名下的会话」，表现就是一串莫名其妙的鉴权 / 限流失败。
/// **认得出的会话越多，整段换号就越安全** —— 这是能否扩大换号域的前置条件。
fn conversation_key(target: &str, body: &[u8]) -> Option<String> {
    session_id_from_path(target)
        .or_else(|| session_id_from_query(target))
        .or_else(|| session_id_from_body(body))
}

/// 原始上游 HTTP 主机（取自 TraeWork `product.json`，不硬编码；进程内缓存一次）。
fn upstream_host() -> Option<String> {
    static UP: OnceLock<Option<String>> = OnceLock::new();
    UP.get_or_init(|| endpoint::read_upstreams().0).clone()
}

/// `https://host[:port]/...` → `host`（只用于日志与规则匹配，不做校验）。
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let auth = rest.split(['/', '?']).next().unwrap_or(rest);
    auth.split(':').next().unwrap_or(auth).to_ascii_lowercase()
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

/// 向上游发一次请求：方法 / 路径 / 请求头 / 请求体原样透传，**只有凭据由调用方决定**。
///
/// - `auth = Some(v)` ⇒ 用 `v` 覆盖 `Authorization`（池化账号）；
/// - `auth = None` ⇒ 连 `Authorization` 都不带（客户端本来就没发）。
fn send_upstream(
    req: &Request,
    url: &str,
    body: &[u8],
    auth: Option<&str>,
) -> Result<reqwest::Response, reqwest::Error> {
    tauri::async_runtime::block_on(async {
        let mut r = CLIENT.request(
            reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET),
            url,
        );
        if let Some(a) = auth {
            r = r.header("Authorization", a);
        }
        for (k, v) in &req.headers {
            if !hop_by_hop(k) {
                r = r.header(k.as_str(), v.as_str());
            }
        }
        if !body.is_empty() {
            r = r.body(body.to_vec());
        }
        r.send().await
    })
}

// ---------------------------------------------------------------------------
// WebSocket 透传（实时通道）
// ---------------------------------------------------------------------------
//
// 为什么非有不可：SOLO 模式的对话走 `bootConfig.ws` 那条 **WebSocket**
// （`wss://trae-ws-cn.mchost.guru/custom_model`），它压根不走 HTTP 反代——渲染进程日志里
// 是 `lite.send_message` / `lite.subscribe_events`（`adapter: local`）。
// 不把这条通道接住，改道就只覆盖了一半流量，账号池也就永远用不上。
//
// 这一版刻意**只做原样透传**：握手请求一个字段都不改，101 之后退化成裸字节隧道。
// 身份替换留到看清握手里带什么之后再说 —— 2026-09-15 已经栽过一次
// 「以为换对了、结果把应用弄坏」的跟头，这次先证明「接住不破坏」。

/// WebSocket 上游（取自 `product.json`，不硬编码）。
pub(crate) struct WsUpstream {
    /// `wss://` → 需要 TLS（端口默认 443）；`ws://` 则直连。
    pub(crate) secure: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
    /// 路径前缀（如 `/custom_model`）：反代据此把它和普通 HTTP 请求区分开。
    pub(crate) path: String,
}

/// 轮询式转发的读超时，同时也是「一个方向最多要等多久才轮到」的上限。
///
/// TLS 连接**没法拆成两个线程各占一个方向**（`StreamOwned` 的 read 与 write 都要 `&mut`，
/// 而 `Arc<Mutex<_>>` 会让阻塞读把写方向一起锁死）。所以单线程交替读写，
/// 靠这个短超时保证两个方向都能及时轮到。30ms 对控制通道够用。
const WS_POLL: Duration = Duration::from_millis(30);
/// 双向都彻底静默多久就收摊。正常是长连接，但别为一个已经断掉的连接永远空转。
const WS_IDLE_MAX: Duration = Duration::from_secs(30 * 60);

/// 反代作为 WS **客户端**连上游时用的上游连接。
enum Upstream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Upstream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Upstream::Plain(s) => s.read(buf),
            Upstream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Upstream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Upstream::Plain(s) => s.write(buf),
            Upstream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Upstream::Plain(s) => s.flush(),
            Upstream::Tls(s) => s.flush(),
        }
    }
}

/// 连官方 ws 上游用的客户端根证书。走公开根（webpki-roots）而不是系统信任库：
/// 目标是公网域名，与用户钥匙串无关，少一层「依赖用户环境」的变数。
fn ws_client_config() -> Option<&'static Arc<rustls::ClientConfig>> {
    static C: OnceLock<Option<Arc<rustls::ClientConfig>>> = OnceLock::new();
    C.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Some(Arc::new(cfg))
    })
    .as_ref()
}

/// 原始 ws 上游地址（进程内缓存一次）。
fn ws_upstream() -> Option<&'static WsUpstream> {
    static UP: OnceLock<Option<WsUpstream>> = OnceLock::new();
    UP.get_or_init(|| {
        let raw = endpoint::read_upstreams().1?;
        let (scheme, rest) = raw.split_once("://")?;
        let secure = scheme.eq_ignore_ascii_case("wss");
        let (host_port, path) = match rest.split_once('/') {
            Some((h, p)) => (h, format!("/{}", p.split(['?', '#']).next().unwrap_or(""))),
            None => (rest, String::new()),
        };
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
            None => (host_port.to_string(), if secure { 443 } else { 80 }),
        };
        if host.is_empty() {
            return None;
        }
        Some(WsUpstream {
            secure,
            host,
            port,
            path,
        })
    })
    .as_ref()
}

/// 这条请求是不是 WebSocket 升级握手。
pub(crate) fn is_websocket_upgrade(req: &Request) -> bool {
    header_value(req, "upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
        || req
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("sec-websocket-key") && !v.is_empty())
}

/// 记录一次 WS 握手请求里出现过的头**名字**（只记名字，绝不记值）。
///
/// 目的是回答一个很具体的问题：**身份藏在哪个头里**。换凭据之前必须先知道这一点，
/// 否则就会重复 2026-09-15 那次「以为换对了、结果换坏了」。
fn note_ws_handshake(dir: &std::path::Path, req: &Request) {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        return;
    }
    let mut names: Vec<String> = req
        .headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();
    let has_auth = names
        .iter()
        .any(|n| n == "authorization" || n.contains("token") || n.contains("cookie"));
    journal::append(
        dir,
        "ws_handshake",
        &format!(
            "WebSocket 握手请求头（仅名字）：[{}] {}",
            names.join(", "),
            if has_auth {
                "—— 含鉴权类头，身份很可能就在这里 ⇒ 可在此替换凭据"
            } else {
                "—— 没有明显的鉴权头，身份多半在握手之后的首帧里（要换得解析帧）"
            }
        ),
    );
}

fn open_ws_upstream(up: &WsUpstream) -> Result<Upstream, String> {
    let tcp = TcpStream::connect((up.host.as_str(), up.port))
        .map_err(|e| format!("连接 {}:{} 失败：{e}", up.host, up.port))?;
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(WS_POLL));
    let _ = tcp.set_write_timeout(Some(IDLE_TIMEOUT));
    if !up.secure {
        return Ok(Upstream::Plain(tcp));
    }
    let cfg = ws_client_config().ok_or_else(|| "无法构建 TLS 客户端配置".to_string())?;
    let name = rustls::pki_types::ServerName::try_from(up.host.clone())
        .map_err(|e| format!("上游主机名不合法：{e}"))?;
    let conn = rustls::ClientConnection::new(cfg.clone(), name)
        .map_err(|e| format!("创建 TLS 会话失败：{e}"))?;
    Ok(Upstream::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
}

/// 原样转发握手请求，**只改 Host**（客户端发给我们的是 `127.0.0.1:PORT`，
/// 直接抛给上游会让它按错误的主机名处理）。
///
/// `auth = Some(v)` 时**顺带把 `Authorization` 换成 `v`**（账号池）。原始那一行会被丢弃 ——
/// 留着就会同时出现两个 `Authorization`，而「服务器取第一个还是最后一个」是实现细节，
/// 不能赌。
fn build_ws_request(buf: &[u8], req: &Request, up: &WsUpstream, auth: Option<&str>) -> Vec<u8> {
    let head_end = req.head_end.min(buf.len());
    let text = String::from_utf8_lossy(&buf[..head_end]);
    let default_port = if up.secure { 443 } else { 80 };
    let host = if up.port == default_port {
        up.host.clone()
    } else {
        format!("{}:{}", up.host, up.port)
    };
    let mut out = String::with_capacity(text.len() + 64);
    for (i, line) in text.split("\r\n").enumerate() {
        if line.is_empty() {
            continue;
        }
        if i == 0 {
            out.push_str(&format!(
                "{} {} HTTP/1.1\r\n",
                req.method,
                normalize_target(&req.target)
            ));
            continue;
        }
        let name = line.split_once(':').map(|(n, _)| n.trim()).unwrap_or("");
        if name.eq_ignore_ascii_case("host") {
            out.push_str(&format!("Host: {host}\r\n"));
            continue;
        }
        if name.eq_ignore_ascii_case("authorization") {
            // `auth = None`（默认：不换 WS 凭据）时必须**原样保留**这一行 ——
            // 悄悄丢掉它会让整条实时通道失去身份，表现为握手 401，
            // 而排查方向会全跑偏到「上游挂了」。
            match auth {
                Some(v) => out.push_str(&format!("Authorization: {v}\r\n")),
                None => {
                    out.push_str(line);
                    out.push_str("\r\n");
                }
            }
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// 读到上游应答的头部（含结尾的空行）。
fn read_head(up: &mut Upstream) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        match up.read(&mut tmp) {
            Ok(0) => return Err("上游在应答握手前就关闭了连接".into()),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if find_slice(&buf, b"\r\n\r\n").is_some() {
                    return Ok(buf);
                }
                if buf.len() > MAX_HEAD {
                    return Err("上游应答头过大".into());
                }
            }
            Err(e) => return Err(format!("读上游应答失败：{e}")),
        }
    }
}

fn status_of(head: &[u8]) -> Option<u16> {
    let end = find_slice(head, b"\r\n")?;
    let line = std::str::from_utf8(&head[..end]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn is_poll_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

fn set_poll_timeout(c: &TcpStream) {
    let _ = c.set_read_timeout(Some(WS_POLL));
}

fn set_poll_timeout_up(u: &Upstream) {
    let sock = match u {
        Upstream::Plain(s) => s,
        Upstream::Tls(s) => &s.sock,
    };
    let _ = sock.set_read_timeout(Some(WS_POLL));
}

/// 上游应答不是 101 时，把剩余字节原样接完（此时上游多半会自己关连接）。
fn relay_until_eof(up: &mut Upstream, down: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match up.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                down.write_all(&buf[..n])?;
                down.flush()?;
            }
            Err(_) => return Ok(()),
        }
    }
}

/// 101 之后的双向裸字节转发，直到任意一端关闭或长时间双向静默。
fn relay_forever(down: &mut TcpStream, up: &mut Upstream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    let mut idle = Instant::now();
    loop {
        let mut moved = false;
        match down.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                up.write_all(&buf[..n])?;
                up.flush()?;
                moved = true;
            }
            Err(e) if is_poll_timeout(&e) => {}
            Err(_) => return Ok(()),
        }
        match up.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                down.write_all(&buf[..n])?;
                down.flush()?;
                moved = true;
            }
            Err(e) if is_poll_timeout(&e) => {}
            Err(_) => return Ok(()),
        }
        if moved {
            idle = Instant::now();
        } else if idle.elapsed() > WS_IDLE_MAX {
            return Ok(());
        }
    }
}

/// WebSocket 接管：解析上游 → 交给 [`ws_handshake_and_relay`]。
///
/// 只做「解析 + 路径校验」，真正的握手与隧道转发放在纯部件里，
/// 这样测试能拿一个**本地假上游**把它整条跑通（不必碰用户真机上的 TraeWork）。
pub(crate) fn handle_websocket(
    down: &mut TcpStream,
    buf: &[u8],
    req: &Request,
    dir: &std::path::Path,
    up: &WsUpstream,
    path_guard: Option<&str>,
) {
    let bare = normalize_target(&req.target).to_string();
    note_path(dir, &req.method, &up.host, &bare);
    // 前缀护栏：这里是「冒充官方域名」的端点模式，必须确认这条升级请求本来就该走 ws 通道
    // （在此之前还支持过中间人模式，那时 CONNECT 的目标域名本身就是依据、护栏传 `None` ——
    //  那条路已随「经系统代理接管」一起移除）。
    if let Some(prefix) = path_guard {
        if !prefix.is_empty() && !bare.starts_with(prefix) {
            journal::append_dedup(
                dir,
                "proxy_error",
                &format!(
                    "WebSocket 升级请求的路径 {bare} 不在 ws 上游前缀 {prefix} 下，已拒绝（避免把非实时请求塞进实时通道）"
                ),
            );
            return respond(down, 502, "text/plain", b"websocket path not routed", &[]);
        }
    }

    note_ws_handshake(dir, req);

    // WebSocket 换不换凭据由规则决定，**默认不换** —— 它是实时长连接，
    // 一旦服务端按「连接归属」校验，混用账号的表现是一串莫名其妙的鉴权失败。
    let rules = crate::rules::Rules::load(dir);
    let auth = if rules.swap_ws && !rules.observe_only {
        let conv = session_id_from_path(&req.target)
            .or_else(|| header_value(req, "x-conversation-id").map(str::to_string))
            .filter(|s| !s.is_empty());
        match tauri::async_runtime::block_on(choose_account(dir, conv.as_deref(), &[])) {
            Some(a) => {
                journal::append(
                    dir,
                    "ws_swap",
                    &format!("WebSocket 握手凭据已换成账号「{}」（{bare}）", a.name),
                );
                Some(format!("Cloud-IDE-JWT {}", a.token))
            }
            None => None,
        }
    } else {
        None
    };

    ws_handshake_and_relay(down, buf, req, up, dir, auth.as_deref());
}

/// 握手转发 + 隧道转发（纯部件：上游由调用方给出，故可用本地假上游整条测试）。
fn ws_handshake_and_relay(
    down: &mut TcpStream,
    buf: &[u8],
    req: &Request,
    up: &WsUpstream,
    dir: &std::path::Path,
    auth: Option<&str>,
) {
    let bare = normalize_target(&req.target).to_string();
    let mut upstream = match open_ws_upstream(up) {
        Ok(s) => s,
        Err(e) => {
            journal::append(dir, "proxy_error", &format!("WebSocket 上游不可用：{e}"));
            return respond(
                down,
                502,
                "text/plain",
                format!("websocket upstream error: {e}").as_bytes(),
                &[],
            );
        }
    };

    let head = build_ws_request(buf, req, up, auth);
    if let Err(e) = upstream.write_all(&head).and_then(|_| upstream.flush()) {
        journal::append(dir, "proxy_error", &format!("转发 WebSocket 握手失败：{e}"));
        return respond(down, 502, "text/plain", b"websocket handshake write failed", &[]);
    }

    let resp_head = match read_head(&mut upstream) {
        Ok(h) => h,
        Err(e) => {
            journal::append(dir, "proxy_error", &format!("读取 WebSocket 上游应答失败：{e}"));
            return respond(
                down,
                502,
                "text/plain",
                format!("websocket handshake read failed: {e}").as_bytes(),
                &[],
            );
        }
    };
    let status = status_of(&resp_head).unwrap_or(0);
    if let Err(e) = down.write_all(&resp_head).and_then(|_| down.flush()) {
        journal::append(dir, "proxy_error", &format!("回写 WebSocket 应答失败：{e}"));
        return;
    }
    if status != 101 {
        journal::append(
            dir,
            "proxy_upstream_status",
            &format!("WebSocket 握手被上游拒绝（{status}，{bare}）"),
        );
        let _ = relay_until_eof(&mut upstream, down);
        return;
    }
    journal::append(
        dir,
        "ws_open",
        &format!("WebSocket 通道已接通，开始原样透传（{bare}）"),
    );
    set_poll_timeout(down);
    set_poll_timeout_up(&upstream);
    let _ = relay_forever(down, &mut upstream);
}

/// 接管核心：把 TraeWork 原生请求透传到原始上游。
///
/// - 路径 / 查询 / 请求体原样透传；
/// - **扣费路径**（[`BILLING_PREFIXES`]，可被 `proxy-rules.json` 覆盖）：
///   `Authorization` 换成池化账号的 `Cloud-IDE-JWT`，按会话粘滞（同一会话固定同一账号，
///   避免账号错配），429 时并入冷却、解绑粘滞、换账号重试（上限 [`FAILOVER_MAX_TRIES`]）；
/// - **其余路径**：连凭据一起原样透传，不参与账号池；
/// - 响应（含 SSE）边收边转，并叠加 CORS 头。
///
/// `upstream` 是**上游基址**（如 `https://trae-api-cn.mchost.guru`），由调用方给出：
/// 端点模式下它来自 `product.json` 里记录的原始域名，中间人模式下它就是 CONNECT 的目标域名。
pub(crate) fn handle_transparent(
    stream: &mut TcpStream,
    buf: &[u8],
    req: &Request,
    dir: &std::path::Path,
    upstream: &str,
) {
    let cors = cors_headers(req);

    if req.method == "OPTIONS" {
        respond_head_only(stream, 204, &cors);
        return;
    }

    let host = host_of(upstream);
    let body_bytes = req_body(buf, req).to_vec();
    let bare = normalize_target(&req.target).to_string();
    let conv = conversation_key(&req.target, &body_bytes)
        .or_else(|| header_value(req, "x-conversation-id").map(str::to_string))
        .filter(|s| !s.is_empty());
    note_path(dir, &req.method, &host, &bare);
    let url = format!("{}{}", upstream.trim_end_matches('/'), bare);
    let rules = crate::rules::Rules::load(dir);
    let swapping = rules.should_swap(&bare, BILLING_PREFIXES);

    // ①′ **能力探针**：会话 id 藏在 body 里的域，我们到底认不认得出会话？
    //
    // 这一条放在换号判定**之前**，因为它与「这次换不换」无关：它回答的是
    // **「这个域能不能安全地被划进换号域」**。换号的安全性完全取决于「同会话的请求认不认得同一个会话」——
    // 认得出，整段换号就是一个自洽的身份域；认不出，同会话的不同请求各选各的账号，
    // 上游看到「用 B 的凭据动 A 名下的会话」，回的就是一串莫名其妙的鉴权/限流错
    // （2026-09-15「操作过于频繁」就是这个形态）。所以**观察模式下也照写**：
    // 它是扩大换号域的前置条件，不是换号的副产品。
    //     ⚠️ 例外：body 里会话 id 是**空串** ⇒ 这条请求本身要**新建**会话（新会话归谁由
    //     这次请求的 token 决定，见 [`body_declares_empty_session`]），不算「认不出」。
    if conv.is_none()
        && !body_declares_empty_session(&body_bytes)
        && BODY_BORNE_ID_PREFIXES.iter().any(|p| bare.starts_with(p))
    {
        let first = unbound_warned()
            .lock()
            .map(|mut s| s.insert(bare.clone()))
            .unwrap_or(false);
        if first {
            journal::append(
                dir,
                "unbound_session",
                &format!(
                    "[{bare}] 这条请求的会话 id 在 body 里，但没能认出来 ⇒ 该域**不能**整体换号，\
                     否则同会话的请求会用到不同账号（上游多半报鉴权或限流错）"
                ),
            );
        }
    }

    // ① 非换号路径（或观察模式）：**原样**转发，保留 TraeWork 自己的 `Authorization`。
    //    这类请求不需要账号池，所以池子空着也照常工作（不该因为没勾账号就 503）。
    if !swapping {
        let auth = header_value(req, "authorization").map(str::to_string);
        match send_upstream(req, &url, &body_bytes, auth.as_deref()) {
            Ok(resp) => stream_response_passthrough(stream, resp, &cors, dir, &bare),
            Err(e) => {
                journal::append(dir, "proxy_error", &format!("[{bare}] 上游请求失败：upstream error: {e}"));
                respond(
                    stream,
                    502,
                    "text/plain",
                    format!("upstream error: {e}").as_bytes(),
                    &[],
                );
            }
        }
        return;
    }

    // ② 换号路径：换成池化账号的凭据，并在 429 时无感换号重试
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

        let credential = format!("Cloud-IDE-JWT {}", account.token);
        let upstream = send_upstream(req, &url, &body_bytes, Some(&credential));

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

pub(crate) fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8], extra: &[(&str, &str)]) {
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

    /// 测试用的判据入口：走**默认规则**（= 内置表），与线上同一条代码路径。
    fn swap_for(path: &str) -> bool {
        crate::rules::Rules::default().should_swap(path, BILLING_PREFIXES)
    }

    #[test]
    fn pooled_token_only_for_billing_paths() {
        // 会扣积分的路径才换凭据
        assert!(swap_for("/api/remote/v1/chat_sessions"));
        assert!(swap_for("/api/remote/v1/chat_sessions/c1/messages"));
        assert!(swap_for("/api/remote/v1/chat_sessions/c1/events"));
        assert!(swap_for("/api/remote/v1/models"));
        assert!(swap_for("/api/remote/v1/file_converts/start"));
        // 其余一律沿用 TraeWork 自己的身份，别让用户看到「别人账号的数据」
        for p in [
            "/api/remote/v1/skills?page_size=200",
            "/api/remote/v1/git/repositories",
            "/api/remote/v1/user/settings",
            "/api/remote/v1/scheduled_tasks?page_size=100",
            "/api/remote/v1/plugins?page_size=50",
            "/api/solo_hub/v1/apps/online",
            "/api/solo_hub/v1/conversations/messages/batchInsert",
            "/api/ide/v1/features",
            "/api/ide/v1/knowledgebase/ckg_config",
            "/api/v1/commercial/get_session_usage",
            "/healthz",
        ] {
            assert!(!swap_for(p), "{p} 不该换凭据");
        }
        // ⚠️ 内置表**故意**不含 agent 路径，但**别把这条读成「agent 路径不该换」**。
        //    2026-09-15 实测把 `/api/agent/v3/` 整段打开后 TraeWork 每条消息回「操作过于频繁」，
        //    当时的解释是「推理走 `bootConfig.ws` 的 WebSocket，HTTP 换不到」——**已被证伪**：
        //    那晚日志里 0 条 `ws_*` 事件，而 `lite.send_message`/`subscribe_events` 走的就是
        //    HTTP `/api/agent/v3/*`。真正的原因是**归属不一致**（会话 id 在 body 里，旧版认不出，
        //    同会话被分到不同账号）。
        //    ⇒ 想动这里：改 `proxy-rules.json`，把整段 `/api/agent/v3/` 与 `chat_sessions`
        //    **一起**放进同一个身份域，并从**新建会话**开始测。看到这条断言失败，先读函数文档。
        for p in [
            "/api/agent/v3/llm_utils_chat",
            "/api/agent/v3/create_agent_task",
            "/api/agent/v3/workflow/start",
            "/api/agent/v3/sync_history_state",
            "/api/ide/v1/super_completion_query",
        ] {
            assert!(!swap_for(p), "{p} 不在内置表里（内置表是保守基线，见函数文档）");
        }
    }

    #[test]
    fn conversation_key_reads_path_query_and_json_body() {
        // 路径形态（`chat_sessions` 一族）
        assert_eq!(
            conversation_key("/api/remote/v1/chat_sessions/c1/messages", b"").as_deref(),
            Some("c1")
        );
        // 查询串形态
        assert_eq!(
            conversation_key("/api/agent/v3/x?session_id=c2&page=1", b"").as_deref(),
            Some("c2")
        );
        // body 形态 —— `/api/agent/v3/*` 的真实样子：URL 上根本看不出会话
        assert_eq!(
            conversation_key("/api/agent/v3/workflow/start", br#"{"session_id":"c3","query":"hi"}"#)
                .as_deref(),
            Some("c3")
        );
        // 驼峰也认（两侧序列化风格不一致，只认一种迟早会踩空）
        assert_eq!(
            conversation_key("/api/agent/v3/create_agent_task", br#"{"sessionId": "c4"}"#).as_deref(),
            Some("c4")
        );
    }

    #[test]
    fn conversation_key_does_not_guess() {
        // 非字符串值不是 id：乱猜一个数字当会话键，比认不出更危险（会把无关请求粘到一起）
        assert_eq!(conversation_key("/api/agent/v3/x", br#"{"session_id":123}"#), None);
        assert_eq!(conversation_key("/api/agent/v3/x", br#"{"session_id":{"a":1}}"#), None);
        assert_eq!(conversation_key("/api/agent/v3/x", br#"{"session_id":""}"#), None);
        assert_eq!(conversation_key("/api/agent/v3/x", br#"{"session_id":null}"#), None);
        // 列表接口本来就没有会话概念 —— 不该被算成「认出失败」
        assert_eq!(conversation_key("/api/remote/v1/chat_sessions?mode=work", b""), None);
        assert_eq!(conversation_key("/api/agent/v3/x?session_id=&a=1", b""), None);
        assert_eq!(conversation_key("/healthz", b""), None);
    }

    #[test]
    fn empty_session_id_means_new_session_not_unbound() {
        // 新建会话：body 里 session_id 是空串 —— 归属由这次请求的 token 决定，不算「认不出」
        assert!(body_declares_empty_session(br#"{"session_id":"","query":"hi"}"#));
        assert!(body_declares_empty_session(br#"{"sessionId": ""}"#));
        assert!(body_declares_empty_session(br#"{"chat_session_id":""}"#));
        // 键压根不在 ⇒ 确实认不出，探针该响
        assert!(!body_declares_empty_session(br#"{"query":"hi"}"#));
        // 键在但值不是「空字符串」⇒ 不算新建（数字 / null / 非空串都不认）
        assert!(!body_declares_empty_session(br#"{"session_id":null}"#));
        assert!(!body_declares_empty_session(br#"{"session_id":"abc"}"#));
        assert!(!body_declares_empty_session(br#"{"session_id":123}"#));
        // 与主路径一致：认得出 id 时就该走粘滞，不该判成新建
        assert_eq!(
            conversation_key("/api/agent/v3/workflow/start", br#"{"session_id":"c9"}"#).as_deref(),
            Some("c9")
        );
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

    /// 转发给上游的握手请求：**只改 Host**，其余头一个不动（身份类头必须原样带过去）。
    #[test]
    fn ws_request_rewrites_only_the_host_header() {
        let raw = b"GET /custom_model HTTP/1.1\r\nHost: 127.0.0.1:8788\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nAuthorization: Cloud-IDE-JWT abc.def\r\nSec-WebSocket-Key: kk==\r\n\r\n";
        let req = parse_request(raw).unwrap();
        let up = WsUpstream {
            secure: true,
            host: "trae-ws-cn.mchost.guru".into(),
            port: 443,
            path: "/custom_model".into(),
        };
        let head = String::from_utf8(build_ws_request(raw, &req, &up, None)).unwrap();
        assert!(head.starts_with("GET /custom_model HTTP/1.1\r\n"), "{head}");
        assert!(head.contains("Host: trae-ws-cn.mchost.guru\r\n"), "{head}");
        assert!(!head.contains("127.0.0.1"), "不该把本机 Host 转发给上游：{head}");
        // 身份与握手头必须原样保留
        assert!(head.contains("Authorization: Cloud-IDE-JWT abc.def\r\n"), "{head}");
        assert!(head.contains("Sec-WebSocket-Key: kk==\r\n"), "{head}");
        assert!(head.contains("Upgrade: websocket\r\n"), "{head}");
        // 非默认端口才带端口
        let up2 = WsUpstream { port: 8443, ..up };
        let head2 = String::from_utf8(build_ws_request(raw, &req, &up2, None)).unwrap();
        assert!(head2.contains("Host: trae-ws-cn.mchost.guru:8443\r\n"), "{head2}");
    }

    /// WebSocket 透传整条链路：下游握手 → 连上游 → 转发握手 → 101 → **裸字节双向透传**。
    ///
    /// 刻意用**本地假上游**而不是真机：要证明的是「这条通道接得住、不破坏」，
    /// 而不是拿用户的 TraeWork 做实验 —— 2026-09-15 已经吃过一次「以为改对了、
    /// 结果把应用弄坏」的亏，这次先把风险关在测试里。
    #[test]
    fn websocket_passthrough_reaches_the_upstream_and_relays_bytes() {
        use std::io::BufRead;
        let dir = std::env::temp_dir().join(format!("twa-ws-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // 假 ws 上游：读完握手头 → 回 101 → 之后把收到的字节原样回显
        let up_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = up_listener.local_addr().unwrap().port();
        let up_thread = std::thread::spawn(move || {
            let (sock, _) = up_listener.accept().unwrap();
            let mut rd = std::io::BufReader::new(sock.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                if rd.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut wr = sock;
            wr.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .unwrap();
            wr.flush().unwrap();
            let mut buf = [0u8; 1024];
            loop {
                match rd.get_mut().read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        wr.write_all(&buf[..n]).unwrap();
                        wr.flush().unwrap();
                    }
                }
            }
        });

        // 下游一侧：让被代理的连接在本线程 accept
        let down_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let down_port = down_listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", down_port)).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            c.write_all(
                b"GET /custom_model HTTP/1.1\r\nHost: 127.0.0.1:8788\r\nUpgrade: websocket\r\n\
                  Connection: Upgrade\r\nSec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\
                  Sec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
            c.flush().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if c.read(&mut b).unwrap() == 0 {
                    break;
                }
                head.push(b[0]);
            }
            let text = String::from_utf8_lossy(&head).to_string();
            assert!(text.starts_with("HTTP/1.1 101"), "应当拿到 101，实际：{text}");
            // 隧道里发一串字节，必须原样回来
            c.write_all(b"ping-through-tunnel").unwrap();
            c.flush().unwrap();
            let mut got = vec![0u8; 64];
            let n = c.read(&mut got).unwrap();
            assert_eq!(&got[..n], b"ping-through-tunnel");
        });

        let (sock, _) = down_listener.accept().unwrap();
        let mut down = sock;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let req = loop {
            let n = down.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(r) = parse_request(&buf) {
                break r;
            }
        };
        let up = WsUpstream {
            secure: false,
            host: "127.0.0.1".into(),
            port: up_port,
            path: "/custom_model".into(),
        };
        ws_handshake_and_relay(&mut down, &buf, &req, &up, &dir, None);
        client.join().unwrap();
        up_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
