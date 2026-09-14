import { memo, useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import type { Settings } from "../types";
import { checkAndInstall, type UpdateProgress } from "../updater";
import Switch from "../components/Switch";

interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
  notify: (msg: string) => void;
}

function SettingsPage({ settings, update, notify }: Props) {
  const [version, setVersion] = useState("");
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    getVersion().then(setVersion).catch(() => setVersion(""));
  }, []);

  const onUpdate = async () => {
    if (busy) return;
    setBusy(true);
    setProgress({ status: "checking", message: "正在检查更新…" });
    await checkAndInstall((p) => {
      setProgress(p);
      if (p.status === "error") notify(p.message);
    });
    setBusy(false);
  };

  return (
    <>
      <section className="card">
        <h2>设置</h2>
        <div className="form" style={{ maxWidth: 460 }}>
          <div className="switch-row">
            <Switch
              checked={!!settings?.checkin_enabled}
              onChange={(v) => update({ checkin_enabled: v })}
            />
            <span className="switch-label">启用定时签到</span>
            <span className="switch-hint">
              {settings?.checkin_enabled ? `每天 ${settings?.checkin_time || "10:00"}` : "已停用"}
            </span>
          </div>
          {settings?.checkin_enabled !== false && (
            <label>
              定时签到时刻
              <input
                type="time"
                value={settings?.checkin_time || "10:00"}
                onChange={(e) => update({ checkin_time: e.target.value })}
              />
            </label>
          )}
          <label>
            Webhook 通知地址
            <input
              type="text"
              placeholder="留空则不通知；如 https://notify-hub.../hook/xxx"
              value={settings?.webhook_url || ""}
              onChange={(e) => update({ webhook_url: e.target.value })}
            />
          </label>
        </div>
        <p className="muted">
          应用常驻系统托盘；到达设定时刻自动触发全账号签到（可用上方开关停用）。
          关闭主窗口即隐藏到托盘。
        </p>
      </section>

      <section className="card">
        <h2>关于与更新</h2>
        <div className="form" style={{ maxWidth: 420 }}>
          <div className="muted" style={{ margin: 0 }}>
            当前版本：v{version || "?"}
          </div>
          <div style={{ flexDirection: "row", alignItems: "center", gap: 12 }}>
            <button onClick={onUpdate} disabled={busy}>
              {busy ? "处理中…" : "检查更新"}
            </button>
            {progress && (
              <span className="muted" style={{ margin: 0 }}>
                {progress.message}
              </span>
            )}
          </div>
        </div>
        <p className="muted">
          更新从 GitHub Release 拉取已签名的新版本并自动安装、重启。
          若提示「已经是最新版本」，说明当前已是最新。
        </p>
      </section>
    </>
  );
}

export default memo(SettingsPage);
