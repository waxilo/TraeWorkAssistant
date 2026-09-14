//! 接管动态（journal）：记录「智能接管」到底做了什么。
//!
//! 开启/关闭接管、重启 TraeWork、某个会话开始用哪个账号、被限流换号、代理异常——
//! 每条都追加到数据目录下的 `takeover-journal.jsonl`，供「智能接管」页回看。
//!
//! ## 为什么落盘而不是放在内存
//!
//! 事件由**反代的连接线程**写入（每个 TCP 连接一个线程），界面则通过 Tauri 命令读取。
//! jsonl 追加语义天然支持「一边写一边读」，且助手重启后历史仍在——排障时最想看的
//! 恰恰是**上一次**那批事件。配合每行一个 JSON，单条损坏也不会毁掉整份历史。
//!
//! ## 容量与并发
//!
//! 只保留最近 [`MAX_EVENTS`] 条（超出丢弃最旧的）。所有写入串行化在一把进程内互斥锁上
//! （否则多线程同时「读全量 + 重写」会互相覆盖），单次事件量级很小（每次写入重写整个
//! 文件，故**不要**在每请求路径上调用 [`append`]——只在会话切换/限流/异常时调用）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 接管动态文件名（落在助手自己的数据目录）。
const JOURNAL_FILE: &str = "takeover-journal.jsonl";
/// 最多保留的事件条数。
const MAX_EVENTS: usize = 500;

/// 一条接管事件。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JournalEvent {
    /// 本地时间字符串（展示用）
    pub at: String,
    /// 毫秒时间戳（排序 / 前端 key 用）
    pub at_ms: i64,
    /// 事件类型，见模块内使用的常量式字符串（`install` / `route_start` / `failover` …）
    pub event: String,
    /// 人类可读说明
    pub detail: String,
}

pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

/// 写入锁：反代每条连接一个线程，并发「读全量 + 重写」会丢事件。
fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 读取全部事件，**最新在前**。
pub fn read(data_dir: &Path) -> Vec<JournalEvent> {
    let Ok(text) = std::fs::read_to_string(journal_path(data_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<JournalEvent>(l).ok())
        .collect()
}

fn write_all(data_dir: &Path, events: &[JournalEvent]) -> Result<(), String> {
    let mut buf = String::with_capacity(events.len() * 160);
    for e in events {
        let line = serde_json::to_string(e).map_err(|err| err.to_string())?;
        buf.push_str(&line);
        buf.push('\n');
    }
    std::fs::write(journal_path(data_dir), buf).map_err(|e| e.to_string())
}

/// 追加一条事件。**失败只打日志，绝不打断反代主流程。**
pub fn append(data_dir: &Path, event: &str, detail: &str) {
    write_entry(data_dir, event, detail, false)
}

/// 追加一条事件，但**若最新一条与本次完全相同则跳过**。
///
/// 用于「配置型问题」类事件（如账号池为空、上游域名解析不出来）——这类问题会让
/// **每一个**路过反代的请求都产生同一行，不抑制就会把 500 条容量瞬间刷满，
/// 把真正有价值的「谁用了哪个账号」挤出去。
pub fn append_dedup(data_dir: &Path, event: &str, detail: &str) {
    write_entry(data_dir, event, detail, true)
}

fn write_entry(data_dir: &Path, event: &str, detail: &str, dedup: bool) {
    let now = chrono::Local::now();
    let entry = JournalEvent {
        at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        at_ms: now.timestamp_millis(),
        event: event.to_string(),
        detail: detail.to_string(),
    };
    let _guard = write_lock().lock();
    let mut events = read(data_dir);
    if dedup {
        if let Some(last) = events.first() {
            if last.event == event && last.detail == detail {
                return;
            }
        }
    }
    events.insert(0, entry);
    if events.len() > MAX_EVENTS {
        events.truncate(MAX_EVENTS);
    }
    if let Err(e) = write_all(data_dir, &events) {
        eprintln!("[接管动态] 写入失败：{e}");
    }
}

/// 清空全部事件。
pub fn clear(data_dir: &Path) {
    let _guard = write_lock().lock();
    let _ = std::fs::remove_file(journal_path(data_dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn appends_newest_first_and_roundtrips() {
        let dir = tmp("twa_journal_basic_test");
        append(&dir, "install", "开启接管");
        append(&dir, "route_start", "会话 abc 开始使用账号「A」");
        let events = read(&dir);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "route_start", "最新事件必须排在最前");
        assert_eq!(events[0].detail, "会话 abc 开始使用账号「A」");
        assert_eq!(events[1].event, "install");
        assert!(events[0].at_ms > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caps_length_and_keeps_latest() {
        let dir = tmp("twa_journal_cap_test");
        for i in 0..(MAX_EVENTS + 20) {
            append(&dir, "failover", &format!("e{i}"));
        }
        let events = read(&dir);
        assert_eq!(events.len(), MAX_EVENTS, "不得超过上限");
        assert_eq!(events[0].detail, format!("e{}", MAX_EVENTS + 19), "必须保留最新的");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_history() {
        let dir = tmp("twa_journal_clear_test");
        append(&dir, "install", "x");
        assert_eq!(read(&dir).len(), 1);
        clear(&dir);
        assert!(read(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn survives_corrupt_line() {
        let dir = tmp("twa_journal_corrupt_test");
        append(&dir, "install", "good");
        // 手工塞一行坏数据：单条损坏不应毁掉其余历史
        let p = journal_path(&dir);
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("{ not json\n");
        std::fs::write(&p, text).unwrap();
        assert_eq!(read(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_skips_consecutive_duplicates_only() {
        let dir = tmp("twa_journal_dedup_test");
        append_dedup(&dir, "proxy_error", "账号池为空");
        append_dedup(&dir, "proxy_error", "账号池为空");
        append_dedup(&dir, "proxy_error", "账号池为空");
        assert_eq!(read(&dir).len(), 1, "连续重复必须被抑制");
        // 中间插一条不同事件后，同一内容可以再记
        append(&dir, "route_start", "会话 a 开始使用账号「A」");
        append_dedup(&dir, "proxy_error", "账号池为空");
        assert_eq!(read(&dir).len(), 3);
        // 普通 append 不做抑制（每次都是真实发生的事）
        append(&dir, "install", "x");
        append(&dir, "install", "x");
        assert_eq!(read(&dir).len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发写入不得丢事件：反代每个连接一个线程，全靠这把锁。
    #[test]
    fn loses_nothing_under_concurrency() {
        let dir = tmp("twa_journal_concurrency_test");
        let mut handles = Vec::new();
        for t in 0..4 {
            let d = dir.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    append(&d, "route_start", &format!("t{t}-i{i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(read(&dir).len(), 40, "40 次追加必须一条不少");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
