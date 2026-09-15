import { memo, useCallback, useEffect, useMemo, useState } from "react";
import {
  getTakeoverStatus,
  enableTakeover,
  disableTakeover,
  takeoverEvents,
  clearTakeoverEvents,
} from "../api";
import type { Account, JournalEvent, TakeoverStatus, Settings } from "../types";
import { mmdd } from "../format";
import Switch from "../components/Switch";

/**
 * 智能接管页。
 *
 * 设计取向：**一个开关管到底**，其余控件都只在「它此刻真的挡着路」时出现。
 *
 * - 改道方式**只有一种：端点改写**（把 TraeWork 安装目录里 `product.json` 的
 *   `bootConfig` 指向 `http://127.0.0.1:PORT`），端点恒为**明文回环、不需要任何证书**。
 *   曾经的「经系统代理接管」与「端点讲 https + 自签 CA」两条路已整体移除 —— 界面上
 *   因此不再有「改道方式 / 端点模式」这类选择，也没有证书安装/移除。
 * - **TraeWork 补丁的生命周期完全跟着开关走**，所以界面上一行都不给：
 *   开接管时后端自动打（`enable_endpoint` 第 0 步）、关接管时后端自动还原（与端点还原
 *   合并在同一次重启里）。留一个「还原补丁」按钮只会多出一个「忘了点」的状态。
 *   唯一仍要在这里说的是**打不成的时候** —— 那正是开关灰着的理由（见 `problem`）。
 * - 开关是唯一的「开着吗」，接管动态是唯一的「刚才发生了什么」，所以「已生效 / 未生效 /
 *   直连官方 / 处理中」这类复述型标签全部去掉。
 * - **接管动态每次打开本页都从空开始**（`clearTakeoverEvents` —— 用户要求）：它回答的是
 *   「这次打开之后发生了什么」，不是历史档案。想留档就趁页面开着别走。
 * - 选号规则、上游地址这类实现细节不进界面 —— 排障看「接管动态」与
 *   `proxy-rules.json`（后者刻意做成热加载：真机对账时要能不重编译地调）。
 * - 文字只在**有东西挡住你**时出现（见 `problem`）：不正常才是需要解释的时刻。
 */
interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
  notify: (msg: string) => void;
  /** 账号池：决定哪些账号可以被接管扣费（未勾选的一律不参与） */
  accounts: Account[];
}

/** 接管动态轮询间隔。页面只在被打开时挂载，所以这个定时器不会在后台空转。 */
const POLL_MS = 5000;

type Kind = "on" | "off" | "route" | "failover" | "restart" | "warn" | "err";

/**
 * 事件类型 → 圆点配色与短标签。
 *
 * 只覆盖**本版本仍会写出**的事件。历史上那几条（`cert_install` / `mode_switch` /
 * `proxy_route_*` / `tunnel_*`）随对应功能一并删掉了：旧的日志文件里可能还留着它们，
 * 落到 `default` 分支显示成通用的「事件」即可，不值得为一个再也产生不了的类型留映射。
 */
function eventKind(e: JournalEvent): { label: string; cls: Kind } {
  switch (e.event) {
    case "install":
      return { label: "开启接管", cls: "on" };
    case "uninstall":
      return { label: "关闭接管", cls: "off" };
    case "sweep":
      return { label: "自动恢复", cls: "off" };
    // 旧版本的「经系统代理接管」在 TraeWork 的 `User/settings.json` 里留过回环代理痕迹。
    // 那条路已整体移除（本端点对 CONNECT 一律 405），痕迹留着 = 整应用不可用，
    // 所以每次开机与关接管都会确认清一遍 —— 清干净了也要在这里留个痕，证明它被处理过。
    case "legacy_proxy_clear":
      return { label: "清理旧版痕迹", cls: "off" };
    case "restart_trae":
      return { label: "重启 TraeWork", cls: "restart" };
    // 补丁由开关自动打/还原，但这两条记录**必须留着**：它是「TraeWork 被改过没有」的唯一实证。
    // 还原失败时也落在这两条上（detail 里写明），所以标签只描述动作、不预判成败。
    case "patch_apply":
      return { label: "打补丁", cls: "on" };
    case "patch_revert":
      return { label: "还原补丁", cls: "off" };
    // 闸门拦下的改写：这是**保护性**拦截（写下去应用会崩），不是接管坏了，但必须让人看见。
    case "install_blocked":
      return { label: "已阻止改道", cls: "err" };
    case "rules_save":
    case "rules_write":
      return { label: "接管规则", cls: "restart" };
    case "route_start":
      return { label: "开始使用账号", cls: "route" };
    case "unbound_session":
      // 「会话 id 认不出」= 不能把这一整段接口换成池账号（换了会串号）。
      // 用异常色，因为它是**必须处理**的配置型问题，不是普通事件。
      return { label: "会话认不出", cls: "err" };
    case "failover":
      return { label: "限流切换", cls: "failover" };
    // 接管**自己**出的问题：连接失败 / 报文非法 / 流中断 —— 这些才叫「代理异常」。
    case "proxy_error":
    case "proxy_bad_request":
    case "proxy_stream_error":
      return { label: "代理异常", cls: "err" };
    // 上游**自己回**的 4xx/5xx：代理只是原样透传并在动态里记一笔，不代表接管有问题。
    // 4xx（尤其 403 / 404）基本都是账号权限或业务状态 ⇒ 用告警色；
    // 5xx 才可能是上游侧故障 ⇒ 保留异常色。
    // ⚠️ 曾与 proxy_error 共用一个红标签，结果「扣费明明成功了，日志却一片红」
    //    （2026-09-15 用户实测困惑点）—— 把「上游说了什么」和「接管坏了」分开显示。
    case "proxy_upstream_status": {
      const m = /返回 (\d{3})/.exec(e.detail);
      const code = m ? Number(m[1]) : 0;
      return code >= 500
        ? { label: "上游故障", cls: "err" }
        : { label: "上游拒绝", cls: "warn" };
    }
    default:
      return { label: "事件", cls: "restart" };
  }
}

