import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  GatewayStatus,
  Settings,
  CheckinResult,
  LogEntry,
  AcctStatus,
  OAuthStart,
  OAuthPoll,
  InjectOutcome,
  InjectionStatus,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");
export const importAccounts = (accounts: Account[]) =>
  invoke<Account[]>("import_accounts", { accounts });
export const removeAccount = (id: string) =>
  invoke<Account[]>("remove_account", { id });
export const discoverLocal = () => invoke<Account[]>("discover_local");
export const toggleAccount = (id: string, enabled: boolean) =>
  invoke<Account[]>("toggle_account", { id, enabled });
export const checkinOne = (id: string) => invoke<CheckinResult>("checkin_one", { id });
export const checkinAll = () => invoke<CheckinResult[]>("checkin_all");
export const checkinStatus = async (): Promise<AcctStatus[]> => {
  const rows = await invoke<[string, Record<string, unknown> | null][]>("checkin_status");
  return rows.map(([id, data]) => ({
    id,
    checked_in: !!data?.checked_in,
    credits: (data?.credits as number) ?? null,
    message: (data?.message as string) ?? "",
  }));
};
export const getSettings = () => invoke<Settings>("get_settings");
export const getGatewayStatus = () => invoke<GatewayStatus>("gateway_status");
export const saveSettings = (settings: Settings) =>
  invoke<Settings>("save_settings", { settings });
export const importFromFile = (path: string) => invoke<Account[]>("import_from_file", { path });
export const addManualAccount = (o: {
  name?: string;
  host?: string;
  token: string;
  region?: string;
}) => invoke<Account[]>("add_manual_account", o);
export const getLogs = () => invoke<LogEntry[]>("get_logs");
export const clearLogs = () => invoke<void>("clear_logs");
export const oauthStart = (host?: string | null) =>
  invoke<OAuthStart>("oauth_start", { host: host ?? null });
export const oauthPoll = (loginId: string) =>
  invoke<OAuthPoll>("oauth_poll", { loginId });
export const openExternal = (url: string) => invoke<void>("open_external", { url });

export const injectModel = () => invoke<InjectOutcome>("inject_model");
export const getInjectionStatus = () => invoke<InjectionStatus>("injection_status");
export const revertInjection = () => invoke<number>("revert_injection");
