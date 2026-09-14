#![recursion_limit = "256"]

mod accounts;
mod checkin;
mod commands;
mod endpoint;
mod journal;
mod logs;
mod notify;
mod oauth;
mod proxy;
mod refresh;
mod scheduler;
mod trae_auth;
mod tray;

use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Tauri updater 的 reqwest 默认走系统代理；本机代理（如 Clash）对 GitHub release-assets
    // CDN 不稳定（HTTP 000 / 502），会导致 latest.json 都拉不下来。
    // 启动时把 GitHub 相关域名加入 NO_PROXY，让更新器直连 GitHub。
    const GITHUB_NO_PROXY: &str = "github.com,.github.com,githubusercontent.com,.githubusercontent.com";
    match std::env::var("NO_PROXY") {
        Ok(v) if !v.is_empty() => {
            std::env::set_var("NO_PROXY", format!("{}, {}", v, GITHUB_NO_PROXY));
        }
        _ => {
            std::env::set_var("NO_PROXY", GITHUB_NO_PROXY);
        }
    }

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 定时签到 + 智能接管反代：独立后台线程，与进程同生命周期
            scheduler::spawn(app.handle().clone());
            proxy::spawn_proxy(app.handle().clone());
            // 启动清扫：等反代有机会绑定端口后，判断是否需要恢复 TraeWork 端点配置。
            // 只保留「接管开启且反代确实在监听」这一种情形，其余一律恢复官方直连。
            let heal_app = app.handle().clone();
            std::thread::spawn(move || {
                for _ in 0..12 {
                    if let Ok(d) = commands::try_data_dir(&heal_app) {
                        let s = accounts::load_settings(&d);
                        if s.takeover_enabled {
                            for _ in 0..30 {
                                if proxy::status().active {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                        }
                        let keep = s.takeover_enabled && proxy::status().active;
                        let _ = endpoint::sweep(&d, keep);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            });
            let _notify = notify::Notifier::new();
            // 系统托盘：后台常驻入口
            tray::setup(app.handle()).expect("初始化系统托盘失败");
            Ok(())
        })
        // 关闭窗口 = 隐藏到托盘，进程常驻
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_accounts,
            commands::import_accounts,
            commands::remove_account,
            commands::discover_local,
            commands::checkin_one,
            commands::checkin_all,
            commands::checkin_status,
            commands::get_logs,
            commands::clear_logs,
            commands::get_settings,
            commands::save_settings,
            commands::oauth_start,
            commands::oauth_poll,
            commands::open_external,
            commands::takeover_status,
            commands::takeover_enable,
            commands::takeover_disable,
            commands::takeover_events,
            commands::clear_takeover_events,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(|_handle, event| match &event {
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => tray::show_main(_handle),
        _ => {}
    });
}
