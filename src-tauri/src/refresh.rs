//! Access token 续签：用 refresh token 换新 access token（best-effort 旁路）。
//!
//! TraeWork 的 OAuthenticator 走标准 OAuth refresh（refresh_token 授权）。精确刷新端点
//! 随版本可能变化，这里对几个已知模式做候选遍历，命中即返回；全部失败则保留旧 token，
//! 绝不影响签到主流程。

use crate::accounts::Account;
use serde_json::Value;
use std::time::Duration;

const REFRESH_PATHS: &[&str] = &[
    "/trae/api/v2/user/refresh_token",
    "/trae/api/v1/user/refresh_token",
    "/api/v2/user/refresh_token",
    "/api/v1/auth/refresh",
];

fn normalize_host(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{}", u)
    }
}

/// 对一个账号尝试续签。成功返回更新后的字段并落盘；失败返回 Err（不影响签到）。
pub async fn refresh_account(dir: &std::path::Path, account: &mut Account) -> Result<(), String> {
    let Some(rt) = account.refresh_token.clone() else {
        return Err("该账号没有 refresh token".into());
    };
    let host = normalize_host(
        &account
            .host
            .clone()
            .unwrap_or_else(|| "https://api.trae.cn".into()),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_err = "所有刷新路径均失败".to_string();
    for path in REFRESH_PATHS {
        let url = format!("{}{}", host, path);
        let body = serde_json::json!({ "refreshToken": rt });
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) => {
                let text = r.text().await.unwrap_or_default();
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let code = v.get("code").and_then(Value::as_i64);
                if code != Some(0) {
                    last_err = v
                        .get("msg")
                        .or_else(|| v.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or(format!("code={code:?}"));
                    continue;
                }
                let data = v.get("data").cloned().unwrap_or(Value::Null);
                let new_token = data
                    .get("token")
                    .or_else(|| data.get("accessToken"))
                    .or_else(|| data.get("access_token"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| "刷新响应缺少 token".to_string())?;
                account.token = new_token;
                if let Some(nrt) = data
                    .get("refreshToken")
                    .or_else(|| data.get("refresh_token"))
                    .and_then(Value::as_str)
                {
                    account.refresh_token = Some(nrt.to_string());
                }
                if let Some(exp) = data
                    .get("expiredAt")
                    .or_else(|| data.get("expiresAt"))
                    .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
                {
                    account.expires_at = Some(exp);
                }
                // 落盘
                let all = crate::accounts::load_accounts(dir);
                let mut merged = all.clone();
                if let Some(a) = merged.iter_mut().find(|a| a.id == account.id) {
                    *a = account.clone();
                }
                crate::accounts::save_accounts(dir, &merged)?;
                return Ok(());
            }
            Err(e) => {
                last_err = format!("{} 请求失败：{}", url, e);
            }
        }
    }
    Err(last_err)
}
