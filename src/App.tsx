import { useCallback, useEffect, useState } from "react";
import { getSettings, saveSettings } from "./api";
import type { Page, Settings } from "./types";
import AccountsPage from "./pages/AccountsPage";
import GatewayPage from "./pages/GatewayPage";
import LogsPage from "./pages/LogsPage";
import SettingsPage from "./pages/SettingsPage";

const NAV: [Page, string][] = [
  ["accounts", "账号与签到"],
  ["gateway", "智能接管"],
  ["logs", "签到日志"],
  ["settings", "设置"],
];

export default function App() {
  const [page, setPage] = useState<Page>("accounts");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [toast, setToast] = useState("");

  const notify = useCallback((msg: string) => {
    setToast(msg);
    window.setTimeout(() => setToast(""), 1800);
  }, []);

  useEffect(() => {
    getSettings().then(setSettings).catch(() => {});
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
    <>
      <header className="top">
        <div>
          <h1>TraeWork Assistant</h1>
          <span className="sub">多账号签到 · 智能接管</span>
        </div>
      </header>

      <div className="shell">
        <nav className="sidenav">
          {NAV.map(([k, label]) => (
            <button
              key={k}
              className={page === k ? "active" : ""}
              onClick={() => setPage(k)}
            >
              {label}
            </button>
          ))}
        </nav>

        <main>
          {page === "accounts" && <AccountsPage notify={notify} />}
          {page === "gateway" && <GatewayPage settings={settings} update={update} />}
          {page === "logs" && <LogsPage />}
          {page === "settings" && <SettingsPage settings={settings} update={update} />}
        </main>
      </div>

      <div className={`toast${toast ? " show" : ""}`}>{toast}</div>
    </>
  );
}
