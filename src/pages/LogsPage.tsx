import { memo, useEffect, useState } from "react";
import { getLogs, clearLogs } from "../api";
import type { LogEntry } from "../types";

function LogsPage() {
  const [logs, setLogs] = useState<LogEntry[]>([]);

  const load = () => getLogs().then(setLogs).catch(() => {});
  useEffect(() => {
    load();
  }, []);

  return (
    <section className="card">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h2>签到日志</h2>
        <button
          className="danger ghost"
          onClick={async () => {
            await clearLogs();
            load();
          }}
        >
          清空
        </button>
      </div>
      <div className="logs">
        {logs.length === 0 ? (
          <div className="muted">暂无日志</div>
        ) : (
          logs.map((l, i) => (
            <div className="log-line" key={i}>
              <span className="muted">{l.at}</span>{" "}
              <span className={l.success ? "ok" : "bad"}>{l.account}</span> {l.message}
            </div>
          ))
        )}
      </div>
    </section>
  );
}

export default memo(LogsPage);
