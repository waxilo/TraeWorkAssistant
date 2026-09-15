import { memo, useCallback, useEffect, useMemo, useState } from "react";
import {
  getTakeoverStatus,
  enableTakeover,
  disableTakeover,
  setTakeoverApps,
  takeoverEvents,
  clearTakeoverEvents,
} from "../api";
import type { Account, AppStatus, JournalEvent, TakeoverStatus, Settings } from "../types";
import { mmdd } from "../format";
import Switch from "../components/Switch";

/**
 * 智能接管页。
 *
 * 设计取向：**一个开关 + 两张勾选表**，其余控件都只在「它此刻真的挡着路」时出现。
 *
 * - 改道方式**只有一种：端点改写**（把应用安装目录里 `product.json` 的 `bootConfig`
 *   指向 `http://127.0.0.1:PORT`），端点恒为**明文回环、不需要任何证书**。
 *   曾经的「经系统代理接管」与「端点讲 https + 自签 CA」两条路已整体移除 —— 界面上
 *   因此不再有「改道方式 / 端点模式」这类选择，也没有证书安装/移除。
 * - **接管哪些应用**：本机可能同时装了多个 Trae shell（如 `TRAE SOLO CN` + `Trae CN`），
 *   所以「接管对象」是一个**多选**（`接管应用` 那一行）。语义与「参与扣费」完全一致：
 *   **一个都不勾 = 全部接管**，取消勾选 = 只有勾上的被改道、被重启。
 *   ⚠️ 多选**只在真的多于一个应用时**才有交互（只有一个时那枚 chip 是只读的 ——
 *   全不选会被后端解释成「全部」，点它想说的事根本表达不出来）。
 * - **补丁的生命周期完全跟着选择走**：勾上时后端自动打（开接管第 0 步 / `set_apps` 的增量步）、
 *   取消时后端自动还原（与端点还原合并在同一次重启里）。留一个「还原补丁」按钮只会多出
 *   一个「忘了点」的状态。唯一仍要在这里说的是**打不成的时候** —— 那正是开关灰着的理由。
 * - 开关是唯一的「开着吗」，接管动态是唯一的「刚才发生了什么」，所以「已生效 / 未生效 /
 *   直连官方 / 处理中」这类复述型标签全部去掉。
 * - **接管动态每次打开本页都从空开始**（`clearTakeoverEvents` —— 用户要求）：它回答的是
 *   「这次打开之后发生了什么」，不是历史档案。想留档就趁页面开着别走。
 * - 选号规则、上游地址这类实现细节不进界面 —— 排障看「接管动态」与
 *   `proxy-rules.json`（后者刻意做成热加载：真机对账时要能不重编译地调）。
 * - 文字只在**有东西挡住你**时出现（见 `issue`）：不正常才是需要解释的时刻，
 *   而且是**按应用**各说各的 —— 合成一句话必然要说谎。
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
    // 旧版本的「经系统代理接管」在应用的 `User/settings.json` 里留过回环代理痕迹。
    // 那条路已整体移除（本端点对 CONNECT 一律 405），痕迹留着 = 整应用不可用，
    // 所以每次开机与关接管都会确认清一遍 —— 清干净了也要在这里留个痕，证明它被处理过。
    case "legacy_proxy_clear":
      return { label: "清理旧版痕迹", cls: "off" };
    case "restart_trae":
      return { label: "重启应用", cls: "restart" };
    // 补丁由开关/选择自动打与还原，但这两条记录**必须留着**：它是「应用被改过没有」的唯一实证。
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

/**
 * 单个应用的健康灯。只在**接管开着**时才渲染（关着时它必然全灰，是噪声）：
 * - `ok`   = 端点已由本助手改道到本机，且它的补丁在位；
 * - `warn` = 这个应用有事（后端给了 `message`），或改道丢了 / 不是本助手改的。
 */
function appHealth(a: AppStatus, enabled: boolean): "ok" | "warn" | "idle" {
  if (!a.selected || !enabled) return "idle";
  if (a.message) return "warn";
  return a.installed && a.ours && a.patch.patched ? "ok" : "warn";
}

