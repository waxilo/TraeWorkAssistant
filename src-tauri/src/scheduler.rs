//! 定时签到线程：按设置里的时刻（HH:MM）每天触发一次全账号签到。
//! 只在应用运行期间生效（与进程同生命周期）。
//!
//! ## 为什么要「多轮补签」
//!
//! TraeWork 的领取接口（`/trae/api/v2/ug/checkin_credits/claim`）会返回
//! **9074「当前参与用户太多，请稍后再试」** —— 这是服务端对领取接口的限流，
//! 与请求参数无关，过一段时间才放行（真机实测：状态查询一直正常，
//! 领取可连续十几分钟返回 9074）。只试一次的定时任务会直接失败，
//! 所以这里在单账号内部退避重试之外，再加**整体补签轮次**。

use crate::accounts::{self, Account};
use crate::commands;
use crate::checkin::CheckinResult;
use crate::logs;
use std::time::Duration;

/// 最多 3 轮（首轮 + 2 轮补签）。
const MAX_ROUNDS: usize = 3;
/// 轮与轮之间的间隔。
const ROUND_GAP_SECS: u64 = 600;

pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_day: Option<chrono::NaiveDate> = None;
        loop {
            let dir = match commands::try_data_dir(&app) {
                Ok(d) => d,
                Err(_) => {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let settings = accounts::load_settings(&dir);
            let now = chrono::Local::now();
            if settings.checkin_enabled {
                let today = now.date_naive();
                if last_day != Some(today) {
                    let cur = now.format("%H:%M").to_string();
                    if cur == settings.checkin_time {
                        last_day = Some(today);
                        run_checkin(&app, &dir);
                    }
                }
            } else {
                last_day = Some(now.date_naive());
            }
            std::thread::sleep(Duration::from_secs(20));
        }
    });
}

fn run_checkin(app: &tauri::AppHandle, dir: &std::path::Path) {
    let settings = accounts::load_settings(dir);
    // 账号列表里已无「启用」概念：所有账号一律参与定时签到。
    let mut pending: Vec<Account> = accounts::load_accounts(dir);
    if pending.is_empty() {
        logs::push("系统", true, "暂无账号，跳过定时签到");
        return;
    }

    let mut done: Vec<(String, CheckinResult)> = Vec::new();
    for round in 0..MAX_ROUNDS {
        if pending.is_empty() {
            break;
        }
        if round > 0 {
            logs::push(
                "系统",
                true,
                format!(
                    "第 {} 轮补签：{} 个账号上一轮被限流（9074），{} 分钟后重试",
                    round + 1,
                    pending.len(),
                    ROUND_GAP_SECS / 60
                ),
            );
            std::thread::sleep(Duration::from_secs(ROUND_GAP_SECS));
        }

        let mut next: Vec<Account> = Vec::new();
        for mut account in pending {
            let name = account.name.clone();
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let _ = tauri::async_runtime::block_on(crate::refresh::refresh_account(dir, &mut account));
            let r = tauri::async_runtime::block_on(crate::checkin::do_checkin(&account));
            logs::push(
                &name,
                r.success,
                format!("[{}] 第 {} 轮 · {}", now, round + 1, r.message),
            );
            // 仅「服务端限流」值得下一轮再试；鉴权失败/业务错误重试无意义
            if r.transient && round + 1 < MAX_ROUNDS {
                next.push(account);
            } else {
                done.push((name, r));
            }
        }
        pending = next;
    }

    // 兜底：极端情况下仍留在 pending 的账号（不应发生）计入失败
    for account in pending {
        done.push((
            account.name.clone(),
            CheckinResult {
                success: false,
                already: false,
                inactive: false,
                transient: true,
                auth_failed: false,
                message: "限流未放行".into(),
                credit: None,
                host: None,
                at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            },
        ));
    }

    let mut ok = 0usize;
    let mut already = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for (name, r) in &done {
        if r.already {
            already += 1;
        } else if r.success {
            ok += 1;
        } else if !r.inactive {
            failed.push(format!("{}：{}", name, r.message));
        }
    }

    // webhook 通知：仅配置了地址才发，失败只记日志、不影响签到结果
    let webhook = settings.webhook_url.trim();
    if !webhook.is_empty() {
        let title = crate::notify::summary_title(ok, already, failed.len());
        let msg = crate::notify::summary_message(ok, already, &failed);
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let out = match tauri::async_runtime::block_on(crate::notify::send(webhook, &title, &msg)) {
            Ok(r) => r,
            Err(e) => format!("发送失败：{e}"),
        };
        logs::push("系统", true, format!("[{}] Webhook 通知：{}", now, out));
    }
    let _ = app;
}
