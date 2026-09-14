import { memo, useCallback, useEffect, useMemo, useState } from "react";
import {
  getTakeoverStatus,
  enableTakeover,
  disableTakeover,
  takeoverEvents,
  clearTakeoverEvents,
} from "../api";
import type { JournalEvent, TakeoverStatus, Settings } from "../types";
import Switch from "../components/Switch";

/**
 * 智能接管页。
 *
 * 设计取向：**默认视图只回答两个问题**——「现在开着吗」和「刚才谁在用哪个账号」。
 * 选号规则、上游地址、覆盖文件路径这类实现细节全部收进「技术细节」折叠区：
 * 它们是排查问题时才需要的，常驻会淹掉真正要看的开关与动态。
 */
interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
  notify: (msg: string) => void;
}

/** 接管动态轮询间隔。页面只在被打开时挂载，所以这个定时器不会在后台空转。 */
const POLL_MS = 5000;

/** 事件类型 → 界面标签与配色 */
function eventKind(e: JournalEvent): {
  label: string;
  cls: "on" | "off" | "route" | "failover" | "restart" | "err";
} {
  switch (e.event) {
    case "install":
      return { label: "开启接管", cls: "on" };
    case "uninstall":
      return { label: "关闭接管", cls: "off" };
    case "sweep":
      return { label: "自动恢复", cls: "off" };
    case "restart_trae":
      return { label: "重启 TraeWork", cls: "restart" };
    case "route_start":
      return { label: "开始使用账号", cls: "route" };
    case "failover":
      return { label: "限流切换", cls: "failover" };
    case "proxy_error":
    case "proxy_bad_request":
    case "proxy_stream_error":
    case "proxy_upstream_status":
      return { label: "代理异常", cls: "err" };
    default:
      return { label: "事件", cls: "restart" };
  }
}

