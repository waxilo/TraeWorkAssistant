//! 定时签到线程：按设置里的时刻（HH:MM）每天触发一次全账号签到。
//! 只在应用运行期间生效（与进程同生命周期）。
//!
//! 顺带兼顾一件小事：**端点自愈** —— TraeWork 升级会整份替换 `product.json`，
//! 智能接管的端点改写会随之丢失，这里每 5 分钟确认一次并补写（见 `endpoint::repair`）。
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
/// 端点自愈的常规检查间隔：TraeWork 升级会整份替换 `product.json`，改写随之丢失。
const REPAIR_OK_GAP_SECS: u64 = 300;
/// 还没就绪时的重试间隔（反代可能刚起来、文件刚被覆盖）——短一点，尽快自愈。
const REPAIR_RETRY_GAP_SECS: u64 = 20;

pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_day: Option<chrono::NaiveDate> = None;
        // 启动即检查一次端点改写是否还在
        let mut next_repair = std::time::Instant::now();
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
            if settings.takeover_enabled && std::time::Instant::now() >= next_repair {
                // ⚠️ 端点必须与 `commands::enable_endpoint` 用**同一个**来源
                // （`endpoint::base_url`）。端点只有一种形态了，所以这里不再随模式二选一 ——
                // 但「两端共用同一个函数」这条纪律要保住：曾经这里写死过一个值，与安装侧
                // 不一致，于是自愈每 20s 都被 `endpoint::preflight` 以「会让 TraeWork
                // 启动即崩」为由拒绝，journal 里刷屏 `install_blocked`，而 TraeWork 升级
                // 覆盖 `product.json` 后**再也不会有改道**（接管静默失效）。
                let endpoint_base = crate::endpoint::base_url(settings.takeover_port);
                let all = crate::target::discover();
                let selected = crate::target::select(&settings.takeover_apps);
                let proxy_ok = crate::proxy::status().active;

                // ① 名单之外的应用整个放下（端点 + 补丁）。这条同时兜住两件事：
                //    用户在界面里取消勾选（那条路会立即处理，这里是幂等的第二道）、
                //    以及有人直接手改了 `settings.json`。
                crate::endpoint::sweep(&dir, &all, &selected);

                // ② 反代不在监听时，指向本机的端点**必须先还回官方**（补丁不动）。
                //    两条生命周期的分工见 `endpoint::restore_endpoints` 的文档 ——
                //    简单说：补丁跟着「名单」走、端点跟着「连得上」走，否则会有还补丁/重打补丁的抖动。
                if !proxy_ok {
                    crate::endpoint::restore_endpoints(&dir, &all);
                }

                // ③ 名单里的每个应用：补丁必须在位，端点必须指向本机。
                let mut ready = !selected.is_empty();
                for t in &selected {
                    // **持续前提**：该应用的 `out/main.js` 必须是打过补丁的。
                    // 升级会把它整份换掉、补丁随之消失，而端点还写着明文 `http://` ——
                    // **它下一次启动就会崩**。所以每轮都确认一遍，丢了就补回来。
                    let gate_ok = match crate::patch::apply(&dir, t) {
                        Ok(p) => p.patched,
                        Err(e) => {
                            crate::journal::append_dedup(
                                &dir,
                                "patch_gone",
                                &format!(
                                    "免证书模式：「{}」的闸门补丁无法保证（{e}）。\
                                     已放弃对它做端点改写并恢复官方直连 —— \
                                     否则它下次启动会因明文端点崩掉",
                                    t.id
                                ),
                            );
                            false
                        }
                    };

                    let ok = if gate_ok {
                        // 反代没监听时绝不改写端点，否则会把应用指向死端口
                        proxy_ok && crate::endpoint::repair(t, &dir, &endpoint_base)
                    } else {
                        // fail-safe：补丁没保证就**绝不**把明文端点留在它的 product.json 里。
                        // `uninstall` 幂等，不是我们改的就不动。
                        let _ = crate::endpoint::uninstall(t, &dir);
                        false
                    };
                    ready &= ok;
                }
                next_repair = std::time::Instant::now()
                    + Duration::from_secs(if ready {
                        REPAIR_OK_GAP_SECS
                    } else {
                        REPAIR_RETRY_GAP_SECS
                    });
            }
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
            let _ = tauri::async_runtime::block_on(crate::renew::renew_if_needed(dir, &mut account));
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
