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

/// 请求头：鉴权用 `Cloud-IDE-JWT`，并带上 `X-User-Region`（缺省会导致 claim 报 9004）
fn headers(account: &Account) -> (String, Vec<(String, String)>) {
    let mut hdrs: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json".into()),
    ];
    if let Some(r) = &account.region {
        hdrs.push(("X-User-Region".into(), r.clone()));
    }
    (
        format!("Cloud-IDE-JWT {}", account.token),
        hdrs,
    )
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

    // 未命中明确状态，直接尝试领取
    for path in CLAIM_PATHS {
        let url = format!("{}{}", host, path);
        let mut req = client
            .post(&url)
            .header("Authorization", &auth_hdr)
            .body("{}")
            .timeout(Duration::from_secs(20));
        for (k, v) in &hdrs {
            req = req.header(k, v);
        }
        let resp = req.send().await;
        match resp {
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
                if msg.is_empty() {
                    msg = if text.trim().is_empty() {
                        format!("返回体为空（HTTP {})", status.as_u16())
                    } else {
                        format!("HTTP {} ({})", status.as_u16(), text.trim().chars().take(120).collect::<String>())
                    };
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
                last = Some(msg);
            }
            Err(e) => {
                last = Some(format!("{} 请求失败：{}", url, e));
            }
        }
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
