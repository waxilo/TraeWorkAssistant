export interface Account {
  id: string;
  name: string;
  phone?: string | null;
  region?: string | null;
  user_id?: string | null;
  token: string;
  refresh_token?: string | null;
  host?: string | null;
  expires_at?: number | null;
  refresh_expires_at?: number | null;
  created_at: string;
  enabled: boolean;
}

export interface GatewayStatus {
  active: boolean;
  port: number;
  error: string | null;
}

export interface Settings {
  checkin_enabled: boolean;
  checkin_time: string;
  gateway_enabled: boolean;
  gateway_port: number;
  billing_account_ids: string[];
  webhook_url: string;
  injection_enabled: boolean;
}

export interface CheckinResult {
  success: boolean;
  already: boolean;
  inactive: boolean;
  message: string;
  credit?: number | null;
  host?: string | null;
  at: string;
}

export interface LogEntry {
  at: string;
  account: string;
  message: string;
  success: boolean;
}

export interface AcctStatus {
  id: string;
  checked_in: boolean;
  credits: number | null;
  message: string;
}

export type Page = "accounts" | "gateway" | "logs" | "settings";

export interface OAuthStart {
  login_id: string;
  verification_uri: string;
  host: string;
  expires_in: number;
}

export interface OAuthPoll {
  done: boolean;
  token?: string | null;
  refresh_token?: string | null;
  host?: string | null;
  region?: string | null;
  uid?: string | null;
  nickname?: string | null;
  phone?: string | null;
  expires_at?: number | null;
  error?: string | null;
}

export interface InjectOutcome {
  needs_quit: boolean;
  restarted: boolean;
  injected_db_count: number;
  labels: string[];
  message: string;
}

export interface InjectionEntry {
  db: string;
  key: string;
  labels: string[];
  base_url: string;
}

export interface InjectionStatus {
  trae_running: boolean;
  entries: InjectionEntry[];
}
