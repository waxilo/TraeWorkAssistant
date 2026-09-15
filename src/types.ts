export interface Account {
  /** 本地记录 id，**由后端分配**（导入时不要自己造） */
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
 * 「待导入」的账号：只有前端真正知道的部分。
 *
 * `id` 与 `created_at` 属于**服务端职责**（由 `import_accounts` 统一补齐），前端不编：
 * 早期这里硬编码 `id: ""`，结果 `checkin_one(a.id)` / `remove_account(a.id)` /
 * `statuses[a.id]` 全靠 id 匹配，两个空 id 就会永远命中同一个账号。
 */
export type NewAccount = Omit<Account, "id" | "created_at">;

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
  /**
   * 智能接管：端点改写（`product.json` 指向本机明文反代）+ 免证书（不装 CA、不讲 TLS）一体开关。
   *
   * ⚠️ 免证书是**唯一**形态：端点恒为 `http://127.0.0.1:PORT`，前提是目标应用已打过
   * 「免证书补丁」；未打补丁时写明文端点会让它启动即崩。
   */
  takeover_enabled: boolean;
  /** 本地反代监听端口 */
  takeover_port: number;
  /**
   * 接管哪些应用（按应用 id，即 `.app` 名，如 `TRAE SOLO CN` / `Trae CN`）。
   *
   * ⚠️ **空列表 = 全部**（与 `billing_account_ids` 同一套语义，也是「从没配置过」的默认态）
   * —— 所以界面上把空列表渲染成「全部勾选」，且「全不选」被禁止（那会写回空列表、退回全部）。
   * 本机已经不存在的 id 会被后端丢掉，不会留在设置里。
   */
  takeover_apps: string[];
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
 * `install` / `uninstall` / `sweep` / `restart_trae` / `route_start` / `failover` /
 * `proxy_*` / `unbound_session`（会话 id 认不出，扩大换号域的前置探针）
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
  /**
   * 「还有余量的额度包」里最早的到期时间（毫秒）；未知为 null。
   * 「智能接管」选号的第一排序键就是它（先扣快到期的额度）。
   */
  earliest_expiry_ms?: number | null;
  message: string;
}

// `RenewReport` / `RenewSource` 已随「手动续签」一起删除（2026-09-15）：
// 后端不再有 `renew_accounts` 命令，续签结论只从两条路体现 ——
// 账号列表里的到期时间（被推远 = 续上了）与签到日志里的失败记录。

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

/**
 * 接管规则（`proxy-rules.json`，运行时热加载，改完立即生效）。
 *
 * 「哪些请求该换成账号池凭据」是靠实测收敛的，所以这几个旋钮必须能在**不重编译**的前提下调 ——
 * 真机对账时一轮「发消息 → 看扣了谁」只要几十秒。
 */
export interface TakeoverRules {
  /** 只观察、不换凭据（诊断用，也是唯一「绝对弄不坏应用」的形态） */
  observe_only: boolean;
  /** 覆盖内置扣费前缀表（空 = 用内置表；非空 = **完全取代**） */
  swap_http_prefixes: string[];
  /** 是否连 WebSocket 握手里的 `Authorization` 一起换 */
  swap_ws: boolean;
  /** **仅诊断**：强制判为「透传」的前缀，即使内置表命中 */
  never_swap_prefixes: string[];
}

/**
 * 某个应用的主进程「闸门补丁」状态。
 *
 * 补丁把它的 URL pattern 闸门从「只认 https」改成「认任何 scheme」，
 * 于是本地端点可以走**明文回环** —— 免证书模式的唯一前置条件。
 * ⚠️ 打得成与否由 `writable` 决定：macOS「App 管理」(TCC) 会拦住对已签名应用包的修改。
 * ⚠️ 现在**每个应用各有一份**（本机可能装了多个 Trae shell），互不影响。
 */
export interface PatchStatus {
  /** 找到该应用的 `out/main.js` 了吗（false = 这个应用不支持免证书模式） */
  supported: boolean;
  /** 被改的目标文件 */
  target?: string | null;
  /** 目标文件**真能写**吗（TCC / 只读卷只有真写一次才知道） */
  writable: boolean;
  /** 当前是否已打过补丁 */
  patched: boolean;
  /** 版本是否被识别（`false` 时助手**拒绝**打补丁 —— 宁可不禁用证书，也不能把应用弄坏） */
  recognized: boolean;
  /** 扫到的闸门处数（诊断用，正常是 2） */
  gates: number;
  /** 第 3 处补丁点（身份头规则的 pattern 数组）是否在位 */
  identity_patterns: boolean;
  /** 后端给的那句话：能打 / 已打 / 为什么打不了。界面直接显示，不重写。 */
  message: string;
}

/**
 * 本机发现到的**一个** Trae 应用（含未被勾选的）。
 *
 * `id` = `.app` 名（macOS）/ 安装目录名（Windows），同时是设置里的选择键、界面标签与
 * 进程控制句柄 —— 三者同源，不给它加一层会漂的映射。
 */
export interface AppStatus {
  /** 稳定 id（macOS 下 = `.app` 名），也是设置里记录选择用的键 */
  id: string;
  /** 显示名（当前与 id 相同） */
  label: string;
  /** 应用包（macOS）/ 安装目录（Windows）—— 排障时要能一眼看到在改谁 */
  bundle: string;
  app_dir: string;
  /** 是否在接管名单里（名单为空 = 全部 ⇒ 这里恒 `true`） */
  selected: boolean;
  /** 当前是否在运行 */
  running: boolean;
  /** `product.json` 的端点是否已指向本机反代 */
  installed: boolean;
  /** 上述改写是不是本助手写的（只有带标记才敢还原） */
  ours: boolean;
  /** 它的安装目录是否**真能写** —— 不能写就没有任何一步能成 */
  writable: boolean;
  /** 它自己的闸门补丁状态 */
  patch: PatchStatus;
  upstream_http: string | null;
  upstream_ws: string | null;
  /**
   * **只在有事要说时非空**（版本不认识 / 不可写 / 被别人改过 / 端点丢了）。
   * 一切正常时是空串 —— 界面上一行文字都不该出现。
   */
  message: string;
}

/** 智能接管状态（本地反代 + 应用改道）。 */
export interface TakeoverStatus {
  /** 用户是否已开启接管 */
  enabled: boolean;
  /** 本地反代监听端口 */
  port: number;
  /** 本地反代是否正在监听 */
  proxy_active: boolean;
  proxy_error: string | null;
  /** 本机反代端点基址（恒为明文 `http://127.0.0.1:PORT`，只作展示） */
  endpoint_base: string;
  /** 当前生效的接管规则 */
  rules: TakeoverRules;
  /** 端点覆盖租约是否新鲜（反代的心跳） */
  lease_fresh: boolean;
  /** 本机发现到的**全部** Trae 应用（含未勾选的）—— 界面据此渲染「接管应用」多选 */
  apps: AppStatus[];
  /** 接管名单里、但本机已经不存在的 id（应用卸载了 / 改名了） */
  missing_apps: string[];
  /** 总状态那句话（成功时也可以是陈述句；界面只在有东西挡路时才显示） */
  message: string;
}
