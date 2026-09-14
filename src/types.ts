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
  device_id?: string | null;
  machine_id?: string | null;
  created_at: string;
  /** 积分快照（供「智能接管」选号：先扣谁的额度） */
  credit_snapshot?: CreditSnapshot | null;
}

/**
 * 账号**已有积分**快照（来自 TraeWork 的 `ide_user_ent_usage` 额度用量接口，
 * 即「剩余可用额度」）。⚠️ 不是签到奖励积分。
 */
export interface CreditSnapshot {
  /** 剩余可用积分；未知为 null */
  credits: number | null;
  /** 不限量（账号存在 credits_limit = -1 的额度包） */
  unlimited: boolean;
  /** 「还有余量的额度包」里最早的到期时间（毫秒）；未知为 null */
  earliest_expiry_ms: number | null;
  fetched_at: string;
}

export interface Settings {
  checkin_enabled: boolean;
  checkin_time: string;
  /** 智能接管：本地反代 + 写 TraeWork 端点覆盖（一体开关） */
  takeover_enabled: boolean;
  /** 本地反代监听端口 */
  takeover_port: number;
  billing_account_ids: string[];
  webhook_url: string;
}

export interface CheckinResult {
  success: boolean;
  already: boolean;
  inactive: boolean;
  /** 服务端限流（9074）等瞬时失败，稍后会自动补签 */
  transient: boolean;
  /** token 失效，需要重新登录 */
  auth_failed: boolean;
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

/**
 * 一条「接管动态」。事件类型见 `journal.rs`：
 * `install` / `uninstall` / `sweep` / `restart_trae` / `route_start` / `failover` / `proxy_*`
 */
export interface JournalEvent {
  at: string;
  at_ms: number;
  event: string;
  detail: string;
}

export interface AcctStatus {
  id: string;
  checked_in: boolean;
  /** 账号**已有积分**（entitlement 剩余额度）；未知为 null */
  credits: number | null;
  /** 不限量 */
  unlimited: boolean;
  message: string;
}

export type Page = "accounts" | "takeover" | "logs" | "settings";

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
  device_id?: string | null;
  machine_id?: string | null;
  error?: string | null;
}

/** 智能接管状态（本地反代 + TraeWork 端点覆盖）。 */
export interface TakeoverStatus {
  /** 用户是否已开启接管 */
  enabled: boolean;
  /** 本地反代监听端口 */
  port: number;
  /** 本地反代是否正在监听 */
  proxy_active: boolean;
  proxy_error: string | null;
  /** TraeWork 当前是否在运行 */
  trae_running: boolean;
  /** 是否找到 TraeWork 安装目录（false = 本机不支持接管） */
  supported: boolean;
  app_dir: string | null;
  /** 端点覆盖文件是否存在 */
  installed: boolean;
  /** 覆盖文件是否由本助手写入 */
  ours: boolean;
  /** 安装目录是否可写 */
  writable: boolean;
  /** 本机反代基址 */
  http_base: string;
  upstream_http: string | null;
  upstream_ws: string | null;
  lease_fresh: boolean;
  message: string;
}