/** chip 的悬停提示：在改谁、改到哪、有没有问题 —— 排障时要能一眼看到这几样。 */
function appTitle(a: AppStatus, enabled: boolean, multi: boolean): string {
  const parts: string[] = [a.bundle];
  if (a.app_dir && a.app_dir !== a.bundle) parts.push(`目录 ${a.app_dir}`);
  if (a.message) {
    parts.push(a.message);
  } else if (!a.selected) {
    parts.push("未接管");
  } else if (!enabled) {
    parts.push("接管未开启，勾选只决定下次开启时改谁");
  } else {
    parts.push(a.installed && a.ours ? "端点已改道本机" : "尚未改道");
  }
  if (a.upstream_http) parts.push(`上游 ${a.upstream_http}`);
  if (a.running) parts.push("正在运行");
  if (multi) parts.push("点击切换是否接管");
  return parts.join(" · ");
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

  /** 本机发现到的**全部**应用（没勾的也在里面 —— 多选的前提就是能看见它们）。 */
  const apps = st?.apps ?? [];
  const appIds = useMemo(() => new Set(apps.map((a) => a.id)), [apps]);
  const multi = apps.length > 1;

  /**
   * 接管名单。
   *
   * **空列表 = 全部**（与后端 `target::select` 一致，也是「没配置过」的默认状态），
   * 所以界面上把空列表渲染成「全部勾选」；用户一动就把显式列表写进设置。
   * 「全不选」被禁止：那会写回空列表，后端又会退回「全部」，与「没勾的不接管」正好相反。
   * 本机已不存在的 id 在后端会被丢掉，这里也过滤一遍，免得设置里留一条永远勾不亮的名字。
   */
  const picked = useMemo(
    () => (settings?.takeover_apps ?? []).filter((id) => appIds.has(id)),
    [settings, appIds]
  );
  const appOn = useCallback((id: string) => picked.length === 0 || picked.includes(id), [picked]);
  /** 真正在名单里的应用 —— 判据与展示都用它，别在两处各算一遍（必然分叉）。 */
  const active = useMemo(() => apps.filter((a) => appOn(a.id)), [apps, appOn]);

  /**
   * 「生效中」= 开关开着、反代真在监听、且**至少一个**在名单里的应用端点还被改着。
   * 三者缺一都不算生效（应用升级会悄悄把 product.json 换回去）。
   */
  const live = enabled && !!st?.proxy_active && active.some((a) => a.installed);

  /**
   * 开接管会给名单里的应用**逐个**打补丁，任何一个打不成整次开启都会失败（后端回滚已打的）
   * ⇒ 开关能不能点的判据是「名单里**每个**应用都打得成」，不是「至少一个」。
   */
  const blocked = useMemo(
    () => active.filter((a) => !(a.patch.recognized && a.patch.writable)),
    [active]
  );
  const patchReady = active.length > 0 && blocked.length === 0;

  /**
   * 页面上唯一的文字出口。只在「有东西挡住你 / 坏了」时给内容，且**按应用**分条。
   *
   * 为什么必须按应用分：本机可能装了两个 Trae shell，一个能接管、另一个版本不认识 ——
   * 合成一句话必然要说谎，用户也无从知道该点掉哪一个。所以结构是
   * 「一句总结（可选，只装全局性的事）+ 逐条明细（可选，每条都能归属到某个应用）」，
   * 前置条件的话术（补丁 / 端口 / 不可写）直接用后端那句，不在这里重写 —— 两份必然分叉，
   * 而边界恰恰是「补丁没打 / 打不上」这种最容易错的地方。
   */
  const issue = useMemo<{ text: string | null; items: string[] } | null>(() => {
    if (!st) return error ? { text: error, items: [] } : null;
    if (apps.length === 0) {
      return error
        ? { text: error, items: [] }
        : { text: "没有找到可接管的 Trae 应用，本机无法开启接管。", items: [] };
    }

    const items: string[] = [];
    if (st.missing_apps.length > 0) {
      items.push(`接管名单里有本机找不到的应用，已跳过：${st.missing_apps.join("、")}`);
    }
    // 名单里每个应用各一句（后端只在有事时说，正常是空串）。只有一条明细时前缀是废话。
    for (const a of active) {
      if (a.message) items.push(multi ? `${a.label}：${a.message}` : a.message);
    }

    let text: string | null = null;
    if (error) {
      // 失败必须留在页面上（toast 会消失），否则开关弹回去却没说为什么
      text = error;
    } else if (!enabled && blocked.length > 0) {
      // 开关此刻灰着 ⇒ 要说「怎么才能开」。原因已在明细里逐条写清了，这里只给下一步。
      text = "取消勾选上面打不了补丁的应用，就可以接管其余应用。";
    } else if (enabled && !st.proxy_active) {
      text = st.proxy_error ? `本地反代没有在监听：${st.proxy_error}` : "本地反代没有在监听。";
    } else if (enabled && st.rules?.observe_only) {
      text =
        "当前是观察模式：流量已全部经过本机，但一个凭据都还没换 —— 把 proxy-rules.json 的 observe_only 改成 false 才会真正走账号池。";
    }
    if (!text && items.length === 0) return null;
    return { text, items };
  }, [error, st, apps, enabled, blocked, active, multi]);

  /**
   * 参与扣费的账号。语义与「接管应用」同一套：**空 = 全部**，且禁止全不选。
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
   * 改「接管哪些应用」。
   *
   * 必须走后端命令（而不是像账号那样直接写设置）：**开关开着时它要立即生效** ——
   * 后端做增量协调（新勾的补丁 + 改道并重启、取消的还原并重启、没变的一个字节都不碰）。
   * 开关关着时后端只记设置，那就不该说「已生效」。
   *
   * ⚠️ 本地选择**乐观先写**：后端在「打补丁失败」时也会把选择存下来（只回一句失败原因），
   * 所以先写与磁盘一致；真错了也会被接下来的 `load()` 拉回来。
   */
  const toggleApp = async (id: string) => {
    const current = picked.length === 0 ? apps.map((a) => a.id) : picked;
    const next = current.includes(id) ? current.filter((x) => x !== id) : [...current, id];
    if (next.length === 0) {
      notify("至少保留一个应用");
      return;
    }
    // 全部勾上 = 写空名单（后端语义就是「全部」），避免设置里留一份与默认等价的显式名单
    const ids = next.length === apps.length ? [] : next;
    setBusy(true);
    setError(null);
    update({ takeover_apps: ids });
    try {
      const r = await setTakeoverApps(ids);
      setSt(r);
      notify(enabled ? "接管应用已更新" : "已记录，开启接管时生效");
    } catch (e) {
      // 勾不上必须留在页面上：否则「勾了却没接管」会变成一个看不见的状态
      setError(String(e));
    } finally {
      setBusy(false);
      void load();
    }
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
   * 开关。**它是唯一会动别人应用的东西**，两个方向都是「打包动作」：
   * - 开：给名单里的应用逐个打补丁 → 预检 → 起反代 → 改端点 → 重启它们；
   * - 关：还原端点 → 还原补丁 → 重启它们 → 停反代。
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
        notify("已恢复官方直连，补丁也已还原。");
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
      {/* ── 控制区：一个开关 + 接管哪些应用 + 一个端口 + 哪些账号参与扣费。没有别的控件，也没有复述状态的文字 ── */}
      <section className={`card hero${live ? " live" : ""}`}>
        <div className="hero-row">
          <h2>智能接管</h2>
          <Switch
            size="lg"
            checked={enabled}
            disabled={
              busy ||
              apps.length === 0 ||
              // 名单里有应用打不成补丁 ⇒ 开接管必然失败（它第一步就是逐个打补丁），
              // 先在「能开」之前灰掉（原因与下一步见下面那张勾选表和 `issue`）。
              (!enabled && !patchReady)
            }
            title={
              enabled
                ? "关闭接管：恢复官方直连，并还原给这些应用打的免证书补丁（会重启它们）"
                : "开启接管：给勾选的应用打免证书补丁、把端点改到本机反代（会重启它们）"
            }
            onChange={(v) => onToggle(v)}
          />
        </div>

        {apps.length > 0 && (
          <div className="hero-row">
            <span
              className="field-label"
              title={
                multi
                  ? "只接管控勾选的应用；一个都不勾 = 全部接管。开启状态下改动会立即生效（只重启受影响的应用）"
                  : "本机只发现这一个 Trae 应用，没有选择余地"
              }
            >
              接管应用
            </span>
            <div className="chips">
              {apps.map((a) => {
                const on = appOn(a.id);
                return (
                  <button
                    key={a.id}
                    className={"chip" + (on ? " on" : "") + (multi ? "" : " static")}
                    // 只有一个应用时不给点：全不选会被后端解释成「全部」，点了也表达不出别的意思
                    disabled={busy || !multi}
                    title={appTitle(a, enabled, multi)}
                    onClick={() => void toggleApp(a.id)}
                  >
                    {on && <span className="tick">✓</span>}
                    {a.label}
                    {on && enabled && <span className={"app-dot " + appHealth(a, enabled)} />}
                  </button>
                );
              })}
            </div>
          </div>
        )}

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

        {issue && (
          <div className="notice">
            {issue.text && <div>{issue.text}</div>}
            {issue.items.length > 0 && (
              <ul className="notice-list">
                {issue.items.map((t, i) => (
                  <li key={i}>{t}</li>
                ))}
              </ul>
            )}
          </div>
        )}
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
