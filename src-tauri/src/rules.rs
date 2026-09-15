//! 接管规则的**热加载**配置：`<数据目录>/proxy-rules.json`。
//!
//! ## 为什么要有这个文件
//!
//! 「哪些请求该换成账号池凭据」是**靠实测收敛**的，不是靠读文档能定下来的：
//! TraeWork 换个接口名、把推理从 HTTP 挪到 WebSocket、某个路径只是长得像扣费路径，
//! 判错的代价都是**静默**的（界面上一切正常，池子账号一分不扣）。
//!
//! 把这几个旋钮放进一个**运行时读取**的 JSON，意味着调这些参数**不需要重新编译、重新签名、
//! 重新启动** —— 改完立即生效。这在真机对账时是决定性的：一轮「发消息 → 看扣了谁」只要几十秒，
//! 而重编译一轮要好几分钟，还会把「哪次改动导致了变化」这件事搅浑。
//!
//! ## 默认值是**保守**的
//!
//! 文件不存在时用 [`Rules::default`]：只换内置表里那几条**已经被验证不会弄坏应用**的前缀，
//! **不**动 WebSocket。想扩大战果就显式写进文件 —— 默认值绝不允许「开启即坏」。
//!
//! ## 观察模式
//!
//! `observe_only: true` 时**一个凭据都不换**，但仍然：走完整条隧道、记录每条请求的
//! 域名/路径/判定结果/上游状态码。这是排查「接管开着为什么没省额度」的**首选形态** ——
//! 先看清应用在和谁说什么，再决定改哪条规则。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const RULES_FILE: &str = "proxy-rules.json";

/// 规则文件的缓存有效期。文件很小，但**每个请求**都要问一次「这条路径要不要换号」，
/// 所以既不能每次读盘，也不能永远缓存（那就失去了热加载的意义）。
const CACHE_TTL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// 只观察、不换凭据。诊断用，也是唯一「绝对弄不坏应用」的形态。
    pub observe_only: bool,
    /// 覆盖内置扣费前缀表（空 = 用内置表）。
    pub swap_http_prefixes: Vec<String>,
    /// 是否连 WebSocket 握手里的 `Authorization` 一起换。
    ///
    /// ⚠️ 这不是可有可无的：端点是**连 `ws` 服务一起改写**的（见 `endpoint::PATCHED_SERVICES`），
    /// 所以实时通道真的会经过本机反代 —— 端点模式下它照样在管辖范围内。
    pub swap_ws: bool,
    /// **仅诊断**：把这些前缀强制判为「透传」，即使内置表命中。
    pub never_swap_prefixes: Vec<String>,
}

impl Rules {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(RULES_FILE)
    }

    /// 读规则（带 1s 缓存）。文件不存在 / 读不懂 → 用默认值，**绝不因为配置坏了就停摆**。
    pub fn load(dir: &Path) -> Rules {
        let path = Self::path(dir);
        if let Ok(map) = cache().lock() {
            if let Some((at, rules)) = map.get(&path) {
                if at.elapsed() < CACHE_TTL {
                    return rules.clone();
                }
            }
        }
        let rules = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Rules>(&text).unwrap_or_default(),
            Err(_) => Rules::default(),
        };
        if let Ok(mut map) = cache().lock() {
            // 清掉同一路径的过期项，顺手把没人再问的路径删掉 —— 键是数据目录，数量天然有限
            map.retain(|p, (at, _)| *p == path || at.elapsed() < CACHE_TTL);
            map.insert(path, (Instant::now(), rules.clone()));
        }
        rules
    }

    /// 把内置默认写成一份**带说明**的文件（幂等：已存在就不动）。
    ///
    /// 写出来的目的不是「让用户去编辑」，而是**让这些旋钮可见** ——
    /// 排查接管不生效时，第一件要确认的事就是「现在到底用的是哪张表」。
    pub fn write_default_if_absent(dir: &Path) -> Result<PathBuf, String> {
        let path = Self::path(dir);
        if path.exists() {
            return Ok(path);
        }
        let text = serde_json::to_string_pretty(&Rules::default())
            .map_err(|e| format!("序列化默认规则失败：{e}"))?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
        Ok(path)
    }

    /// 这条路径要不要换成账号池凭据。
    ///
    /// `builtin` = 代码里那张内置表 —— 文件里的 `swap_http_prefixes` 非空时**完全取代**它
    /// （而不是叠加）：测试时需要一个「干净、只由文件决定」的状态，否则永远分不清
    /// 命中的是内置表还是自己写的规则。
    pub fn should_swap(&self, path: &str, builtin: &[&str]) -> bool {
        if self.observe_only {
            return false;
        }
        if self.never_swap_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
            return false;
        }
        if self.swap_http_prefixes.is_empty() {
            return builtin.iter().any(|p| path.starts_with(p));
        }
        self.swap_http_prefixes.iter().any(|p| path.starts_with(p.as_str()))
    }
}

