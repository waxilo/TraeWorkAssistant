//! 定时签到线程：按设置里的时刻（HH:MM）每天触发一次全账号签到。
//! 只在应用运行期间生效（与进程同生命周期）。

use crate::accounts;
use crate::commands;
use crate::logs;
use std::time::Duration;

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
    let list = accounts::load_accounts(dir);
    if list.is_empty() {
        logs::push("系统", true, "未发现账号，跳过定时签到");
        return;
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut ok = 0usize;
    let mut already = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for mut account in list {
        let name = account.name.clone();
        let _ = tauri::async_runtime::block_on(crate::refresh::refresh_account(dir, &mut account));
        let r = tauri::async_runtime::block_on(crate::checkin::do_checkin(&account));
        logs::push(&name, r.success, format!("[{}] {}", now, r.message));
        if r.success && !r.already {
            ok += 1;
        } else if r.already {
            already += 1;
        } else if !r.inactive {
            failed.push(format!("{}：{}", name, r.message));
        }
    }
    // webhook 通知：仅配置了地址才发，失败只记日志、不影响签到结果
    let webhook = settings.webhook_url.trim();
    if !webhook.is_empty() {
        let msg = crate::notify::summary_message(ok, already, &failed);
        let out = match tauri::async_runtime::block_on(crate::notify::send(webhook, &msg)) {
            Ok(r) => r,
            Err(e) => format!("发送失败：{e}"),
        };
        logs::push("系统", true, format!("[{}] Webhook 通知：{}", now, out));
    }
    let _ = app;
}
