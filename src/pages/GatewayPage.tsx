import { useEffect, useState } from "react";
import {
  getGatewayStatus,
  injectModel,
  getInjectionStatus,
  revertInjection,
} from "../api";
import type { GatewayStatus, InjectionStatus, Settings } from "../types";

interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
}

export default function GatewayPage({ settings, update }: Props) {
  const port = settings?.gateway_port ?? 8788;
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [inj, setInj] = useState<InjectionStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  // 网关运行状态轮询
  useEffect(() => {
    let alive = true;
    const load = () => {
      getGatewayStatus().then((s) => alive && setStatus(s)).catch(() => {});
    };
    load();
    const t = window.setInterval(load, 800);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, []);

  // 注入状态加载
  const loadInj = () => {
    getInjectionStatus().then(setInj).catch(() => {});
  };
  useEffect(() => {
    loadInj();
  }, []);

  let tag: "ok" | "bad" | "off" = "off";
  let text = "状态检测中…";
  if (status?.active) {
    tag = "ok";
    text = `已监听 127.0.0.1:${status.port}`;
  } else if (status?.error) {
    tag = "bad";
    text = `启动失败：${status.error}`;
  } else if (status) {
    tag = "off";
    text = "网关未启用（勾选上方开关后自动启动）";
  }

  const onInject = async () => {
    setBusy(true);
    setNote(null);
    try {
      const r = await injectModel();
      if (r.needs_quit) {
        setNote(`⚠ ${r.message}`);
      } else {
        setNote(`✔ ${r.message}`);
        update({ injection_enabled: true });
        loadInj();
      }
    } catch (e) {
      setNote(`✖ ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const onRevert = async () => {
    setBusy(true);
    setNote(null);
    try {
      const n = await revertInjection();
      setNote(`✔ 已还原 ${n} 个 state.vscdb 为注入前状态。`);
      update({ injection_enabled: false });
      loadInj();
    } catch (e) {
      setNote(`✖ ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const injected = (inj?.entries ?? []).flatMap((e) => e.labels);
  const hasInjection = injected.length > 0;

  return (
    <section className="card">
      <h2>智能接管（池化网关）</h2>
      <p className="muted" style={{ marginTop: 0 }}>
        <b>一键注入</b>：帮你在 TraeWork 内置模型里注入「TraePool · 账号池」，无需手动添加自定义模型。
        选中它后，聊天请求经本地网关按账号池自动选号、会话粘滞，遇限流无感切换。
      </p>
      <div className="row">
        <label>
          <input
            type="checkbox"
            checked={!!settings?.gateway_enabled}
            onChange={(e) => update({ gateway_enabled: e.target.checked })}
          />{" "}
          开启本地网关
        </label>
        <span className="muted">端口：</span>
        <input
          type="number"
          style={{ width: 90 }}
          value={port}
          onChange={(e) => update({ gateway_port: parseInt(e.target.value) || 8788 })}
        />
        <span className={`tag ${tag}`}>{text}</span>
      </div>

      <h3>内置模型注入</h3>
      <p className="muted">
        写入 TraeWork 的 <code>state.vscdb</code> 需在 <b>TraeWork 关闭时</b>进行
        （运行时写入会在退出时被覆盖）。若 TraeWork 正在运行，点击会提示先退出。
      </p>
      <div className="row" style={{ gap: 8 }}>
        <button onClick={onInject} disabled={busy}>
          {busy ? "处理中…" : hasInjection ? "重新注入" : "一键注入内置模型"}
        </button>
        {hasInjection && (
          <button onClick={onRevert} disabled={busy} style={{ color: "var(--danger, #d93026)" }}>
            还原为注入前
          </button>
        )}
        {inj?.trae_running && <span className="tag bad">TraeWork 运行中</span>}
        {hasInjection && !inj?.trae_running && <span className="tag ok">已注入 {injected.length} 个入口</span>}
        {!hasInjection && !inj?.trae_running && <span className="tag off">未注入</span>}
      </div>
      {note && <p className="muted" style={{ marginTop: 8 }}>{note}</p>}
      {hasInjection && (
        <p className="muted" style={{ marginTop: 4 }}>
          重启 TraeWork 后，在模型选择器选「TraePool · 账号池」即可走账号池。
          目标：{inj?.entries[0]?.base_url}
        </p>
      )}
    </section>
  );
}
