import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  discoverLocal,
  importAccounts,
  importFromFile,
  addManualAccount,
  oauthStart,
  oauthPoll,
  openExternal,
} from "../api";
import type { Account } from "../types";

type Tab = "scan" | "file" | "manual" | "browser";

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

  const pickFile = async () => {
    const file = await open({
      multiple: false,
      filters: [{ name: "登录态", extensions: ["json"] }],
    });
    if (!file || typeof file !== "string") return;
    setBusy(true);
    try {
      const list = await importFromFile(file);
      onImported(list);
      notify("已导入外部登录态账号");
      onClose();
    } catch (e) {
      setStatus("导入失败: " + e);
    } finally {
      setBusy(false);
    }
  };

  const [manual, setManual] = useState({ name: "", region: "", token: "" });
  const submitManual = async () => {
    if (!manual.token?.trim()) {
      notify("请粘贴 token");
      return;
    }
    setBusy(true);
    try {
      const list = await addManualAccount({
        name: manual.name || undefined,
        host: host || undefined,
        token: manual.token,
        region: manual.region || undefined,
      });
      onImported(list);
      notify("已添加账号");
      onClose();
    } catch (e) {
      setStatus("添加失败: " + e);
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
            created_at: new Date().toISOString(),
            enabled: true,
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
              ["file", "导入登录态文件"],
              ["browser", "浏览器登录"],
              ["manual", "手动粘贴"],
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

        {tab === "file" && (
          <p className="muted">
            选择任意一份 <code>storage.json</code>（其他机器/拷贝出的登录态）解析并导入。
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

        {tab === "manual" && (
          <div className="form">
            <label>名称（可选）<input value={manual.name} onChange={(e) => setManual({ ...manual, name: e.target.value })} placeholder="如：小号 / 主力" /></label>
            <label>Host（可选）<input value={host} onChange={(e) => setHost(e.target.value)} placeholder="https://api.trae.cn" /></label>
            <label>区域（可选）<input value={manual.region} onChange={(e) => setManual({ ...manual, region: e.target.value })} placeholder="CN" /></label>
            <label>Token（必填）<textarea rows={4} value={manual.token} onChange={(e) => setManual({ ...manual, token: e.target.value })} placeholder="粘贴完整登录令牌" /></label>
          </div>
        )}

        <div className="row" style={{ marginTop: 14, justifyContent: "flex-end" }}>
          <button className="ghost" onClick={onClose} disabled={busy}>取消</button>
          {tab === "scan" && <button onClick={scan} disabled={busy}>{busy ? "扫描中…" : "扫描"}</button>}
          {tab === "file" && <button onClick={pickFile} disabled={busy}>选择文件</button>}
          {tab === "browser" && <button onClick={startBrowser} disabled={busy}>{busy ? "等待登录…" : "去登录"}</button>}
          {tab === "manual" && <button onClick={submitManual} disabled={busy}>添加</button>}
        </div>
      </div>
    </div>
  );
}
