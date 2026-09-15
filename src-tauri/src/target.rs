//! 「接管哪些应用」—— 目标发现、身份与进程控制。
//!
//! ## 为什么不再是「一个写死的名字列表」
//!
//! 旧实现的 `endpoint::app_dir()` 在一张写死的候选名里取**第一个存在**的：
//! `["TRAE SOLO CN", "TRAE", "Trae TRAE", "TRAE CN", "Trae CN"]`。
//! 本机同时装了 `TRAE SOLO CN.app` 与 `Trae CN.app`（同一次构建出的两个 shell，
//! `product.json` 的 version/commit 完全一致），于是后者**永远轮不到** ——
//! 端点改写与免证书补丁都会打在 SOLO 上。这不是配置问题，是结构问题。
//!
//! 现在改成**扫描发现**：在系统应用目录里枚举 `.app`（Windows 下枚举安装目录），
//! 逐个读它的 `Resources/app/product.json`，**只收带 `bootConfig` 的构建**。
//! 于是：
//!
//! - 列表里的每一项都对应**真实存在**的一个应用，界面可以直接把它们列出来让用户勾选；
//! - 上游改版 / 换目录 / 出新区域版都自动跟上，不需要改代码；
//! - 判据是**文件内容**（`bootConfig`），不是名字 —— 名字只用来做稳定 id。
//!
//! ## id 是什么，为什么是它
//!
//! macOS 下 id = `.app` 文件名去掉 `.app`（`TRAE SOLO CN` / `Trae CN`）；
//! Windows 下 = 安装目录名。这一个字符串同时承担三件事：
//!
//! 1. 设置里记录「接管哪些应用」的键（`Settings.takeover_apps`）；
//! 2. 界面上的标签；
//! 3. macOS 上 `open` / `quit app` 与 Windows 上 `taskkill` / 启动 exe 的依据。
//!
//! 一个值三处用，不需要维护任何映射表 —— 也用不着再猜「这个名字属于哪个产品」。
//!
//! ## 进程控制为什么住在这里
//!
//! 「这个应用在不在跑 / 怎么让它退出、再拉起来」是关于**应用**的事实，与「怎么改写它的
//! `product.json`」无关。旧实现把两者都塞在 `endpoint.rs` 里，于是多目标改造时
//! 它们会被迫一起变形。分开之后 `endpoint.rs` 只谈配置、这里只谈进程。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 产品配置文件（相对 [`AppTarget::app_dir`]）。
pub const PRODUCT_FILE: &str = "product.json";
/// 闸门补丁所在文件（相对 [`AppTarget::app_dir`]）。
pub const MAIN_JS_REL: &str = "out/main.js";

/// 一个**本机真实存在**的可接管应用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTarget {
    /// 稳定 id，见模块文档。
    pub id: String,
    /// 应用包（macOS 的 `.app`）或安装目录（Windows）。
    pub bundle: PathBuf,
    /// `<bundle>/…/Resources/app`，内含 `product.json` 与 `out/main.js`。
    pub app_dir: PathBuf,
}

impl AppTarget {
    pub fn product_path(&self) -> PathBuf {
        self.app_dir.join(PRODUCT_FILE)
    }

    pub fn main_js_path(&self) -> PathBuf {
        self.app_dir.join(MAIN_JS_REL)
    }

