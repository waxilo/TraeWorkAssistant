//! 本地池化网关（默认 `127.0.0.1:8788`）：TraeWork「自定义模型 → 本地网关 → 账号池」。
//!
//! TraeWork 通过「设置 → 模型 → 添加自定义模型」指向：
//!    请求地址: `http://127.0.0.1:{port}/v1/chat/completions`
//!    模型 ID:   任何一个占位（网关自行路由）
//!
//! 网关对外是 **OpenAI 兼容协议**，对内按账号池为每个账号选一个上游推理端点：
//!    转发地址: `{account.host}/v1/chat/completions`
//!    鉴权头:  `Authorization: Cloud-IDE-JWT {account.token}`
//!    请求体:  原样透传（model 由网关决定，账号 token 由网关注入）
//!
//! 路由逻辑：
//! 1. **会话粘滞**：带 `x-conversation-id` 的会话复用上次账号（换号丢上下文），TTL 后释放；
//! 2. **限流无感切换**：某账号返回 429，打入 10 分钟冷却、解绑会话，换下一个账号重发
//!    （上限 2 次）；全部失败则原样透传最后一个 429；
//! 3. **白名单**：设置里勾选的账号才有资格被扣费；空 = 全部。
//!
//! 响应一律 chunked 流式下发（对话是 SSE，缓冲成一次 body 会报 Empty stream）。

use crate::accounts;
use crate::commands;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ACCEPT_POLL: Duration = Duration::from_millis(150);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const STICKY_TTL: Duration = Duration::from_secs(30 * 60);
const COOLDOWN_TTL: Duration = Duration::from_secs(10 * 60);
const FAILOVER_MAX_TRIES: usize = 3;
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

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
// 运行状态（供前端反馈：勾选后能立刻看到是否真的在监听）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct GatewayStatus {
    pub active: bool,
    pub port: u16,
    pub error: Option<String>,
}

static GATEWAY_STATUS: OnceLock<Mutex<GatewayStatus>> = OnceLock::new();
fn gateway_status_cell() -> &'static Mutex<GatewayStatus> {
    GATEWAY_STATUS.get_or_init(Default::default)
}

fn set_status(v: GatewayStatus) {
    if let Ok(mut g) = gateway_status_cell().lock() {
        *g = v;
    }
}

