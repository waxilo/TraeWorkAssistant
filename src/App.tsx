import { useCallback, useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { getSettings, saveSettings, listAccounts, checkinStatus, refreshAccountProfiles } from "./api";
import type { AcctStatus, Account, Page, Settings } from "./types";
import AccountsPage from "./pages/AccountsPage";
import TakeoverPage from "./pages/TakeoverPage";
import LogsPage from "./pages/LogsPage";
import SettingsPage from "./pages/SettingsPage";

/**
 * 应用外壳：左侧导航 + 右侧内容区。
 *
 * ## 为什么「切 tab 卡」——以及这里怎么解决
 *
 * 早期实现把 4 个页面**全部常驻挂载**、只用 CSS 切可见性。那有四个叠加的代价：
 * 1. 每次点导航都会 `setPage` → `App` 重渲染 → **4 个页面全部重渲染**（表格、日志列表
 *    逐行 reconcile，哪怕它们是 `display:none`）；
 * 2. 隐藏页面里的定时器仍在跑（接管页 2s 轮询一次 IPC + JSON 反序列化）；
 * 3. `display:none → block` 会让浏览器重新 layout 整棵子树；
 * 4. 页面切换动画用了 `transform`，给一个很大的子树建了合成层。
 *
 * 现在改成参考实现的做法：**只渲染当前页**（其余页面根本不在 DOM 里，没有 reconcile、
 * 没有轮询、没有 layout），并把**跨页共享的数据上提到这里**（`accounts` / `statuses` /
 * `statusText`）——这样重新挂载一个页面时不会重新拉数据，「切回来」是零成本的。
 *
 * 页面组件都用 `React.memo` 包了：toast 之类的外层状态变化不会再带着整页一起重渲染。
 */
type IconProps = { size?: number };
const ic = (d: string, extra = "") => ({ size = 18 }: IconProps) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none"
    stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"
    dangerouslySetInnerHTML={{ __html: d + extra }} />
);
const AccountsIcon = ic('<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>');
const TakeoverIcon = ic('<path d="M13 2 3 14h9l-1 8 10-12h-9l1-8z"/>');
const LogsIcon = ic('<line x1="8" y1="6" x2="21" y2="6"/><line x1="8" y1="12" x2="21" y2="12"/><line x1="8" y1="18" x2="21" y2="18"/><circle cx="3.5" cy="6" r="1.2"/><circle cx="3.5" cy="12" r="1.2"/><circle cx="3.5" cy="18" r="1.2"/>');
const SettingsIcon = ic('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>');

const NAV: { key: Page; label: string; Icon: (p: IconProps) => JSX.Element }[] = [
  { key: "accounts", label: "账号与签到", Icon: AccountsIcon },
  { key: "takeover", label: "智能接管", Icon: TakeoverIcon },
  { key: "logs", label: "签到日志", Icon: LogsIcon },
  { key: "settings", label: "设置", Icon: SettingsIcon },
];

export default function App() {
  const [page, setPage] = useState<Page>("accounts");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [version, setVersion] = useState("");
  const [toast, setToast] = useState("");
  // ↓ 跨页共享数据（上提到这里，页面重新挂载时不重新拉取）
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [statuses, setStatuses] = useState<Record<string, AcctStatus>>({});
  const [statusText, setStatusText] = useState("");

  const notify = useCallback((msg: string) => {
    setToast(msg);
    window.setTimeout(() => setToast(""), 1800);
  }, []);

  /** 拉取每个账号的签到状态与积分（每个账号一次请求，属于重活，故只在必要时调用） */
  const refreshStatus = useCallback(async () => {
    setStatusText("查询中…");
    try {
      const st = await checkinStatus();
      setStatuses(Object.fromEntries(st.map((s) => [s.id, s])));
      setStatusText(st.length ? `已刷新 ${st.length} 个账号状态` : "未取到任何账号状态");
    } catch (e) {
      setStatusText("查询失败: " + e);
    }
  }, []);

  useEffect(() => {
    getSettings().then(setSettings).catch(() => {});
    getVersion().then(setVersion).catch(() => {});
    listAccounts().then(setAccounts).catch(() => {});
    // 账号资料补全：真昵称（GetUserInfo.ScreenName）与脱敏手机号（NonPlainTextMobile）都只能
    // 从服务端查，所以启动后按需回源一次 —— 把「浏览器登录账号」/手机号当名字这类占位值换成
    // 真名，并给缺手机号的账号补上。不阻塞首屏：先渲染本地列表，拿到结果再覆盖
    // （后端只对资料不全的账号发请求，离线时静默返回原列表）。
    refreshAccountProfiles().then(setAccounts).catch(() => {});
    void refreshStatus();
  }, [refreshStatus]);

  const update = useCallback(
    (patch: Partial<Settings>) => {
      setSettings((prev) => {
        const next = { ...(prev ?? ({} as Settings)), ...patch };
        saveSettings(next).then(setSettings).catch((e) => notify("保存失败: " + e));
        return next;
      });
    },
    [notify]
  );

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-logo">T</div>
          <div className="brand-text">
            <span className="brand-name">TraeWork</span>
            <span className="brand-sub">Assistant</span>
          </div>
        </div>

        <nav className="nav">
          {NAV.map(({ key, label, Icon }) => (
            <button
              key={key}
              className={"nav-item" + (page === key ? " active" : "")}
              onClick={() => setPage(key)}
              title={label}
            >
              <span className="nav-icon"><Icon /></span>
              <span className="nav-label">{label}</span>
              {key === "takeover" && settings?.takeover_enabled && (
                <span className="nav-dot" title="接管生效中" />
              )}
            </button>
          ))}
        </nav>

        <div className="sidebar-foot">
          <span className="dot" />
          <span>v{version || "…"}</span>
        </div>
      </aside>

      <main className="content">
        {/* 只渲染当前页；`key` 让重新进入该页时重播一次淡入动画 */}
        <div className="page" key={page}>
          {page === "accounts" && (
            <AccountsPage
              accounts={accounts}
              setAccounts={setAccounts}
              statuses={statuses}
              statusText={statusText}
              setStatusText={setStatusText}
              refreshStatus={refreshStatus}
              notify={notify}
            />
          )}
          {page === "takeover" && (
            <TakeoverPage
              settings={settings}
              update={update}
              notify={notify}
              accounts={accounts}
            />
          )}
          {page === "logs" && <LogsPage />}
          {page === "settings" && (
            <SettingsPage settings={settings} update={update} notify={notify} />
          )}
        </div>
      </main>

      <div className={`toast${toast ? " show" : ""}`}>{toast}</div>
    </div>
  );
}
