#![recursion_limit = "256"]

mod accounts;
mod checkin;
mod commands;
mod devicekey;
mod endpoint;
mod journal;
mod logs;
mod notify;
mod oauth;
mod patch;
mod portcheck;
mod profile;
mod proxy;
mod renew;
mod rules;
mod scheduler;
mod token;
mod trae_auth;
mod traework;
mod tray;

use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // ⚠️ 这里**故意什么都不做**：曾经在这里注入
    // `NO_PROXY=github.com,.github.com,githubusercontent.com,…` 让更新器「绕开代理直连 GitHub」，
    // 已于 2026-09-15 删除。旧注释的前提（「本机代理对 GitHub CDN 不稳」）不但过时，而且反了：
    //
    // 1. updater 插件用的是 reqwest 0.13，它的 `system-proxy` 特性**默认开启**，在 macOS 上会
    //    自己读「系统代理」（hyper-util → SystemConfiguration 的 HTTP(S)Proxy）。只要系统代理开着，
    //    更新器**本来就走代理**，根本不需要我们插手；
    // 2. reqwest 在每次请求前会**先查 `NO_PROXY`**（`Matcher::intercept()` 的第一句就是它），
    //    命中就直接连 —— 也就是说那段注入把自己刚接上的代理又排除了；
    // 3. 而本机**直连 `github.com` 已被掐死**（DNS 给 20.205.243.166，443 TCP 超时；同一时刻
    //    `api.github.com` 反而直连 200）。而插件没设超时 ⇒ 症状是「界面卡在『正在检查更新…』
    //    约 75s 才报错」。
    //
    // 实测（2026-09-15 23:1x）：经 Clash(`127.0.0.1:7897`) 同一条
    // `releases/latest/download/latest.json` **0.6s 返回 302**，直连 8s / 20s 均无响应。
    //
    // ⇒ 要让某些域名不走代理，请在**系统代理的 bypass 列表**里配；不要在这里写 `NO_PROXY`，
    //   那会连系统代理一起排除掉，而系统代理是更新器唯一的出路。
    let app = tauri::Builder::default()
        // **必须第一个注册**：单实例守卫生效时，后起的实例会在此直接退出，
        // 根本走不到 `setup()`，也就不会去抢反代端口、不会碰 TraeWork。
        //
        // 为什么非有不可：两个实例共享同一份 `settings.json` 和同一个反代端口。
        // 用户在 A 里打开接管 → 写入共享的 `takeover_enabled` → B 轮询到后也去绑同一端口
        // → 只有一个能绑上，抢输的按设计「回滚整个共享开关」→ 赢的看到开关变 false 又释放端口。
        // 净效果是接管永远稳不住，而报错只有一句毫无出路的 `Address already in use`。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第二次点击图标 = 「把已有窗口拿到前面来」，而不是再开一份
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 设备身份（密钥对 + 设备号）要落盘，续签才能成立 —— 见 `devicekey` 顶部说明
            if let Ok(dir) = commands::try_data_dir(app.handle()) {
                oauth::set_data_dir(dir);
            }
            // 定时签到 + 自动续签（**无手动入口**：续签全自动，界面上没有按钮）
            // + 智能接管反代：独立后台线程，与进程同生命周期
            scheduler::spawn(app.handle().clone());
            renew::spawn(app.handle().clone());
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
                        // 端点改写的唯一保留条件：**接管开着 且 反代确实在监听**。
                        // 其余一律恢复官方直连 —— 端点指向一个没人接的端口，是整应用不可用。
                        let keep = s.takeover_enabled && proxy::status().active;
                        let _ = endpoint::sweep(&d, keep);
                        // ⚠️ 反向清扫**不能因为「经系统代理接管」已被移除就删掉**。
                        // 那条路在**用户机器上**留的痕迹不会自己消失：老版本可能把 TraeWork 的
                        // `User/settings.json` 指到了本机回环代理，而新版的反代不再做正向代理
                        // （明文 CONNECT 一律 405），那个设置留着 = TraeWork 整应用不可用。
                        // 所以每次启动都确认一遍：只要它还指着本机回环，就清掉。
                        // `uninstall` 按**回环指纹**判定，绝不碰用户自己配的非回环代理。
                        if traework::applied(s.takeover_port) {
                            if let Ok(msg) = traework::uninstall(&d) {
                                let _ = journal::append(&d, "legacy_proxy_clear", &msg);
                            }
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
            commands::refresh_account_profiles,
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
            commands::takeover_rules,
            commands::takeover_save_rules,
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
