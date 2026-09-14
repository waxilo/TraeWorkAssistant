//! 通知/告警：当前实现为把事件落到日志（避免阻塞式系统弹窗干扰常驻进程）；
//! 预留 webhook 扩展点。

use std::time::Duration;

#[derive(Clone)]
pub struct Notifier {}

impl Notifier {
    pub fn new() -> Self {
        Self {}
    }
}

// ---------------------------------------------------------------------------
// Webhook 通知：GET {webhook}?message=<url-encoded>
// 必须带浏览器 UA（Cloudflare 前置会对脚本类 UA 返回 403）；失败不影响签到，
// 最多重试 3 次（1.5s / 3s 退避）。
// ---------------------------------------------------------------------------

const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                          AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_ATTEMPTS: u32 = 3;

fn excerpt(s: &str) -> String {
    let t = s.trim();
    let cut: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 把消息拼进 webhook 的 query（`?message=…`），负责百分号编码。
pub fn build_url(webhook: &str, message: &str) -> Result<String, String> {
    let raw = webhook.trim();
    if raw.is_empty() {
        return Err("未配置 webhook 地址".into());
    }
    let mut url = reqwest::Url::parse(raw).map_err(|e| format!("webhook 地址无效：{e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("webhook 仅支持 http(s) 地址".into());
    }
    url.query_pairs_mut().append_pair("message", message);
    Ok(url.to_string())
}

/// 发送一条通知。返回可读的成功描述（含响应体片段）或最终失败原因。
pub async fn send(webhook: &str, message: &str) -> Result<String, String> {
    let url = build_url(webhook, message)?;
    let client = reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))?;

    let mut last_err = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let b = excerpt(&body);
                    return Ok(if b.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        format!("HTTP {} {}", status.as_u16(), b)
                    });
                }
                last_err = format!("HTTP {} {}", status.as_u16(), excerpt(&body));
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(1500 * attempt as u64)).await;
        }
    }
    Err(format!("已重试 {MAX_ATTEMPTS} 次仍失败：{last_err}"))
}

/// 把一次定时签到的计数汇总成一条通知正文。
pub fn summary_message(ok: usize, already: usize, failed: &[String]) -> String {
    let total = ok + already + failed.len();
    let mut s = format!(
        "TraeWork 签到完成：成功 {ok} / 已签 {already} / 失败 {}（共 {total} 个账号）",
        failed.len()
    );
    if !failed.is_empty() {
        s.push_str("\n失败明细：");
        for f in failed.iter().take(5) {
            s.push_str(&format!("\n· {f}"));
        }
        if failed.len() > 5 {
            s.push_str(&format!("\n…另有 {} 个失败账号", failed.len() - 5));
        }
    }
    s
}
