//! 账号与设置的持久化层：一个 data 目录下两份 JSON。
//!
//! - `accounts.json`：已导入的 TraeWork 账号列表
//! - `settings.json`：应用设置（签到开关/时刻、智能接管开关/端口、账号白名单、webhook）

use crate::trae_auth::TraeLocalAccount;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub phone: Option<String>,
    pub region: Option<String>,
    pub user_id: Option<String>,
    pub token: String,
    pub refresh_token: Option<String>,
    pub host: Option<String>,
    pub expires_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
    /// 设备标识（签到 API 的隐藏必填头 `X-Device-Id` / `X-Machine-Id` 来源）。
    /// 来自本机 storage.json 的 telemetry，或浏览器登录时绑定的设备；缺失时由 user_id/id 派生稳定值。
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub machine_id: Option<String>,
    pub created_at: String,
    /// 积分快照（供「智能接管」选号：先扣谁的额度）。
    /// `#[serde(default)]` 让旧 `accounts.json`（没有该字段）能正常反序列化。
    #[serde(default)]
    pub credit_snapshot: Option<CreditSnapshot>,
}

/// 账号**已有积分**快照。
///
/// 数据源是 `POST /trae/api/v2/pay/ide_user_ent_usage`（IDE 版 entitlement 用量），
/// 由 `checkin::parse_ent_usage` 按官方 `hHe()` 汇总出「剩余可用积分」与「最快到期时间」。
/// ⚠️ 这不是 `checkin_credits/status` 里的 `credits`——那个是**签到奖励**，两回事。
///
/// 「智能接管」按「**到期最早优先 → 无到期数据靠后 → 积分多者优先**」挑账号
/// （见 `proxy::pick_index`）：先消耗快到期的额度，避免浪费。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CreditSnapshot {
    /// 剩余可用积分；未知为 `None`
    pub credits: Option<i64>,
    /// 是否不限量（entitlement 里存在 `credits_limit = -1` 的包）；此时 `credits` 为 `None`
    #[serde(default)]
    pub unlimited: bool,
    /// 「还有余量的额度包」里最早的到期时间（毫秒时间戳）；未知为 `None`
    pub earliest_expiry_ms: Option<i64>,
    /// 抓取时刻（本地 `YYYY-MM-DD HH:MM:SS`），用于判断快照是否过期
    pub fetched_at: String,
}

impl CreditSnapshot {
    pub fn now(
        credits: Option<i64>,
        unlimited: bool,
        earliest_expiry_ms: Option<i64>,
    ) -> CreditSnapshot {
        CreditSnapshot {
            credits,
            unlimited,
            earliest_expiry_ms,
            fetched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        }
    }
}

/// 判定候选账号是否已存在于列表：手机号优先（两端都有且相等即视为同一人），
/// 否则（任一缺少手机号）退化为按 token 比对。用于「按手机号去重、已存在则不重复添加」。
pub fn contains_equivalent(list: &[Account], cand: &Account) -> bool {
    list.iter().any(|x| match (&x.phone, &cand.phone) {
        (Some(p1), Some(p2)) => p1 == p2,
        _ => x.token == cand.token,
    })
}

