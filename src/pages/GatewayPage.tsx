import { useEffect, useState } from "react";
import {
  getGatewayStatus,
  takeoverModel,
  getTakeoverStatus,
  releaseTakeover,
} from "../api";
import type { GatewayStatus, TakeoverStatus, Settings } from "../types";

interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
}

export default function GatewayPage({ settings, update }: Props) {
  const port = settings?.gateway_port ?? 8788;
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [inj, setInj] = useState<TakeoverStatus | null>(null);
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

  // 接管状态加载
  const loadInj = () => {
    getTakeoverStatus().then(setInj).catch(() => {});
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

  const matched = !!inj?.matched;
  const baseUrl = inj?.base_url ?? `http://127.0.0.1:${port}/v1/chat/completions`;
  const selectedCount = (inj?.entries ?? []).filter((e) => e.selected).length;
  const entryCount = (inj?.entries ?? []).length;

  // 开启智能接管 → 自动选中你已添加的网关模型；关闭 → 仅清空选中，绝不删你的模型。
  const onToggleGateway = async (checked: boolean) => {
    setBusy(true);
    setNote(null);
    try {
      update({ gateway_enabled: checked });
      if (checked) {
        const r = await takeoverModel();
        if (r.matched) {
          setNote(`✔ ${r.message}`);
        } else {
          setNote(`⚠ ${r.message}`);
        }
      } else {
        const n = await releaseTakeover();
        setNote(
          n > 0
            ? `✔ 已清空 ${n} 个 state.vscdb 的网关选中（你的自定义模型保留不变）。`
            : "✔ 已关闭本地网关。",
        );
      }
      loadInj();
    } catch (e) {
      setNote(`✖ ${e}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <h2>智能接管（池化网关）</h2>
      <p className="muted" style={{ marginTop: 0 }}>
        本助手<b>不替你写模型进 TraeWork</b>（SOLO CN 的模型列表由服务端权威下发，本地写入约 2
        秒即被覆盖、永不可见）。正确链路是：<b>你先在 TraeWork 手动添加一次</b>指向本机网关的自定义模型，
        本助手负责<b>启停网关 + 自动选中</b>它。聊天请求经本地网关按账号池自动选号、会话粘滞，遇限流无感切换。
      </p>
      <div className="row">
        <label>
          <input
            type="checkbox"
            checked={!!settings?.gateway_enabled}
            disabled={busy}
            onChange={(e) => onToggleGateway(e.target.checked)}
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

      <h3>第一步：在 TraeWork 添加自定义模型（只需一次）</h3>
      <p className="muted">
        打开 TraeWork：<code>设置 → 模型 → 添加自定义模型（OpenAI 兼容）</code>，
        Base URL 填下面这个地址并保存。保存后它会出现在模型列表里（服务端注册、持久）。
      </p>
      <div className="row">
        <code
          style={{
            background: "var(--bg-soft, #f3f3f5)",
            padding: "4px 8px",
            borderRadius: 6,
          }}
        >
          {baseUrl}
        </code>
      </div>

      <h3>第二步：开启上方开关，助手自动选中</h3>
      <div className="row" style={{ gap: 8 }}>
        {inj?.trae_running && <span className="tag bad">TraeWork 运行中</span>}
        {matched && entryCount > 0 && (
          <span className="tag ok">
            已匹配 {entryCount} 个入口
            {selectedCount === entryCount
              ? "，全部已选中"
              : `，${selectedCount} 个已选中`}
          </span>
        )}
        {!matched && !inj?.trae_running && (
          <span className="tag off">未检测到自定义模型</span>
        )}
        {!matched && inj?.trae_running && (
          <span className="tag off">请先添加自定义模型</span>
        )}
        {busy && <span className="tag off">处理中…</span>}
      </div>
      {matched && (
        <p className="muted" style={{ marginTop: 4 }}>
          目标：{baseUrl}。开启开关后，重启 TraeWork 即可在模型选择器看到该模型为已选中。
        </p>
      )}
      {!matched && (
        <p className="muted" style={{ marginTop: 4 }}>
          尚未检测到指向该地址的自定义模型。先按第一步添加并保存，再开启上方开关，助手会自动选中它。
        </p>
      )}
      {note && <p className="muted" style={{ marginTop: 8 }}>{note}</p>}
    </section>
  );
}
