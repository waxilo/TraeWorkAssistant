import { useEffect, useMemo, useState } from "react";
import { listAccounts, removeAccount, toggleAccount, checkinOne, checkinAll, checkinStatus } from "../api";
import type { Account, AcctStatus } from "../types";
import AddAccountModal from "./AddAccountModal";

interface Props {
  notify: (msg: string) => void;
}

const tokenLabel = (a: Account) =>
  a.token.length > 14 ? `${a.token.slice(0, 6)}…(${a.token.length})` : a.token || "—";

export default function AccountsPage({ notify }: Props) {
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [status, setStatus] = useState("");
  const [statuses, setStatuses] = useState<Record<string, AcctStatus>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [showAdd, setShowAdd] = useState(false);

  const load = async () => {
    const list = await listAccounts();
    setAccounts(list);
  };
  useEffect(() => {
    load().catch((e) => notify("加载账号失败: " + e));
    refreshStatus().catch(() => {});
  }, []);

  const refreshStatus = async () => {
    setStatus("查询中…");
    try {
      const st = await checkinStatus();
      setStatuses(Object.fromEntries(st.map((s) => [s.id, s])));
      setStatus(st.length ? `已刷新 ${st.length} 个账号状态` : "");
    } catch (e) {
      setStatus("查询失败: " + e);
    }
  };

  const stats = useMemo(() => {
    const total = accounts.length;
    const checked = accounts.filter((a) => a.enabled && statuses[a.id]?.checked_in).length;
    return { total, checked };
  }, [accounts, statuses]);

  const doCheckinOne = async (id: string) => {
    setBusy(id);
    try {
      const r = await checkinOne(id);
      notify(`[${r.already ? "已签" : r.success ? "成功" : "失败"}] ${r.message}`);
      load();
    } catch (e) {
      notify("签到失败: " + e);
    } finally {
      setBusy(null);
    }
    refreshStatus();
  };

  const doCheckinAll = async () => {
    setBusy("all");
    try {
      const res = await checkinAll();
      const ok = res.filter((r) => r.success).length;
      notify(`签到完成：成功 ${ok}/${res.length}`);
      load();
    } catch (e) {
      notify("签到失败: " + e);
    } finally {
      setBusy(null);
    }
    refreshStatus();
  };

  const doRemove = async (id: string) => {
    const accounts = await removeAccount(id);
    setAccounts(accounts);
    notify("已删除账号");
  };

  const onImported = (list: Account[]) => setAccounts(list);

  return (
    <section className="card">
      <div className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
        <h2>账号与签到</h2>
        <div className="row">
          <button onClick={() => setShowAdd(true)}>添加账号</button>
          <button className="ghost" onClick={refreshStatus}>刷新状态</button>
          <button onClick={doCheckinAll} disabled={!!busy}>
            {busy === "all" ? "签到中…" : "一键全部签到"}
          </button>
        </div>
      </div>

      <div className="stats">
        <div className="stat"><div className="num">{stats.total}</div><div className="lbl">账号总数</div></div>
        <div className="stat"><div className="num">{stats.checked}</div><div className="lbl">今日已签到</div></div>
        <div className="stat"><div className="num">{stats.total - stats.checked}</div><div className="lbl">待签到</div></div>
      </div>
      <div className="status" style={{ margin: "8px 0" }}>{status}</div>

      <table>
        <thead>
          <tr><th>启用</th><th>名称</th><th>区域</th><th>积分</th><th>今日</th><th>Token</th><th>操作</th></tr>
        </thead>
        <tbody>
          {accounts.length === 0 ? (
            <tr><td colSpan={7} className="muted">暂无账号，点「添加账号」扫描本机登录或导入登录态。</td></tr>
          ) : (
            accounts.map((a) => {
              const st = statuses[a.id];
              return (
                <tr key={a.id}>
                  <td>
                    <input
                      type="checkbox"
                      checked={a.enabled}
                      onChange={(e) => toggleAccount(a.id, e.target.checked).then(setAccounts)}
                    />
                  </td>
                  <td>{a.name}</td>
                  <td>{a.region || "—"}</td>
                  <td>{st?.credits ?? "—"}</td>
                  <td>
                    {st ? (
                      st.checked_in ? <span className="tag ok">已签</span> : <span className="tag bad">待签</span>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="muted">{tokenLabel(a)}</td>
                  <td>
                    <div className="row" style={{ gap: 6 }}>
                      {a.enabled && (
                        <button onClick={() => doCheckinOne(a.id)} disabled={busy === a.id}>
                          {busy === a.id ? "签到中" : "签到"}
                        </button>
                      )}
                      <button className="danger ghost" onClick={() => doRemove(a.id)}>删除</button>
                    </div>
                  </td>
                </tr>
              );
            })
          )}
        </tbody>
      </table>

      {showAdd && (
        <AddAccountModal
          onClose={() => setShowAdd(false)}
          onImported={onImported}
          setStatus={setStatus}
          notify={notify}
        />
      )}
    </section>
  );
}