/**
 * 时间列只保留**必要的精度**：今天的事件给 `HH:MM:SS`，跨天的才带上 `MM-DD`。
 * （后端 `at` 已是本地时间串，`at_ms` 用来判断是不是今天。）
 */
function shortTime(e: JournalEvent): string {
  const hms = e.at.length >= 19 ? e.at.slice(11, 19) : e.at;
  const isToday = new Date(e.at_ms).toDateString() === new Date().toDateString();
  return isToday ? hms : `${e.at.slice(5, 10)} ${hms.slice(0, 5)}`;
}

/**
 * chip 上的短标签。账号名是「用户0044120650」这种，直接铺开会把一行撑爆，
 * 所以优先用**人认得的尾部数字**：手机号后 4 位 > user_id 后 4 位 > 名称本身。
 */
function shortLabel(a: Account): string {
  const digits = (a.phone ?? "").replace(/\D/g, "");
  if (digits.length >= 4) return `尾号 ${digits.slice(-4)}`;
  const uid = a.user_id ?? "";
  if (uid.length >= 4) return `ID ${uid.slice(-4)}`;
  return a.name.length > 6 ? a.name.slice(0, 6) : a.name;
}

function TakeoverPage({ settings, update, notify, accounts }: Props) {
  const port = settings?.takeover_port ?? 8788;
  const [st, setSt] = useState<TakeoverStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 端口输入的草稿：直接改设置会让「删一位数字」被 parseInt 兜成默认值，输入过程很跳。 */
  const [portDraft, setPortDraft] = useState(String(port));

  useEffect(() => setPortDraft(String(port)), [port]);

  /** 一次拉齐状态 + 接管动态 */
  const load = useCallback(async () => {
    try {
      const [status, ev] = await Promise.all([getTakeoverStatus(), takeoverEvents()]);
      setSt(status);
      setEvents(ev);
    } catch {
      // 状态读取失败不打扰用户：下一次轮询会自愈
    }
  }, []);

  /**
   * 页面每次打开都**先把上次的动态清空**（用户要求：日志只回答「这次打开之后发生了什么」）。
   *
   * 顺序是「先清、再拉」—— 反过来的话，刚拉到的旧记录会先渲染一帧再被清掉（界面闪一下）。
   * 清空失败**不打扰用户**：顶多这一次还留着旧记录，轮询照样能收到新事件，
   * 比「一打开就弹一个错误」轻得多。
   */
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        await clearTakeoverEvents();
      } catch {
        // 清不掉就算了（见上）
      }
      if (alive) await load();
    })();
    const t = window.setInterval(() => void load(), POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, [load]);

  const enabled = !!settings?.takeover_enabled;
  /**
   * 「生效中」= 开关开着、反代真在监听、且 `product.json` 的改写还在。
   * 三者缺一都不算生效（应用升级会悄悄把 product.json 换回去）。
   */
  const live = enabled && !!st?.proxy_active && !!st?.installed;
  /**
   * 免证书补丁**打得成**吗 —— 它现在是**开关能不能打开**的唯一判据（开接管会自动打）。
   * 打不成只有两种成因：版本不认识 / 安装目录不可写（macOS「App 管理」TCC）。
   */
  const patchReady = !!st?.patch.recognized && !!st?.patch.writable;

  /**
   * 页面上唯一的文字出口。只在「有东西挡住你 / 坏了」时给一句，且一次只说最要紧的那条。
   *
   * 前置条件（补丁 / 端口 / 目录不可写）后端分得最清，直接用它的那句 `message`，
   * 不要在这里重写话术——两份必然分叉，而边界恰恰是「补丁没打 / 打不上」这种最容易错的地方。
   * 唯一需要在这里补的是「一切都正常，但你还没开始省额度」——那是观察模式。
   */
  const problem = useMemo(() => {
    if (error) return error;
    if (!st) return null;
    if (!st.supported) return "没有找到 TraeWork 安装目录，本机无法开启接管。";
    // 开关此刻灰着 ⇒ 必须说清「为什么开不了」。这一条必须排在前面，
    // 否则会出现「按钮是灰的、页面上一句话都没有」——比报错更难排查。
    // 补丁探针的话术自带处置办法（版本不认识 / 不可写 + 怎么办），直接用它的。
    if (!enabled && !patchReady) return st.message;
    if (st.installed && !st.ours) {
      return "TraeWork 的端点已指向本机反代，但不是本助手改的——为免误伤，助手不会动它。";
    }
    if (enabled && !st.proxy_active) {
      return st.proxy_error ? `本地反代没有在监听：${st.proxy_error}` : "本地反代没有在监听。";
    }
    if (enabled && st.rules?.observe_only) {
      return "当前是**观察模式**：流量已全部经过本机，但一个凭据都还没换——把 proxy-rules.json 的 observe_only 改成 false 才会真正走账号池。";
    }
    return null;
  }, [error, st, enabled, patchReady]);

  /**
   * 参与扣费的账号。
   *
   * **空列表 = 全部**（与后端 `billing_candidates` 一致，也是「没配置过」的默认状态），
   * 所以界面上把空列表渲染成「全部勾选」；用户一动就把显式列表写进设置。
   * 「全不选」被禁止：那会写回空列表，后端又会退回「全部」，与「没勾的不扣费」正好相反。
   */
  const accountIds = useMemo(() => new Set(accounts.map((a) => a.id)), [accounts]);
  const billingIds = useMemo(
    () => (settings?.billing_account_ids ?? []).filter((id) => accountIds.has(id)),
    [settings, accountIds]
  );
  const billingOn = useCallback(
    (id: string) => billingIds.length === 0 || billingIds.includes(id),
    [billingIds]
  );

  const toggleBilling = (id: string) => {
    const current = billingIds.length === 0 ? accounts.map((a) => a.id) : billingIds;
    const next = current.includes(id) ? current.filter((x) => x !== id) : [...current, id];
    if (next.length === 0) {
      notify("至少保留一个账号");
      return;
    }
    // 全部选回 = 写空列表（后端语义就是「全部」），避免设置里留一份和默认等价的显式名单
    update({ billing_account_ids: next.length === accounts.length ? [] : next });
  };

  /**
   * chip 的悬停提示：给「核对选号依据」用 —— 账号是谁、还剩多少积分、哪天到期。
   * 到期时间是选号的第一排序键（越早到期越先用），所以必须能在这里看到。
   */
  const chipTitle = (a: Account) => {
    const parts = [a.name];
    if (a.phone) parts.push(a.phone);
    const snap = a.credit_snapshot;
    if (snap?.unlimited) {
      parts.push("积分不限量");
    } else if (snap && (snap.credits !== null || snap.earliest_expiry_ms)) {
      const c = snap.credits === null ? "未知" : String(snap.credits);
      parts.push(
        snap.earliest_expiry_ms
          ? `积分 ${c}，${mmdd(snap.earliest_expiry_ms)} 到期`
          : `积分 ${c}，到期未知`
      );
    } else {
      parts.push("积分未知");
    }
    return parts.join(" · ");
  };

  /**
   * 开关。**它是唯一会动 TraeWork 的东西**，两个方向都是「打包动作」：
   * - 开：打补丁 → 预检 → 起反代 → 改端点 → 重启 TraeWork；
   * - 关：还原端点 → 还原补丁 → 重启 TraeWork → 停反代。
   * 所以文案要说清「它会动别人的应用」，别让用户以为只是本机一个开关。
   */
  const onToggle = async (checked: boolean) => {
    setBusy(true);
    setError(null);
    try {
      if (checked) {
        const r = await enableTakeover();
        setSt(r);
        update({ takeover_enabled: true });
        notify(r.message);
      } else {
        const r = await disableTakeover();
        setSt(r);
        update({ takeover_enabled: false });
        notify("已恢复官方直连，TraeWork 补丁也已还原。");
      }
    } catch (e) {
      // 失败必须留在页面上（toast 会消失），否则开关弹回去却没说为什么
      setError(String(e));
    } finally {
      setBusy(false);
      void load();
    }
  };

  /** 端口只在停用时能改；失焦/回车提交，非法值静默回退（输入框里不留半截数字）。 */
  const commitPort = () => {
    const n = parseInt(portDraft, 10);
    if (!Number.isFinite(n) || n < 1024 || n > 65535 || n === port) {
      setPortDraft(String(port));
      return;
    }
    update({ takeover_port: n });
  };

  const doClearEvents = async () => {
    try {
      await clearTakeoverEvents();
      setEvents([]);
    } catch (e) {
      notify("清空失败：" + e);
    }
  };

  /**
   * 连续相同（类型 + 内容都一样）的事件聚合成一条并附次数。
   * 事件流是「新的在前」，相邻即时间连续——重启风暴、心跳重复这类刷屏只会占一行。
   */
  const grouped = useMemo(() => {
    const out: { e: JournalEvent; count: number }[] = [];
    for (const e of events) {
      const last = out[out.length - 1];
      if (last && last.e.event === e.event && last.e.detail === e.detail) {
        last.count += 1;
      } else {
        out.push({ e, count: 1 });
      }
    }
    return out.slice(0, 80);
  }, [events]);

  return (
    <>
      {/* ── 控制区：一个开关 + 一个端口 + 哪些账号参与扣费。没有别的控件，也没有复述状态的文字 ── */}
      <section className={`card hero${live ? " live" : ""}`}>
        <div className="hero-row">
          <h2>智能接管</h2>
          <Switch
            size="lg"
            checked={enabled}
            disabled={
              busy ||
              !st?.supported ||
              // 补丁打不成 ⇒ 开接管必然失败（它第一步就是打补丁），先在能开之前灰掉。
              (!enabled && !patchReady)
            }
            title={
              enabled
                ? "关闭接管：恢复官方直连，并还原给 TraeWork 打的免证书补丁（会重启 TraeWork）"
                : "开启接管：给 TraeWork 打免证书补丁、把端点改到本机反代（会重启 TraeWork）"
            }
            onChange={(v) => onToggle(v)}
          />
        </div>

        <div className="hero-row">
          <span className="field-label">端口</span>
          <input
            className="port-input"
            type="number"
            inputMode="numeric"
            value={portDraft}
            disabled={enabled || busy}
            title={enabled ? "开启接管时端口不可改，先关闭接管" : "本地反代监听端口"}
            onChange={(e) => setPortDraft(e.target.value)}
            onBlur={commitPort}
            onKeyDown={(e) => {
              if (e.key === "Enter") e.currentTarget.blur();
            }}
          />
        </div>

        {accounts.length > 0 && (
          <div className="hero-row">
            <span
              className="field-label"
              title="没勾选的账号一律不参与扣费；在勾选的账号里，积分到期最早的最先被使用"
            >
              参与扣费
            </span>
            <div className="chips">
              {accounts.map((a) => {
                const on = billingOn(a.id);
                return (
                  <button
                    key={a.id}
                    className={"chip" + (on ? " on" : "")}
                    disabled={busy}
                    title={chipTitle(a)}
                    onClick={() => toggleBilling(a.id)}
                  >
                    {on && <span className="tick">✓</span>}
                    {shortLabel(a)}
                  </button>
                );
              })}
            </div>
          </div>
        )}

        {problem && <div className="notice">{problem}</div>}
      </section>

      {/* ── 接管动态：谁在什么时候用了哪个账号 / 有没有被限流换号 / 代理有没有报错。
              ⚠️ 每次打开本页都会先清空，所以这里给的是「这次打开之后」的事。 ── */}
      <section className="card">
        <div className="card-head">
          <h2>接管动态</h2>
          {events.length > 0 && (
            <button className="icon-btn" title="清空动态" onClick={doClearEvents}>
              <svg
                width="15"
                height="15"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.8"
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden="true"
              >
                <path d="M4 7h16M9.5 7V4.8h5V7M6.5 7l1 12.2h9l1-12.2" />
              </svg>
            </button>
          )}
        </div>

        {grouped.length === 0 ? (
          <div className="feed-empty">暂无动态</div>
        ) : (
          <ul className="feed">
            {grouped.map(({ e, count }, i) => {
              const k = eventKind(e);
              return (
                <li
                  key={`${e.at_ms}-${i}`}
                  className={`feed-item k-${k.cls}`}
                  title={count > 1 ? `相同事件连续出现 ${count} 次` : undefined}
                >
                  <span className="feed-dot" />
                  <span className="feed-time">{shortTime(e)}</span>
                  <span className="feed-label">{k.label}</span>
                  {count > 1 && <span className="feed-count">×{count}</span>}
                  <span className="feed-detail">{e.detail}</span>
                </li>
              );
            })}
          </ul>
        )}
      </section>
    </>
  );
}

export default memo(TakeoverPage);
