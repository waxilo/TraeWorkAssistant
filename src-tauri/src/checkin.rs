//! TraeWork 签到：查询状态 + 领取。
//!
//! 端点（从官方客户端解包与论坛技能包交叉验证，路径可能随版本调整，用数组兼容多版本）：
//! - 状态：`POST /trae/api/v2/ug/checkin_credits/status`
//! - 领取：`POST /trae/api/v2/ug/checkin_credits/claim`
//!
//! 鉴权头：`Authorization: Cloud-IDE-JWT <token>`（不是 Bearer）。
//!
//! host 由账号数据里的 `host` 决定（如 `https://api.trae.cn`）。

use crate::accounts::Account;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

const STATUS_PATHS: &[&str] = &[
    "/trae/api/v2/ug/checkin_credits/status",
    "/ug/checkin_credits/status",
];
const CLAIM_PATHS: &[&str] = &["/trae/api/v2/ug/checkin_credits/claim"];

static ALREADY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("已签到|已领取|already\\s*(?:checked[- ]?in|claimed)|daily.*already").unwrap()
});
static INACTIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("未开启|未开始|未开放|无.*活动|活动.*(?:结束|关闭|暂停)").unwrap()
});

/// 签到结果
#[derive(serde::Serialize, Clone, Debug)]
pub struct CheckinResult {
    pub success: bool,
    pub already: bool,
    pub inactive: bool,
    pub message: String,
    /// 本次签到获得积分
    pub credit: Option<i64>,
    pub host: Option<String>,
    pub at: String,
}

fn host_of(account: &Account) -> String {
    account
        .host
        .clone()
        .unwrap_or_else(|| "https://api.trae.cn".into())
}

fn normalize_host(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{}", u)
    }
}

/// 设备/机器标识：优先用账号存的，否则用 user_id，再否则用 id 派生稳定值
/// （保证同一账号每次请求一致；Trae 签到接口把设备头当必填项，缺则 9004 参数错误）。
fn device_ids(account: &Account) -> (String, String) {
    let dev = account
        .device_id
        .clone()
        .or_else(|| account.user_id.clone())
        .unwrap_or_else(|| account.id.clone());
    let mach = account
        .machine_id
        .clone()
        .or_else(|| account.user_id.clone())
        .unwrap_or_else(|| account.id.clone());
    (dev, mach)
}

/// 请求头：鉴权用 `Cloud-IDE-JWT`，并带齐签到接口必填头。
///
/// - `X-Device-Id` / `X-Machine-Id`：设备标识，缺省会导致 claim/status 报 **9004 参数错误**
///   （即 "The submitted order parameters are incorrect"）；
/// - `X-User-Region`：区域，缺失也会被部分端点判为参数错误；
/// - `X-User-Id`：用户标识（社区实践表明补充后更稳）。
fn headers(account: &Account) -> (String, Vec<(String, String)>) {
    let (dev, mach) = device_ids(account);
    let mut hdrs: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json".into()),
        ("X-Device-Id".into(), dev),
        ("X-Machine-Id".into(), mach),
    ];
    if let Some(r) = &account.region {
        hdrs.push(("X-User-Region".into(), r.clone()));
    }
    if let Some(u) = &account.user_id {
        if !u.is_empty() {
            hdrs.push(("X-User-Id".into(), u.clone()));
        }
    }
    (
        format!("Cloud-IDE-JWT {}", account.token),
        hdrs,
    )
}