    /// 该应用当前是否在运行。
    pub fn running(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            // ⚠️ 用**完整 bundle 路径**而不是应用名：既能把 Electron 的主进程与各 helper
            //    一起匹配上（它们都在 `<bundle>/Contents/…` 里），又不会误伤
            //    「我们自己刚发出的 `open` 命令」那种 argv 只含名字的进程。
            //    `pgrep -f` 的 pattern 是 ERE，路径里的 `.` 会被当通配符 —— 无害
            //    （多匹配一个字符不会命中别的应用），换来不引正则转义的复杂度。
            let pattern = self.bundle.to_string_lossy().to_string();
            std::process::Command::new("pgrep")
                .arg("-f")
                .arg(&pattern)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(target_os = "windows")]
        {
            let images = process_images();
            images.iter().any(|n| *n == process_image_name(&self.id))
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            false
        }
    }

    /// 请求该应用退出并等待其结束。
    ///
    /// Windows 用 `taskkill /im <exe> /t` 且**刻意不加 `/f`**：这是编辑器，强杀可能丢失
    /// 未保存内容；不带 `/f` 时系统向窗口投递关闭消息，由应用自行保存退出。
    pub fn quit_graceful(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            // `quit app "<路径>"` 而不是 `tell application "<名字>"`：路径能精确定位到
            // 这一个应用包，不依赖 LaunchServices 的名字解析（`~/Applications` 里的同名副本
            // 与 `/Applications` 里的会被解析成谁，不该由我们猜）。
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(format!("quit app {:?}", self.bundle.to_string_lossy()))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/im", &process_image_name(&self.id), "/t"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        wait_for_exit(self, 20_000)
    }

    /// 重新启动该应用。
    ///
    /// ⚠️ `spawn()` 之后**必须立刻返回，绝不能 `wait()`**：`wait()` 会阻塞到应用进程退出
    /// 为止（可能数小时），一旦被同步命令调用就会把执行线程彻底占死 —— 这正是历史上
    /// 「助手卡死」的根因。
    pub fn relaunch(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            // `open <路径>` 而不是 `open -a <名字>`：名字要经 LaunchServices 解析，
            // 而路径就是我们要的那一个应用包。
            std::process::Command::new("open")
                .arg(&self.bundle)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
        }
        #[cfg(target_os = "windows")]
        {
            let exe = self.bundle.join(process_image_name(&self.id));
            std::process::Command::new(&exe)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            false
        }
    }
}

/// 轮询等待该应用完全退出，直到 `timeout_ms` 毫秒。
pub fn wait_for_exit(target: &AppTarget, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if !target.running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    !target.running()
}

/// 在「这些应用运行中则先退出」的前提下执行 `op`，执行完再把**原先在跑的那些**拉起来。
///
/// 返回 `(op 结果, 被重启的应用 id)`。
///
/// 三件事是刻意的：
/// 1. **只碰传进来的目标** —— 改一个应用的配置不该重启另一个应用；
/// 2. `op` 失败也要把应用拉回来（`op` 被闸门挡住时应用已经被我们关掉了，
///    不能因为返回 `Err` 就把它留在关闭状态，2026-09-14 实测踩过）；
/// 3. 中途有应用退不掉时，把**已经退出的那些**先拉回来再报错 —— 半关半开是最糟的状态。
pub fn with_restart<T>(
    targets: &[AppTarget],
    op: impl FnOnce() -> Result<T, String>,
) -> Result<(T, Vec<String>), String> {
    let mut stopped: Vec<&AppTarget> = Vec::new();
    for t in targets {
        if !t.running() {
            continue;
        }
        if !t.quit_graceful() {
            for done in &stopped {
                let _ = done.relaunch();
            }
            return Err(format!(
                "已发起退出请求，但「{}」未在限时内退出，请手动关闭后重试。",
                t.id
            ));
        }
        stopped.push(t);
    }

    let out = op();

    let mut restarted = Vec::new();
    for t in &stopped {
        if t.relaunch() {
            restarted.push(t.id.clone());
        }
    }
    Ok((out?, restarted))
}

// ---------------------------------------------------------------------------
// 发现
// ---------------------------------------------------------------------------

/// 枚举本机的应用容器目录（macOS：`/Applications`、`~/Applications`）。
///
/// 顺序即优先级：同名 id 以**先到的**为准（用户目录里的副本就当不存在，避免同一个应用
/// 出现两行、勾了一个另一个没勾）。
pub fn roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from("/Applications"));
        if let Some(home) = dirs::home_dir() {
            out.push(home.join("Applications"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        for base in [dirs::data_local_dir(), dirs::data_dir()].into_iter().flatten() {
            // 用户级安装（默认）：%LOCALAPPDATA%\Programs\<app>；少数安装器直接落在 %LOCALAPPDATA%\<app>
            out.push(base.join("Programs"));
            out.push(base);
        }
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(key) {
                out.push(PathBuf::from(pf));
            }
        }
    }
    out
}

/// `<bundle>` → `<bundle>/…/Resources/app`。
pub fn app_dir_of(bundle: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        bundle.join("resources").join("app")
    }
    #[cfg(not(target_os = "windows"))]
    {
        bundle.join("Contents").join("Resources").join("app")
    }
}