/// 供 Tauri 命令读取网关当前运行状态。
pub fn status() -> GatewayStatus {
    gateway_status_cell()
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

fn sticky_put(conv: &str, id: String, remove: bool) {
    if let Ok(mut map) = sticky_conv().lock() {
        if remove {
            map.remove(conv);
        } else {
            map.insert(conv.to_string(), (Instant::now(), id));
        }
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

fn is_chat_path(target: &str) -> bool {
    let bare = normalize_target(target);
    match bare.find('?') {
        Some(i) => bare[..i].eq_ignore_ascii_case("/v1/chat/completions"),
        None => bare.eq_ignore_ascii_case("/v1/chat/completions")
            || bare.eq_ignore_ascii_case("/v1/messages")
            || bare.eq_ignore_ascii_case("/chat/completions"),
    }
}

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

/// 选出账号：优先会话粘滞，否则剔除冷却账号后取第一个（可扩展为按剩余积分）。
async fn choose_account(
    dir: &std::path::Path,
    conv: Option<&str>,
    ban: &[String],
) -> Option<accounts::Account> {
    let settings = accounts::load_settings(dir);
    let all = accounts::load_accounts(dir);
    if all.is_empty() {
        return None;
    }
    let candidates = billing_candidates(&all, &settings.billing_account_ids);
    let usable: Vec<_> = candidates
        .into_iter()
        .filter(|a| a.enabled && !ban.iter().any(|b| *b == a.id))
        .collect();
    if usable.is_empty() {
        return None;
    }

    // 1) 会话粘滞
    if let Some(conv) = conv {
        if let Some(id) = sticky_hit(conv) {
            if let Some(a) = usable.iter().find(|a| a.id == id) {
                return Some(a.clone());
            }
        }
    }

    // 2) 跳过冷却，取第一个
    let picked = usable.iter().find(|a| !cooling(&a.id)).unwrap_or(&usable[0]);
    Some(picked.clone())
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
        if !settings.gateway_enabled {
            set_status(GatewayStatus::default());
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let port = settings.gateway_port;
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                set_status(GatewayStatus {
                    active: true,
                    port,
                    error: None,
                });
                eprintln!("[gateway] 监听 127.0.0.1:{port}");
                let _ = listener.set_nonblocking(true);
                loop {
                    let cur = accounts::load_settings(&dir);
                    if !cur.gateway_enabled || cur.gateway_port != port {
                        break;
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
                set_status(GatewayStatus::default());
            }
            Err(e) => {
                set_status(GatewayStatus {
                    active: false,
                    port,
                    error: Some(format!("端口 {port} 无法监听：{e}")),
                });
                eprintln!("[gateway] 无法监听 127.0.0.1:{port}：{e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

fn handle_conn(mut stream: TcpStream, app: tauri::AppHandle) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(600)));

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
        respond(&mut stream, 400, "text/plain", b"bad request", &[]);
        return;
    };
    let dir = match commands::try_data_dir(&app) {
        Ok(d) => d,
        Err(_) => {
            respond(&mut stream, 500, "text/plain", b"internal error", &[]);
            return;
        }
    };

    // 仅接管 chat 类路径；其余（models 列举等）返回简单 OK，避免空响应用户困惑
    if !is_chat_path(&req.target) {
        let body = r#"{"object":"list","data":[{"id":"trae-pool","object":"model"}]}"#;
        respond(&mut stream, 200, "application/json", body.as_bytes(), &[( "access-control-allow-origin", "*")]);
        return;
    }

    let body_bytes = req_body(&buf, &req).to_vec();
    let conv = header_value(&req, "x-conversation-id")
        .or_else(|| header_value(&req, "x-chat-id"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut ban: Vec<String> = Vec::new();
    let mut route_started = false;
    loop {
        let account = match tauri::async_runtime::block_on(choose_account(&dir, conv.as_deref(), &ban)) {
            Some(a) => a,
            None => {
                respond(&mut stream, 503, "text/plain", b"no account available for gateway", &[]);
                return;
            }
        };

        // 转发地址：账号 host + OpenAI 兼容路径
        let host = account.host.clone().unwrap_or_else(|| "https://api.trae.cn".into());
        let url = format!("{}{}", host.trim_end_matches('/'), "/v1/chat/completions");

        use reqwest::Method;
        let upstream = tauri::async_runtime::block_on(async {
            let mut r = CLIENT
                .request(Method::from_bytes(req.method.as_bytes()).unwrap_or(Method::POST), &url)
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
            Ok(resp) if resp.status() == 429 => {
                if let Ok(mut m) = cooldown_table().lock() {
                    m.insert(account.id.clone(), Instant::now());
                }
                if let Some(c) = &conv {
                    sticky_put(c, String::new(), true);
                }
                if !route_started {
                    eprintln!(
                        "[gateway] 账号「{}」触发限流(429)",
                        account.name
                    );
                    route_started = true;
                }
                if ban.len() + 1 < FAILOVER_MAX_TRIES {
                    ban.push(account.id.clone());
                    continue;
                }
                stream_response(&mut stream, resp);
                return;
            }
            Ok(resp) => {
                stream_response(&mut stream, resp);
                return;
            }
            Err(e) => {
                respond(&mut stream, 502, "text/plain", format!("upstream error: {e}").as_bytes(), &[]);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 响应写出
// ---------------------------------------------------------------------------

fn write_head(stream: &mut TcpStream, status: u16, ctype: &str, headers: &[(&str, &str)]) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nTransfer-Encoding: chunked\r\nCache-Control: no-cache\r\nConnection: close\r\n",
        reason(status)
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

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

/// 边收边转：上游出一个 chunk 就往下游写一个。
fn stream_response(stream: &mut TcpStream, mut resp: reqwest::Response) {
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let headers: Vec<(&str, &str)> = Vec::new();
    if write_head(stream, status, &ctype, &headers).is_err() {
        return;
    }
    tauri::async_runtime::block_on(async {
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    if write_chunk(stream, &c).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
    let _ = write_end(stream);
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
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

/// 由 `spawn` 驱动的对外入口：`gateway::spawn(app.handle().clone())`
pub fn spawn_gateway(app: tauri::AppHandle) {
    spawn(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nX-Conversation-Id: c1\r\n\r\n{}";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/v1/chat/completions");
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
    fn chat_path_detection() {
        assert!(is_chat_path("/v1/chat/completions"));
        assert!(is_chat_path("http://127.0.0.1:8788/v1/chat/completions"));
        assert!(is_chat_path("/v1/messages"));
        assert!(!is_chat_path("/v1/models"));
    }

    #[test]
    fn billing_candidates_restricts() {
        let mk = |id: &str| accounts::Account {
            id: id.into(), name: id.into(), phone: None, region: None, user_id: None,
            token: "t".into(), refresh_token: None, host: None, expires_at: None,
            refresh_expires_at: None, device_id: None, machine_id: None,
            created_at: String::new(), enabled: true,
        };
        let all = vec![mk("a"), mk("b")];
        assert_eq!(billing_candidates(&all, &[]).len(), 2);
        assert_eq!(billing_candidates(&all, &["b".into()]).len(), 1);
        assert_eq!(billing_candidates(&all, &["zz".into()]).len(), 2, "勾选失效退回全部");
    }
}
