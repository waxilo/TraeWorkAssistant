import { useState } from "react";
import {
  discoverLocal,
  importAccounts,
  oauthStart,
  oauthPoll,
  openExternal,
} from "../api";
import type { Account } from "../types";

type Tab = "scan" | "browser";

interface Props {
  onClose: () => void;
  onImported: (accounts: Account[]) => void;
  notify: (msg: string) => void;
  setStatus: (s: string) => void;
}

export default function AddAccountModal({ onClose, onImported, notify, setStatus }: Props) {
  const [tab, setTab] = useState<Tab>("scan");
  const [busy, setBusy] = useState(false);
  const [host, setHost] = useState("https://api.trae.cn");

  const scan = async () => {
    setBusy(true);
    try {
      const found = await discoverLocal();
      if (!found.length) {
        notify("本机未发现新的未导入账号");
        onClose();
      } else {
        const list = await importAccounts(found);
        onImported(list);
        notify(`已导入 ${found.length} 个本机账号`);
        onClose();
      }
    } catch (e) {
      setStatus("扫描失败: " + e);
    } finally {
      setBusy(false);
    }
  };

  // 「浏览器登录」：申请授权 → 打开系统浏览器 → 轮询回调 token → 导入
  const startBrowser = async () => {
    setBusy(true);
    setStatus("");
    try {
      const st = await oauthStart(host.trim() || null);
      await openExternal(st.verification_uri);
      notify("请在浏览器中完成登录");
      // 轮询直到拿到 token（done=true）或报错
      // eslint-disable-next-line no-constant-condition
      for (;;) {
        await new Promise((r) => setTimeout(r, 2000));
        const p = await oauthPoll(st.login_id);
        if (p.done) {
          if (p.error || !p.token) {
            setStatus("登录失败: " + (p.error || "未获取到 token"));
            break;
          }
          const acct: Account = {
            id: "",
            name: p.nickname || p.phone || "浏览器登录账号",
            phone: p.phone ?? null,
            region: p.region ?? null,
            user_id: p.uid ?? null,
            token: p.token,
            refresh_token: p.refresh_token ?? null,
            host: p.host ?? null,
            expires_at: p.expires_at ?? null,
            refresh_expires_at: null,
            device_id: p.device_id ?? null,
            machine_id: p.machine_id ?? null,
            created_at: new Date().toISOString(),
          };
          const list = await importAccounts([acct]);
          onImported(list);
          notify("已添加新账号");
          onClose();
          break;
        }
      }
    } catch (e) {
      setStatus("浏览器登录失败: " + e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="overlay">
      <div className="modal">
        <h2>添加账号</h2>
        <div className="tabs">
          {(
            [
              ["scan", "扫描本机登录"],
              ["browser", "浏览器登录"],
            ] as [Tab, string][]
          ).map(([k, label]) => (
            <button
              key={k}
              className={`ghost${tab === k ? " active" : ""}`}
              onClick={() => setTab(k)}
            >
              {label}
            </button>
          ))}
        </div>

        {tab === "scan" && (
          <p className="muted">
            扫描本机已登录的 TraeWork 桌面端账号（读取本地加密登录态）并导入。
          </p>
        )}

        {tab === "browser" && (
          <div className="form">
            <p className="muted">
              点击「去登录」，系统会打开浏览器授权页；在页面完成登录后自动回填 token 并导入。
              {" "}
              <strong>注意：</strong>授权端点需逆向桌面端登录包确认后才可用（见后端 oauth.rs）。
            </p>
            <label>
              API Host（可选）
              <input
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="https://api.trae.cn"
              />
            </label>
          </div>
        )}

        <div className="row" style={{ marginTop: 14, justifyContent: "flex-end" }}>
          <button className="ghost" onClick={onClose} disabled={busy}>取消</button>
          {tab === "scan" && <button onClick={scan} disabled={busy}>{busy ? "扫描中…" : "扫描"}</button>}
          {tab === "browser" && <button onClick={startBrowser} disabled={busy}>{busy ? "等待登录…" : "去登录"}</button>}
        </div>
      </div>
    </div>
  );
}