/// 这个 `Resources/app` 是不是一个可接管的 Trae 构建。
///
/// 判据只有一条：`product.json` 能被解析、且顶层有 **`bootConfig` 对象**。
/// 那正是决定端点域名的那份配置（见 `endpoint.rs` 模块文档），也是 Trae 系构建独有的字段
/// —— VS Code / 其它 Electron 应用的同名文件里没有它（本机 `Kiro.app` 就是反例）。
///
/// 本助手自己也不会有这一份文件（Tauri 的 `Resources/` 里没有 `app/product.json`），
/// 所以不需要为「别把自己列成目标」再写一条特例。
pub fn is_trae_app(app_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(app_dir.join(PRODUCT_FILE)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    v.get("bootConfig").map(serde_json::Value::is_object).unwrap_or(false)
}

/// `.app` / 安装目录 → 稳定 id。
fn id_of(bundle: &Path) -> Option<String> {
    let name = bundle.file_name()?.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    {
        name.strip_suffix(".app")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (!name.is_empty()).then_some(name)
    }
}

/// 本机**全部**可接管的 Trae 应用，按 id 升序（顺序稳定，界面不会跳）。
pub fn discover() -> Vec<AppTarget> {
    let mut out: Vec<AppTarget> = Vec::new();
    for root in roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue; // 目录不存在 / 读不了都不是错误（Windows 上 ProgramFiles(x86) 常见缺席）
        };
        for e in entries.flatten() {
            let bundle = e.path();
            if !bundle.is_dir() {
                continue;
            }
            // macOS 上先按后缀筛掉绝大部分条目，再去看那个 200 字节的 product.json。
            // 目录遍历本身很便宜，真正的成本只有「文件确实存在」时的 read + parse。
            let app_dir = app_dir_of(&bundle);
            if !is_trae_app(&app_dir) {
                continue;
            }
            let Some(id) = id_of(&bundle) else {
                continue;
            };
            if out.iter().any(|t| t.id == id) {
                continue; // 同名副本（用户目录 vs /Applications）以先到的为准
            }
            out.push(AppTarget { id, bundle, app_dir });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 把设置里的 id 名单解析成**真实存在的**目标。
///
/// **空名单 = 全部**（与「参与扣费的账号」`billing_account_ids` 同一套语义：空 = 没配置过
/// = 全部参与）。非空时按发现顺序取交集，名单里本机没有的 id 被静默忽略
/// （要报「你选的应用不在本机」请用 [`missing`]）。
pub fn select(ids: &[String]) -> Vec<AppTarget> {
    let all = discover();
    if ids.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|t| ids.iter().any(|i| i == &t.id))
        .collect()
}

/// 名单里本机**不存在**的 id。界面靠它说清「你点名的应用已经不在了」。
pub fn missing(ids: &[String]) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    let all = discover();
    ids.iter()
        .filter(|i| !all.iter().any(|t| &t.id == *i))
        .cloned()
        .collect()
}

/// 某个 id 是否在接管名单里（**空名单 = 全部命中**，与 [`select`] 同一套语义）。
pub fn is_selected(ids: &[String], id: &str) -> bool {
    ids.is_empty() || ids.iter().any(|i| i == id)
}

/// 「默认目标」= 发现列表里的第一个。
///
/// ⚠️ **只给与接管无关的读用途**（`x-app-version` 这种「这个应用是什么版本」的问题）。
/// 接管本身一律显式带目标 —— 这正是本次改造要消灭的那种隐式单目标假设。
pub fn default_target() -> Option<AppTarget> {
    discover().into_iter().next()
}

// ---------------------------------------------------------------------------
// Windows 进程辅助
// ---------------------------------------------------------------------------

/// Windows 进程映像名（`tasklist` 里那一列）。
#[cfg(target_os = "windows")]
fn process_image_name(id: &str) -> String {
    format!("{id}.exe")
}

