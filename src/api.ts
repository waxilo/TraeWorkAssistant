import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  Settings,
  CheckinResult,
  LogEntry,
  JournalEvent,
  AcctStatus,
  OAuthStart,
  OAuthPoll,
  TakeoverStatus,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");
export const importAccounts = (accounts: Account[]) =>
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
