//! 账号与设置的持久化层：一个 data 目录下两份 JSON。
//!
//! - `accounts.json`：已导入的 TraeWork 账号列表
//! - `settings.json`：应用设置（签到开关、网关开关/端口/账号白名单）

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
    pub created_at: String,
    #[serde(default)]
    pub enabled: bool,
}

impl Account {
    /// 判断 access token 是否已过期（未知有效期认为未过期）
    pub fn expired(&self) -> bool {
        match self.expires_at {
            Some(e) => e <= chrono::Utc::now().timestamp_millis(),
            None => false,
        }
    }
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
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            enabled: true,
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
    /// 本地池化网关开关
    pub gateway_enabled: bool,
    /// 网关监听端口
    pub gateway_port: u16,
    /// 参与网关扣费的账号白名单（空 = 全部）
    pub billing_account_ids: Vec<String>,
    /// 告警 webhook（可选）
    pub webhook_url: String,
    /// 是否开启「自动注入内置模型」（供启动自愈）
    #[serde(default)]
    pub injection_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            checkin_enabled: true,
            checkin_time: "10:00".into(),
            gateway_enabled: false,
            gateway_port: 8788,
            billing_account_ids: Vec::new(),
            webhook_url: String::new(),
            injection_enabled: false,
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

pub fn load_settings(dir: &Path) -> Settings {
    let p = dir.join(settings_path());
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(dir: &Path, settings: &Settings) -> Result<(), String> {
    let p = dir.join(settings_path());
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&p, json).map_err(|e| e.to_string())
}
