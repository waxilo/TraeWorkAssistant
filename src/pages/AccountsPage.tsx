import { memo, useMemo, useState } from "react";
import { listAccounts, removeAccount, checkinOne, checkinAll } from "../api";
import type { Account, AcctStatus } from "../types";
import AddAccountModal from "./AddAccountModal";

/**
 * 账号与签到页。
 *
 * **数据全部由外层（`App`）持有**：这里只做展示与触发，不做「挂载即拉取」。
 * 原因见 `App.tsx` 顶部说明——页面每次切 tab 都会重新挂载，若把列表状态放在页面内部，
 * 每切一次回来就要重打一遍 `checkin_status`（每个账号两次网络请求），切 tab 会明显发顿。
 */
interface Props {
  accounts: Account[];
  setAccounts: (list: Account[]) => void;
  statuses: Record<string, AcctStatus>;
  statusText: string;
  setStatusText: (t: string) => void;
  /** 由 `App` 实现的一份刷新逻辑（切 tab 后仍可用同一实例，避免每页各写一份） */
  refreshStatus: () => Promise<void>;
  notify: (msg: string) => void;
}

const tokenLabel = (a: Account) =>
  a.token.length > 14 ? `${a.token.slice(0, 6)}…(${a.token.length})` : a.token || "—";

type Credits = {
  /** 剩余可用积分；未知为 null */
  value: number | null;
  /** 不限量 */
  unlimited: boolean;
  /** 实时值没取到，显示的是账号上存的上次已知值 */
  stale: boolean;
  /** 上次已知值的抓取时刻（仅 `stale` 时有意义） */
  at: string;
};

/**
 * 账号**已有积分**（当前剩余可用额度）：实时状态优先，取不到时回落到账号上存的快照。
 *
 * 两个值同源（后端的 `ide_user_ent_usage` 额度用量接口）：后端每拉一轮状态、每走一次接管
 * 都会写回 `account.credit_snapshot`，所以限流 9074 或掉线时这里仍能算上这个账号，
 * 而不是把它当成 0 悄悄少算。
 *
 * ⚠️ 与「今日签到」无关——签到接口返回的 `credits` 是**签到奖励**，不是已有积分。
 */
function creditsOf(a: Account, statuses: Record<string, AcctStatus>): Credits {
  const live = statuses[a.id];
  if (live && (typeof live.credits === "number" || live.unlimited)) {
    return { value: live.credits ?? null, unlimited: !!live.unlimited, stale: false, at: "" };
  }
  const snap = a.credit_snapshot;
  if (snap && (typeof snap.credits === "number" || snap.unlimited)) {
    return {
      value: snap.credits ?? null,
      unlimited: !!snap.unlimited,
      stale: true,
      at: snap.fetched_at || "",
    };
  }
  return { value: null, unlimited: false, stale: false, at: "" };
}

function AccountsPage({
  accounts,
  setAccounts,
  statuses,
  statusText,
  setStatusText,
  refreshStatus,
  notify,
}: Props) {
  const [busy, setBusy] = useState<string | null>(null);
  const [showAdd, setShowAdd] = useState(false);

  const stats = useMemo(() => {
    const total = accounts.length;
    const checked = accounts.filter((a) => statuses[a.id]?.checked_in).length;
    // 总积分 = **账号池里每个账号已有积分之和**（含只能拿到上次已知值的账号）
    let totalCredits = 0;
    let known = 0;
    let unlimited = 0;
    for (const a of accounts) {
      const c = creditsOf(a, statuses);
      if (c.unlimited) unlimited += 1;
      else if (c.value !== null) {
        totalCredits += c.value;
        known += 1;
      }
    }
    const unknown = total - known - unlimited;
    const hint = [
      `${total} 个账号：${known} 个已取到积分`,
      unlimited ? `${unlimited} 个不限量（未计入合计）` : "",
      unknown > 0 ? `${unknown} 个未取到` : "",
    ]
      .filter(Boolean)
      .join("，");
    return { total, checked, totalCredits, unlimited, hint };
  }, [accounts, statuses]);

  const doCheckinOne = async (id: string) => {
    setBusy(id);
    try {
      const r = await checkinOne(id);
      notify(`[${r.already ? "已签" : r.success ? "成功" : "失败"}] ${r.message}`);
      setAccounts(await reloadAccounts());
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
      const ok = res.filter((r) => r.success || r.already).length;
      notify(`签到完成：成功 ${ok}/${res.length}`);
      setAccounts(await reloadAccounts());
    } catch (e) {
      notify("签到失败: " + e);
    } finally {
      setBusy(null);
    }
    refreshStatus();
  };

  const doRemove = async (id: string) => {
    const left = await removeAccount(id);
    setAccounts(left);
    notify("已删除账号");
  };

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
        <div className="stat" title={stats.hint}>
          <div className="num">
            {stats.totalCredits}
            {stats.unlimited > 0 && <span className="plus">+</span>}
          </div>
          <div className="lbl">总积分</div>
        </div>
      </div>
      <div className="status" style={{ margin: "8px 0" }}>{statusText}</div>

      <table>
        <thead>
          <tr>
            <th>名称</th>
            <th>区域</th>
            <th title="该账号在 TraeWork 里的剩余可用积分（额度用量，非签到奖励）">积分</th>
            <th>今日</th>
            <th>Token</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          {accounts.length === 0 ? (
            <tr><td colSpan={6} className="muted">暂无账号，点「添加账号」扫描本机登录或导入登录态。</td></tr>
          ) : (
            accounts.map((a) => {
              const st = statuses[a.id];
              const cr = creditsOf(a, statuses);
              const cellTitle = cr.unlimited
                ? "不限量"
                : cr.stale
                ? `本次未取到，显示上次已知值${cr.at ? `（${cr.at}）` : ""}`
                : undefined;
              return (
                <tr key={a.id}>
                  <td>{a.name}</td>
                  <td>{a.region || "—"}</td>
                  <td title={cellTitle}>
                    {cr.unlimited ? "不限" : cr.value ?? "—"}
                  </td>
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
                      <button onClick={() => doCheckinOne(a.id)} disabled={busy === a.id}>
                        {busy === a.id ? "签到中" : "签到"}
                      </button>
                      <button className="danger ghost" onClick={() => doRemove(a.id)}>删除</button>
                    </div>
                  </td>
                </tr>
              );
            })
          )}
        </tbody>
      </table>

      <p className="muted" style={{ marginTop: 10 }}>
        「积分」是该账号在 TraeWork 里的<b>剩余可用积分</b>，点「刷新状态」重新拉取；取不到时显示上次已知值。
        「智能接管」的选号依据也是它（<b>到期最早优先，其次积分多者</b>）。
      </p>

      {showAdd && (
        <AddAccountModal
          onClose={() => setShowAdd(false)}
          onImported={setAccounts}
          setStatus={setStatusText}
          notify={notify}
        />
      )}
    </section>
  );
}

/** 拉一次最新账号列表（签到会更新服务端状态，列表本身也要重取） */
async function reloadAccounts(): Promise<Account[]> {
  return listAccounts();
}

export default memo(AccountsPage);