impl From<TraeLocalAccount> for Account {
    fn from(a: TraeLocalAccount) -> Account {
        Account {
            id: uuid::Uuid::new_v4().to_string(),
            name: a
                .nickname
                .clone()
                .or_else(|| a.phone.clone())
                .unwrap_or_else(|| "未命名账号".into()),
            phone: a.phone,
            region: a.region,
            user_id: a.user_id,
            token: a.token,
            refresh_token: a.refresh_token,
            host: a.host,
            expires_at: a.expires_at,
            refresh_expires_at: a.refresh_expires_at,
            device_id: a.device_id,
            machine_id: a.machine_id,
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            credit_snapshot: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 设置
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub checkin_enabled: bool,
    /// 签到时刻（24h，如 "10:00"）
    pub checkin_time: String,
    /// 智能接管开关：启动本地反代 + 写入 TraeWork 端点覆盖（见 `endpoint.rs` / `proxy.rs`）。
    pub takeover_enabled: bool,
    /// 本机反代监听端口
    pub takeover_port: u16,
    /// 参与接管的账号白名单（空 = 全部）
    pub billing_account_ids: Vec<String>,
    /// 告警 webhook（可选）
    pub webhook_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            checkin_enabled: true,
            checkin_time: "10:00".into(),
            takeover_enabled: false,
            takeover_port: 8788,
            billing_account_ids: Vec::new(),
            webhook_url: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 读写
// ---------------------------------------------------------------------------

pub fn accounts_path() -> PathBuf {
    PathBuf::from("accounts.json")
}

pub fn settings_path() -> PathBuf {
    PathBuf::from("settings.json")
}

pub fn load_accounts(dir: &Path) -> Vec<Account> {
    let p = dir.join(accounts_path());
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Account>>(&s).ok())
        .unwrap_or_default()
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> Result<(), String> {
    let p = dir.join(accounts_path());
    let json = serde_json::to_string_pretty(accounts).map_err(|e| e.to_string())?;
    std::fs::write(&p, json).map_err(|e| e.to_string())
}

/// 读取设置。
///
/// 兼容早期版本：那时把「本地网关」(`gateway_enabled`) 与「拦截模式」(`intercept_enabled`)
/// 做成两个独立开关、端口叫 `gateway_port`。现在二者合并为「智能接管」，此处做一次性迁移
/// （任一旧开关为真即视为接管开启；端口沿用旧值），避免升级后用户配置丢失。
///
/// 注意：**不能**用 `#[serde(alias)]` 来做这件事——旧配置里两个键可能同时存在，
/// serde 会因「同一字段被赋值两次」而整体反序列化失败，反而丢掉全部设置。
pub fn load_settings(dir: &Path) -> Settings {
    let p = dir.join(settings_path());
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Settings::default();
    };
    let Ok(raw) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Settings::default();
    };
    let mut s: Settings = serde_json::from_value(raw.clone()).unwrap_or_default();
    let legacy_on = |k: &str| raw.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    if legacy_on("intercept_enabled") || legacy_on("gateway_enabled") {
        s.takeover_enabled = true;
    }
    if raw.get("takeover_port").is_none() {
        if let Some(port) = raw.get("gateway_port").and_then(|v| v.as_u64()) {
            s.takeover_port = port.clamp(1, 65535) as u16;
        }
    }
    s
}

pub fn save_settings(dir: &Path, settings: &Settings) -> Result<(), String> {
    let p = dir.join(settings_path());
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&p, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(settings_path()));
        dir
    }

    #[test]
    fn migrates_legacy_gateway_settings() {
        let dir = tmp("twa_settings_migrate_test");
        let legacy = serde_json::json!({
            "checkin_enabled": true,
            "checkin_time": "09:30",
            "gateway_enabled": false,
            "gateway_port": 9999,
            "intercept_enabled": true,
            "injection_enabled": true,
            "webhook_url": "https://example.invalid/hook",
            "billing_account_ids": []
        });
        std::fs::write(dir.join(settings_path()), legacy.to_string()).unwrap();
        let s = load_settings(&dir);
        assert!(s.takeover_enabled, "旧 intercept_enabled=true 应迁移为接管开启");
        assert_eq!(s.takeover_port, 9999, "旧 gateway_port 应被沿用");
        assert_eq!(s.checkin_time, "09:30", "其余设置不得丢失");
        assert_eq!(s.webhook_url, "https://example.invalid/hook");
        let _ = std::fs::remove_file(dir.join(settings_path()));
    }

    #[test]
    fn defaults_when_settings_absent() {
        let dir = tmp("twa_settings_default_test");
        let s = load_settings(&dir);
        assert!(!s.takeover_enabled);
        assert_eq!(s.takeover_port, 8788);
        assert!(s.checkin_enabled);
    }
}
