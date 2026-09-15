import { memo, useMemo, useState } from "react";
import { listAccounts, removeAccount, checkinOne, checkinAll } from "../api";
import type { Account, AcctStatus } from "../types";
import { mmdd, stamp, daysUntil } from "../format";
import AddAccountModal from "./AddAccountModal";

/**
 * 账号与签到页。
 *
 * **数据全部由外层（`App`）持有**：这里只做展示与触发，不做「挂载即拉取」。
 * 原因见 `App.tsx` 顶部说明——页面每次切 tab 都会重新挂载，若把列表状态放在页面内部，
 * 每切一次回来就要重打一遍 `checkin_status`（每个账号两次网络请求），切 tab 会明显发顿。
 *
 * ⚠️ **续签没有任何按钮，也永远不会有**（2026-09-15 按用户要求删除）：
 * token 续签由后端后台线程全自动完成（启动即巡、之后每 30 分钟一轮，见 `renew::spawn`），
 * 每次签到之前还会顺手续一次。所以这一页只负责**把续签的结果显示出来** ——
 * 那一列到期时间被推远了，就是它干的活。
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

/** 读 JWT 载荷里的 `exp`（秒）。不是 JWT / 解不出来 / 载荷里没有 → `null`。 */
function jwtExp(token: string): number | null {
  const seg = token.split(".")[1];
  if (!seg) return null;
  try {
    // base64url → base64（换字符表 + 补 padding），再按 UTF-8 解出 JSON
    const b64 = seg.replace(/-/g, "+").replace(/_/g, "/");
    const pad = "=".repeat((4 - (b64.length % 4)) % 4);
    const json = JSON.parse(atob(b64 + pad)) as { exp?: unknown };
    return typeof json.exp === "number" ? json.exp : null;
  } catch {
    return null;
  }
}

/**
 * token 到期时间（毫秒）。**必须与后端 `renew::expiry_ms` 完全同源**：
 * 先读 JWT 载荷里的 `exp`（秒），再退回账号上的 `expires_at`
 * （浏览器登录给毫秒、本机登录态给秒，统一按「小于 1e12 视为秒」归一化）。
 *
 * 两边不同源就会出现「界面说还有 20 小时、后台认为已经进窗口」这种错位；
 * 而这一列现在是「自动续签在不在干活」的**唯一反馈**，所以不容许存在两套算法。
 */
function tokenExpiry(a: Account): number | null {
  const raw = jwtExp(a.token) ?? a.expires_at;
  if (typeof raw !== "number" || raw <= 0) return null;
  return raw < 1_000_000_000_000 ? raw * 1000 : raw;
}

/** 自动续签在「到期前 24 小时」内动手，所以剩余不足 24 小时就算没续上，要提出来。 */
const RENEW_WINDOW_MS = 24 * 3600 * 1000;

type Credits = {
  /** 剩余可用积分；未知为 null */
  value: number | null;
  /** 不限量 */
  unlimited: boolean;
  /** 实时值没取到，显示的是账号上存的上次已知值 */
  stale: boolean;
  /** 上次已知值的抓取时刻（仅 `stale` 时有意义） */
  at: string;
  /** 最早到期时间（毫秒）；未知为 null */
  expiry: number | null;
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
    return {
      value: live.credits ?? null,
      unlimited: !!live.unlimited,
      stale: false,
      at: "",
      expiry: live.earliest_expiry_ms ?? a.credit_snapshot?.earliest_expiry_ms ?? null,
    };
  }
  const snap = a.credit_snapshot;
  if (snap && (typeof snap.credits === "number" || snap.unlimited)) {
    return {
      value: snap.credits ?? null,
      unlimited: !!snap.unlimited,
      stale: true,
      at: snap.fetched_at || "",
      expiry: snap.earliest_expiry_ms ?? null,
    };
  }
  return { value: null, unlimited: false, stale: false, at: "", expiry: null };
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
            <th title="token 全自动续签：到期前 24 小时后台自己动手，界面上没有手动按钮">Token</th>
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
              const days = !cr.unlimited && cr.expiry ? daysUntil(cr.expiry) : null;
              const texp = tokenExpiry(a);
              // 剩余不足 24 小时 = 自动续签没能续上；**已过期**只能重新登录（红），
              // 还没过期但已进窗口是黄 —— 两者要能一眼分开
              const texpExpired = texp !== null && texp <= Date.now();
              const texpSoon = texp !== null && texp - Date.now() < RENEW_WINDOW_MS;
              const credCls = days === null ? "" : days < 0 ? " bad" : days <= 3 ? " warn" : "";
              const texpCls = texpExpired ? " bad" : texpSoon ? " warn" : "";
              const cellTitle = cr.unlimited
                ? "不限量"
                : cr.stale
                ? `本次未取到，显示上次已知值${cr.at ? `（${cr.at}）` : ""}`
                : undefined;
              return (
                <tr key={a.id}>
                  {/* 真昵称 + 脱敏手机号。名称来自服务端 `ScreenName`，形如「用户0044120650」——
                      光看名字认不出是哪个号，手机号才是人认得的标识，所以直接显示而不是藏进 title。 */}
                  <td>
                    <div className="acct">
                      <span>{a.name}</span>
                      {a.phone && <span className="acct-phone">{a.phone}</span>}
                    </div>
                  </td>
                  <td>{a.region || "—"}</td>
                  <td title={cellTitle}>
                    {cr.unlimited ? "不限" : cr.value ?? "—"}
                    {/* 到期时间就是「智能接管先扣谁」的第一排序键，所以直接显示、不埋进 title */}
                    {!cr.unlimited && cr.expiry && (
                      <span
                        className={`sub${credCls}`}
                        title="智能接管优先使用到期最早的积分"
                      >
                        {days !== null && days < 0 ? "已过期" : `${mmdd(cr.expiry)} 到期`}
                      </span>
                    )}
                  </td>
                  <td>
                    {st ? (
                      st.checked_in ? <span className="tag ok">已签</span> : <span className="tag bad">待签</span>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="muted">
                    {tokenLabel(a)}
                    {/* 自动续签的可见证据：到期时间被推远了就是续上了。
                        没有按钮之后，这一列就是唯一的反馈 —— 所以「未知」必须显式说出来，
                        否则会有一个「永远不续、界面上又看不出来」的沉默账号。 */}
                    {texp !== null ? (
                      <span
                        className={`sub${texpCls}`}
                        title={
                          texpExpired
                            ? "token 已过期且自动续签没成功，需要重新登录这个账号"
                            : texpSoon
                            ? "已进入续签窗口，后台会在 30 分钟内续一轮"
                            : "自动续签会持续推远这个时间"
                        }
                      >
                        {texpExpired ? "已过期" : `${stamp(texp)} 到期`}
                      </span>
                    ) : (
                      <span
                        className="sub warn"
                        title="这个 token 不是 JWT、账号上也没有到期时间 —— 后台无从判断何时该续签，只能跳过它（总不能每 30 分钟盲换一次票）。重新登录一次即可恢复自动续签。"
                      >
                        到期未知
                      </span>
                    )}
                  </td>
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
