import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  NewAccount,
  Settings,
  CheckinResult,
  LogEntry,
  JournalEvent,
  AcctStatus,
  OAuthStart,
  OAuthPoll,
  TakeoverStatus,
  TakeoverRules,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");
/** 按需回源补全账号资料（占位名/缺失手机号 → 服务端真实昵称与脱敏手机号），返回更新后的列表 */
export const refreshAccountProfiles = () => invoke<Account[]>("refresh_account_profiles");
/** 导入账号：`id` / `created_at` 交给后端补（见 `NewAccount`） */
export const importAccounts = (accounts: NewAccount[]) =>
  invoke<Account[]>("import_accounts", { accounts });
export const removeAccount = (id: string) =>
  invoke<Account[]>("remove_account", { id });
export const discoverLocal = () => invoke<Account[]>("discover_local");
export const checkinOne = (id: string) => invoke<CheckinResult>("checkin_one", { id });
export const checkinAll = () => invoke<CheckinResult[]>("checkin_all");
/** 每个账号的签到状态 + **账号已有积分**（后端直接返回结构化数据，前端不再自行解析 JSON） */
export const checkinStatus = () => invoke<AcctStatus[]>("checkin_status");
export const getSettings = () => invoke<Settings>("get_settings");
export const saveSettings = (settings: Settings) =>
  invoke<Settings>("save_settings", { settings });
export const getLogs = () => invoke<LogEntry[]>("get_logs");
export const clearLogs = () => invoke<void>("clear_logs");
export const oauthStart = (host?: string | null) =>
  invoke<OAuthStart>("oauth_start", { host: host ?? null });
export const oauthPoll = (loginId: string) =>
  invoke<OAuthPoll>("oauth_poll", { loginId });
export const openExternal = (url: string) => invoke<void>("open_external", { url });

export const getTakeoverStatus = () => invoke<TakeoverStatus>("takeover_status");
export const enableTakeover = () => invoke<TakeoverStatus>("takeover_enable");
export const disableTakeover = () => invoke<TakeoverStatus>("takeover_disable");
/** 接管动态（最新在前）：谁用了哪个账号、有没有限流换号、代理是否报错 */
export const takeoverEvents = () => invoke<JournalEvent[]>("takeover_events");
export const clearTakeoverEvents = () => invoke<void>("clear_takeover_events");
// 「打 / 还原 TraeWork 补丁」这两个动作**没有独立命令**：补丁的生命周期已经并进
// `takeover_enable` / `takeover_disable`（开接管自动打、关接管自动还原），
// 界面因此不需要、也不应该再单独碰它 —— 见 `src-tauri/src/commands.rs` 的补丁小节。
/** 当前接管规则（`proxy-rules.json`）。写入后**立即生效**（读侧只有 1 秒缓存）。 */
export const getTakeoverRules = () => invoke<TakeoverRules>("takeover_rules");
export const saveTakeoverRules = (rules: TakeoverRules) =>
  invoke<TakeoverRules>("takeover_save_rules", { rules });