/// 是否可重试的瞬时失败（服务器繁忙/限流，社区已知 code 9074）。
fn is_transient_busy(code: Option<i64>, msg: &str) -> bool {
    if code == Some(9074) {
        return true;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("9074")
        || m.contains("繁忙")
        || m.contains("busy")
        || m.contains("too many")
        || m.contains("rate limit")
        || m.contains("participants")
}

/// 从 status 响应取值：`checked_in`（今日是否已签）、`enable`、`credits`
fn status_fields(v: &Value) -> (bool, bool, Option<i64>) {
    let checked_in = v
        .get("checked_in")
        .or_else(|| v.get("did_checked_in"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let enable = v.get("enable").and_then(Value::as_bool).unwrap_or(true);
    let credits = parse_credit(v);
    (checked_in, enable, credits)
}

/// 从 status/claim 响应里取积分：先看 `data`，再看顶层 `credits`。
/// 会员态日常会把 `extra_credits`（加量）合并进展示值。
fn parse_credit(body: &Value) -> Option<i64> {
    let keys = ["credit", "credits", "gain_credit", "today_credit"];
    for r in [body, body.get("data").unwrap_or(&Value::Null)] {
        for key in keys {
            if let Some(v) = r.get(key) {
                if let Some(n) = v.as_i64() {
                    return Some(n);
                }
                if let Some(s) = v.as_str() {
                    if let Ok(n) = s.trim().parse::<i64>() {
                        return Some(n);
                    }
                }
            }
        }
    }
    // 合并加量积分：credits + extra_credits（会员每日加量）
    let base = body.get("credits").and_then(Value::as_i64);
    let extra = body.get("extra_credits").and_then(Value::as_i64);
    base.zip(extra).map(|(b, e)| b + e).or(base)
}

fn classify(http_ok: bool, code: Option<i64>, msg: &str) -> (bool, bool, bool) {
    let inactive = INACTIVE_RE.is_match(msg);
    let already = code == Some(0) && ALREADY_RE.is_match(msg) && !inactive;
    let ok = !inactive && ((code == Some(0) && http_ok) || already);
    (ok, already, inactive)
}

/// 对一个账号执行签到，遍历候选路径，命中明确结果即返回。
pub async fn do_checkin(account: &Account) -> CheckinResult {
    let client = reqwest::Client::new();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let host = normalize_host(&host_of(account));
    let mut last: Option<String> = None;

    let (auth_hdr, hdrs) = headers(account);

    // 优先查状态：今日已签 → 幂等成功；未开启 → 非活动；否则才去领取
    let status_url = format!("{}{}", host, STATUS_PATHS[0]);
    let mut status_req = client
        .post(&status_url)
        .header("Authorization", &auth_hdr)
        .body("{}")
        .timeout(Duration::from_secs(15));
    for (k, v) in &hdrs {
        status_req = status_req.header(k, v);
    }
    let status = status_req.send().await.ok();
    if let Some(st) = status {
        if let Ok(v) = st.json::<Value>().await {
            let (checked_in, enable, credits) = status_fields(&v);
            let code = v.get("code").and_then(Value::as_i64);
            if code == Some(0) {
                if checked_in {
                    return CheckinResult {
                        success: true,
                        already: true,
                        inactive: false,
                        message: "今日已签到".into(),
                        credit: credits,
                        host: Some(host.clone()),
                        at: now,
                    };
                }
                if !enable {
                    return CheckinResult {
                        success: false,
                        already: false,
                        inactive: true,
                        message: "签到活动未开启".into(),
                        credit: None,
                        host: Some(host.clone()),
                        at: now,
                    };
                }
            }
        }
    }

    // 未命中明确状态，直接尝试领取（每个候选路径做有限重试，应对 9074 限流）
    const MAX_CLAIM_ATTEMPTS: usize = 3;
    for path in CLAIM_PATHS {
        let url = format!("{}{}", host, path);
        let mut attempt = 0usize;
        let mut path_last: Option<String> = None;
        loop {
            attempt += 1;
            let mut req = client
                .post(&url)
                .header("Authorization", &auth_hdr)
                .body("{}")
                .timeout(Duration::from_secs(20));
            for (k, v) in &hdrs {
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(r) => {
                    let status = r.status();
                    let http_ok = status.is_success();
                    let text = r.text().await.unwrap_or_default();
                    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let code = body.get("code").and_then(Value::as_i64);
                    let mut msg = body
                        .get("msg")
                        .or_else(|| body.get("message"))
                        .or_else(|| body.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    // 诊断兜底：把 HTTP 状态码 + 业务 code + 原始响应体带进消息，避免下次再盲猜
                    let detail = if text.trim().is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        format!(
                            "HTTP {} code={:?} {}",
                            status.as_u16(),
                            code,
                            text.trim().chars().take(160).collect::<String>()
                        )
                    };
                    if msg.is_empty() {
                        msg = detail.clone();
                    }
                    let (ok, already, inactive) = classify(http_ok, code, &msg);
                    if ok || already || inactive || code == Some(0) {
                        return CheckinResult {
                            success: ok || code == Some(0),
                            already,
                            inactive,
                            message: msg,
                            credit: parse_credit(&body),
                            host: Some(host.clone()),
                            at: now,
                        };
                    }
                    // 限流/繁忙：退避后重试（同路径），其余失败直接换下一路径
                    if is_transient_busy(code, &msg) && attempt < MAX_CLAIM_ATTEMPTS {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    path_last = Some(format!("[HTTP {} code={:?}] {}", status.as_u16(), code, msg));
                    break;
                }
                Err(e) => {
                    path_last = Some(format!("{} 请求失败：{}", url, e));
                    break;
                }
            }
        }
        last = path_last;
    }

    CheckinResult {
        success: false,
        already: false,
        inactive: false,
        message: last.unwrap_or_else(|| "所有候选路径均失败".into()),
        credit: None,
        host: None,
        at: now,
    }
}

/// 查询签到状态：返回本体（含 `checked_in`/`credits` 等顶层字段），前端可直接渲染。
/// 失败返回 None。best-effort。
pub async fn query_status(account: &Account) -> Option<Value> {
    let client = reqwest::Client::new();
    let host = normalize_host(&host_of(account));
    let (auth_hdr, hdrs) = headers(account);
    let url = format!("{}{}", host, STATUS_PATHS[0]);
    let mut req = client
        .post(&url)
        .header("Authorization", &auth_hdr)
        .body("{}")
        .timeout(Duration::from_secs(15));
    for (k, v) in &hdrs {
        req = req.header(k, v);
    }
    if let Ok(resp) = req.send().await {
        if let Ok(v) = resp.json::<Value>().await {
            if v.get("code").and_then(Value::as_i64) == Some(0) {
                return Some(v);
            }
        }
    }
    None
}

/// 查询签到状态的结构化视图（给定时/汇总用）。
pub struct CheckinStatus {
    pub checked_in: bool,
    pub enable: bool,
    pub credits: Option<i64>,
    pub message: String,
}

impl CheckinStatus {
    pub fn from_status(v: Option<Value>) -> Option<CheckinStatus> {
        let v = v?;
        let (checked_in, enable, credits) = status_fields(&v);
        let message = v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("success")
            .to_string();
        Some(CheckinStatus {
            checked_in,
            enable,
            credits,
            message,
        })
    }
}

/// 从 JWT 的 iss/host 推断 API host（备用；账号数据里一般已有 host）
pub fn issuer_host(token: &str) -> Option<String> {
    let part = token.split('.').nth(1)?;
    let mut padded = part.replace('-', "+").replace('_', "/");
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = STANDARD.decode(padded).ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    let iss = payload.get("iss")?.as_str()?;
    if iss.contains("api.trae.cn") {
        Some("https://api.trae.cn".into())
    } else if iss.contains("api.trae.ai") {
        Some("https://api.trae.ai".into())
    } else {
        Some(iss.trim_end_matches('/').to_string())
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;
    use crate::accounts::Account;
    use crate::trae_auth;

    fn to_account(a: &trae_auth::TraeLocalAccount) -> Account {
        Account {
            id: "t".into(),
            name: a.nickname.clone().unwrap_or_default(),
            phone: a.phone.clone(),
            region: a.region.clone(),
            user_id: a.user_id.clone(),
            token: a.token.clone(),
            refresh_token: a.refresh_token.clone(),
            host: a.host.clone(),
            expires_at: a.expires_at,
            refresh_expires_at: a.refresh_expires_at,
            device_id: a.device_id.clone(),
            machine_id: a.machine_id.clone(),
            created_at: String::new(),
            enabled: true,
        }
    }

    #[test]
    #[ignore]
    fn live_status_and_checkin() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let list = trae_auth::discover_local_accounts();
            println!("accounts={}", list.len());
            for a in list {
                let acc = to_account(&a);
                let st = CheckinStatus::from_status(query_status(&acc).await);
                println!("STATUS checked_in={:?} enable={:?} credits={:?} msg={:?}",
                    st.as_ref().map(|s| s.checked_in),
                    st.as_ref().map(|s| s.enable),
                    st.as_ref().and_then(|s| s.credits),
                    st.as_ref().map(|s| s.message.clone()));
                let r = do_checkin(&acc).await;
                println!("CHECKIN success={} already={} inactive={} credit={:?} msg={:?}",
                    r.success, r.already, r.inactive, r.credit, r.message);
            }
        });
    }
}