/// 规则缓存：**按路径分别缓存**。
///
/// 曾经这里只存一条（`Option<(PathBuf, …)>`）。生产环境只有一个数据目录，所以看不出问题；
/// 但并行跑的测试会各自用不同的临时目录，一条缓存会被互相顶掉 —— 表现为「规则明明写对了
/// 却时灵时不灵」的随机失败。键是数据目录，数量天然有限，改成 map 没有代价。
type Cache = Mutex<HashMap<PathBuf, (Instant, Rules)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 丢掉缓存。写入方（`takeover_save_rules`）调用它，好让界面**立刻**看到新值 ——
/// 否则会有最多 1 秒的「我刚改的规则怎么没生效」。
pub fn invalidate() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTIN: &[&str] = &["/api/remote/v1/chat_sessions", "/api/remote/v1/models"];

    #[test]
    fn default_rules_use_the_builtin_table_and_never_touch_ws() {
        let r = Rules::default();
        assert!(!r.observe_only);
        assert!(r.swap_http_prefixes.is_empty(), "默认必须用内置表");
        assert!(!r.swap_ws, "默认绝不动 WebSocket —— 那是未验证的领域");
        assert!(r.should_swap("/api/remote/v1/chat_sessions?x=1", BUILTIN));
        assert!(!r.should_swap("/api/agent/v3/llm_utils_chat", BUILTIN));
    }

    #[test]
    fn observe_only_swallows_everything() {
        let r = Rules { observe_only: true, ..Rules::default() };
        assert!(!r.should_swap("/api/remote/v1/chat_sessions", BUILTIN));
    }

    #[test]
    fn file_table_replaces_the_builtin_one() {
        let r = Rules {
            swap_http_prefixes: vec!["/api/agent/v3/".into()],
            ..Rules::default()
        };
        assert!(r.should_swap("/api/agent/v3/llm_utils_chat", BUILTIN));
        assert!(
            !r.should_swap("/api/remote/v1/chat_sessions", BUILTIN),
            "文件里的表是**取代**内置表，不是叠加"
        );
    }

    #[test]
    fn never_swap_wins_over_everything() {
        let r = Rules {
            swap_http_prefixes: vec!["/api/".into()],
            never_swap_prefixes: vec!["/api/agent/v3/sync_history_state".into()],
            ..Rules::default()
        };
        assert!(r.should_swap("/api/agent/v3/llm_utils_chat", BUILTIN));
        assert!(!r.should_swap("/api/agent/v3/sync_history_state", BUILTIN));
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("twa-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = Rules::load(&dir);
        assert_eq!(r.observe_only, Rules::default().observe_only);
        // 坏 JSON 也不能让接管停摆：解析失败同样落到默认值
        std::fs::write(Rules::path(&dir), "{ 这不是合法 JSON").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        let r2 = Rules::load(&dir);
        assert!(!r2.observe_only);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
