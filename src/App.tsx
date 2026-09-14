import { useCallback, useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { getSettings, saveSettings } from "./api";
import type { Page, Settings } from "./types";
import AccountsPage from "./pages/AccountsPage";
import GatewayPage from "./pages/GatewayPage";
import LogsPage from "./pages/LogsPage";
import SettingsPage from "./pages/SettingsPage";

type IconProps = { size?: number };
const ic = (d: string, extra = "") => ({ size = 18 }: IconProps) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none"
    stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"
    dangerouslySetInnerHTML={{ __html: d + extra }} />
);
const AccountsIcon = ic('<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>');
const GatewayIcon = ic('<path d="M13 2 3 14h9l-1 8 10-12h-9l1-8z"/>');
const LogsIcon = ic('<line x1="8" y1="6" x2="21" y2="6"/><line x1="8" y1="12" x2="21" y2="12"/><line x1="8" y1="18" x2="21" y2="18"/><circle cx="3.5" cy="6" r="1.2"/><circle cx="3.5" cy="12" r="1.2"/><circle cx="3.5" cy="18" r="1.2"/>');
const SettingsIcon = ic('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>');

const NAV: { key: Page; label: string; Icon: (p: IconProps) => JSX.Element }[] = [
  { key: "accounts", label: "账号与签到", Icon: AccountsIcon },
  { key: "gateway", label: "智能接管", Icon: GatewayIcon },
  { key: "logs", label: "签到日志", Icon: LogsIcon },
  { key: "settings", label: "设置", Icon: SettingsIcon },
];

export default function App() {
  const [page, setPage] = useState<Page>("accounts");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [version, setVersion] = useState("");
  const [toast, setToast] = useState("");

  const notify = useCallback((msg: string) => {
    setToast(msg);
    window.setTimeout(() => setToast(""), 1800);
  }, []);

  useEffect(() => {
    getSettings().then(setSettings).catch(() => {});
    getVersion().then(setVersion).catch(() => {});
  }, []);

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
            </button>
          ))}
        </nav>

        <div className="sidebar-foot">
          <span className="dot" />
          <span>v{version || "…"}</span>
        </div>
      </aside>

      <main className="content">
        {/* 所有页面常驻挂载，仅切换可见性：避免每次切菜单都重挂组件 + 重新拉取数据导致的卡顿 */}
        <section className={"page" + (page === "accounts" ? " active" : "")}>
          <AccountsPage notify={notify} />
        </section>
        <section className={"page" + (page === "gateway" ? " active" : "")}>
          <GatewayPage settings={settings} update={update} />
        </section>
        <section className={"page" + (page === "logs" ? " active" : "")}>
          <LogsPage />
        </section>
        <section className={"page" + (page === "settings" ? " active" : "")}>
          <SettingsPage settings={settings} update={update} notify={notify} />
        </section>
      </main>

      <div className={`toast${toast ? " show" : ""}`}>{toast}</div>
    </div>
  );
}
