#![recursion_limit = "256"]

mod accounts;
mod checkin;
mod commands;
mod gateway;
mod inject;
mod logs;
mod notify;
mod oauth;
mod refresh;
mod scheduler;
mod trae_auth;
mod tray;

use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 定时签到 + 池化网关：独立后台线程，与进程同生命周期
            scheduler::spawn(app.handle().clone());
            gateway::spawn_gateway(app.handle().clone());
            // 启动自愈：若上次已开启「自动注入」且条目缺失，在 TraeWork 未运行时补注
            let heal_app = app.handle().clone();
            std::thread::spawn(move || {
                for _ in 0..12 {
                    if let Ok(d) = commands::try_data_dir(&heal_app) {
                        let s = accounts::load_settings(&d);
                        if s.injection_enabled && !crate::inject::is_trae_running() {
                            let _ = crate::inject::self_heal(true, s.gateway_port);
                        }
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
            commands::import_from_file,
            commands::add_manual_account,
            commands::discover_local,
            commands::toggle_account,
            commands::checkin_one,
            commands::checkin_all,
            commands::checkin_status,
            commands::get_logs,
            commands::clear_logs,
            commands::get_settings,
            commands::save_settings,
            commands::gateway_status,
            commands::oauth_start,
            commands::oauth_poll,
            commands::open_external,
            commands::inject_model,
            commands::injection_status,
            commands::revert_injection,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    app.run(|_handle, event| match &event {
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => tray::show_main(_handle),
        _ => {}
    });
}
