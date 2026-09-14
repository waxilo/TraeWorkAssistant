import type { Settings } from "../types";

interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
}

export default function SettingsPage({ settings, update }: Props) {
  return (
    <section className="card">
      <h2>设置</h2>
      <div className="form" style={{ maxWidth: 420 }}>
        <label style={{ flexDirection: "row", alignItems: "center" }}>
          <input
            type="checkbox"
            checked={!!settings?.checkin_enabled}
            onChange={(e) => update({ checkin_enabled: e.target.checked })}
          />
          启用定时签到
        </label>
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
        <label style={{ flexDirection: "row", alignItems: "center" }}>
          <input
            type="checkbox"
            checked={!!settings?.gateway_enabled}
            onChange={(e) => update({ gateway_enabled: e.target.checked })}
          />
          同步开启本地网关
        </label>
      </div>
      <p className="muted">
        应用常驻系统托盘；到达设定时刻自动触发全账号签到（可用上方开关停用）。
        关闭主窗口即隐藏到托盘。
      </p>
    </section>
  );
}