function TakeoverPage({ settings, update, notify }: Props) {
  const port = settings?.takeover_port ?? 8788;
  const [st, setSt] = useState<TakeoverStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [feedBusy, setFeedBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

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

  useEffect(() => {
    void load();
    const t = window.setInterval(() => void load(), POLL_MS);
    return () => window.clearInterval(t);
  }, [load]);

  const enabled = !!settings?.takeover_enabled;
  const live = enabled && !!st?.proxy_active && !!st?.installed;

  // 开 = 启动本地反代 → 写 TraeWork 端点覆盖 → 重启 TraeWork；关 = 全部还原并恢复官方直连。
  const onToggle = async (checked: boolean) => {
    setBusy(true);
    setNote(null);
    try {
      if (checked) {
        const r = await enableTakeover();
        setSt(r);
        update({ takeover_enabled: true });
        setNote(`✔ ${r.message}`);
      } else {
        const r = await disableTakeover();
        setSt(r);
        update({ takeover_enabled: false });
        setNote("✔ 已恢复官方直连。");
      }
    } catch (e) {
      setNote(`✖ ${e}`);
    } finally {
      setBusy(false);
      void load();
    }
  };

  const doRefreshFeed = async () => {
    setFeedBusy(true);
    try {
      await load();
    } finally {
      setFeedBusy(false);
    }
  };

  const doClearEvents = async () => {
    try {
      await clearTakeoverEvents();
      setEvents([]);
      notify("接管动态已清空");
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
      <section className="card">
        <div className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
          <h2 style={{ margin: 0 }}>智能接管</h2>
          <div className="row">
            {live ? <span className="tag ok">已生效</span> : <span className="tag off">未生效</span>}
            {st?.trae_running && <span className="tag off">TraeWork 运行中</span>}
          </div>
        </div>

        <div className="switch-row" style={{ marginTop: 12 }}>
          <Switch
            checked={enabled}
            disabled={busy || !st?.supported || (!st?.writable && !enabled)}
            onChange={(v) => onToggle(v)}
          />
          <span className="switch-label">开启智能接管</span>
          <span className="switch-hint">{enabled ? "全部会话走账号池" : "直连官方"}</span>
        </div>

        <div className="row" style={{ marginTop: 12 }}>
          <span className="muted">端口</span>
          <input
            type="number"
            style={{ width: 90 }}
            value={port}
            disabled={enabled || busy}
            onChange={(e) => update({ takeover_port: parseInt(e.target.value) || 8788 })}
          />
          {busy && <span className="tag off">处理中…</span>}
          {!st && <span className="tag off">检测中…</span>}
          {st && !st.supported && <span className="tag bad">未找到 TraeWork 安装目录</span>}
          {st?.supported && !st.writable && <span className="tag bad">安装目录不可写</span>}
          {st?.installed && !st.ours && <span className="tag bad">同名文件非本助手写入</span>}
          {enabled && st && !st.proxy_active && <span className="tag bad">反代未在监听</span>}
        </div>

        {(note || st?.message) && (
          <p className="muted" style={{ marginTop: 10 }}>{note ?? st?.message}</p>
        )}

        <details className="tech">
          <summary>技术细节</summary>

          <h3>选号规则</h3>
          <ul className="muted">
            <li>
              <b>会话粘滞</b>：同一会话固定用同一个账号，避免中途换号丢上下文。
            </li>
            <li>
              <b>先用快到期的额度</b>：按「额度到期最早 → 剩余积分多」挑账号，不限量账号排最后。
              积分与到期时间取自账号的额度用量接口，10 分钟缓存一次，可在「账号与签到」页手动刷新。
            </li>
            <li>
              <b>限流无感切换</b>：某账号触发 429 会冷却 10 分钟并自动换号重发，最多换 2 次。
            </li>
          </ul>

          <h3>运行信息</h3>
          <ul className="muted">
            <li>
              本机反代：<code>{st?.http_base ?? `http://127.0.0.1:${port}`}</code>
              {st?.proxy_active ? "（已监听）" : "（未监听）"}
              {st?.proxy_error ? ` · ${st.proxy_error}` : ""}
            </li>
            <li>
              原始上游：<code>{st?.upstream_http ?? "（读取中）"}</code>
              {st?.upstream_ws ? (
                <>
                  {" "}· <code>{st.upstream_ws}</code>
                </>
              ) : null}
            </li>
            <li>
              覆盖文件：
              <code>{st?.app_dir ? `${st.app_dir}/product.desktop.local.json` : "—"}</code>
              {st?.installed ? (st.ours ? "（已写入）" : "（他人文件）") : "（未写入）"}
            </li>
          </ul>

          <h3>注意事项</h3>
          <ul className="muted">
            <li>端点覆盖只在 TraeWork 启动时读取，所以开关会自动重启它。</li>
            <li>
              若本助手异常退出或反代无法监听，下次启动助手会<b>自动删除覆盖</b>恢复官方直连，
              避免把 TraeWork 指向死端口。
            </li>
            <li>
              <code>ws.domain</code>（icube RPC 通道）<b>不覆盖</b>，仍直连官方，以免打断模型管理。
            </li>
          </ul>
        </details>
      </section>

      {/* ── 接管动态：谁在什么时候用了哪个账号 / 有没有被限流换号 / 代理有没有报错 ── */}
      <section className="card">
        <div className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
          <h2 style={{ margin: 0 }}>接管动态</h2>
          <div className="row">
            <button className="ghost" disabled={feedBusy} onClick={doRefreshFeed}>
              {feedBusy ? "刷新中…" : "刷新"}
            </button>
            <button className="danger ghost" disabled={events.length === 0} onClick={doClearEvents}>
              清空
            </button>
          </div>
        </div>

        {events.length === 0 ? (
          <p className="muted" style={{ marginTop: 10 }}>
            暂无动态。开启接管并产生对话后，这里会记录开关、账号选用与限流切换。
          </p>
        ) : (
          <ul className="evt-list">
            {grouped.map(({ e, count }, i) => {
              const k = eventKind(e);
              return (
                <li
                  key={`${e.at_ms}-${i}`}
                  className={`evt evt-${k.cls}`}
                  title={count > 1 ? `相同事件连续出现 ${count} 次` : undefined}
                >
                  <span className="e-at">{e.at}</span>
                  <span className={`e-tag tag-${k.cls}`}>{k.label}</span>
                  {count > 1 && <span className="e-count">×{count}</span>}
                  <span className="e-detail">{e.detail}</span>
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