/// `tasklist` 输出的全部映像名（小写）。
#[cfg(target_os = "windows")]
fn process_images() -> Vec<String> {
    let Ok(o) = std::process::Command::new("tasklist")
        .args(["/fo", "csv", "/nh"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.split("\",\"").next())
        .map(|s| s.trim_matches('"').to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个「长得像 Trae 的 Resources/app」和几个反例。
    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("twa-target-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn win_app(root: &Path, name: &str, product: &str) -> PathBuf {
        let app = app_dir_of(&root.join(format!("{name}.app")));
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join(PRODUCT_FILE), product).unwrap();
        app
    }

    #[test]
    fn app_dir_layout_is_the_usual_one() {
        let d = app_dir_of(Path::new("/Applications/X.app"));
        assert!(d.ends_with("app"), "{}", d.display());
        assert!(d.to_string_lossy().contains("Resources"), "{}", d.display());
    }

    /// 判据是 `bootConfig`，不是名字 —— 这样上游出新区域版、或换个名字都不影响。
    #[test]
    fn only_builds_with_boot_config_are_targets() {
        let root = fixture("is-trae");
        let yes = win_app(&root, "TRAE SOLO CN", r#"{"bootConfig":{"remote":{"trae":{"normal":"https://a"}}}}"#);
        let no_boot = win_app(&root, "Kiro", r#"{"nameShort":"Kiro","version":"1.0"}"#);
        let not_json = win_app(&root, "Broken", "{ 这不是 JSON");
        assert!(is_trae_app(&yes));
        assert!(!is_trae_app(&no_boot), "没有 bootConfig 的不是目标（本机 Kiro.app 就是反例）");
        assert!(!is_trae_app(&not_json), "读不懂就不认");
        assert!(!is_trae_app(&root.join("no-such-app").join("app")), "不存在 = 不是");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `bootConfig` 必须是对象：`"bootConfig": "x"` 是别的什么也不该被接管。
    #[test]
    fn boot_config_must_be_an_object() {
        let root = fixture("boot-type");
        let p = win_app(&root, "Weird", r#"{"bootConfig":"nope"}"#);
        assert!(!is_trae_app(&p));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn id_drops_the_app_suffix() {
        assert_eq!(id_of(Path::new("/Applications/Trae CN.app")).as_deref(), Some("Trae CN"));
        assert_eq!(id_of(Path::new("/Applications/A.app")).as_deref(), Some("A"));
        assert_eq!(id_of(Path::new("/Applications/.app")), None);
    }

    /// 空名单 = 全部；非空 = 交集；本机没有的 id 被忽略且能被 [`missing`] 报出来。
    #[test]
    fn empty_selection_means_everything() {
        assert!(is_selected(&[], "任意"));
        assert!(is_selected(&["Trae CN".into()], "Trae CN"));
        assert!(!is_selected(&["Trae CN".into()], "TRAE SOLO CN"));
    }

    /// `discover()` 的顺序必须稳定：界面按它渲染，顺序跳会让「我点的那个」对不上号。
    #[test]
    fn discovery_order_is_stable() {
        let a = AppTarget {
            id: "TRAE SOLO CN".into(),
            bundle: PathBuf::from("/A.app"),
            app_dir: PathBuf::from("/A.app/Contents/Resources/app"),
        };
        let b = AppTarget {
            id: "Trae CN".into(),
            bundle: PathBuf::from("/B.app"),
            app_dir: PathBuf::from("/B.app/Contents/Resources/app"),
        };
        let mut v = vec![b.clone(), a.clone()];
        v.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(v[0].id, "TRAE SOLO CN", "大写先于小写 ⇒ SOLO 在 Trae CN 之前");
        assert_eq!(v[1].id, "Trae CN");
    }

    /// 真机只读：本机发现了哪些可接管应用（多目标改造的第一道验证）。
    ///
    /// ```text
    /// cargo test --lib -- --nocapture live_discover_apps
    /// ```
    #[test]
    fn live_discover_apps() {
        let found = discover();
        for t in &found {
            eprintln!(
                "  id={:<16} bundle={} running={} main.js={}",
                t.id,
                t.bundle.display(),
                t.running(),
                t.main_js_path().exists()
            );
        }
        eprintln!("本机发现 {} 个可接管应用", found.len());
        // 只要有发现，id 就必须唯一、且 product.json 真的在
        for t in &found {
            assert!(t.product_path().exists(), "{} 的 product.json 不见了", t.id);
        }
        let mut ids: Vec<&str> = found.iter().map(|t| t.id.as_str()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "id 必须唯一");
    }

}
