import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Activity,
  ArrowUpRight,
  ChevronRight,
  Database,
  KeyRound,
  Mail,
  Send,
  Webhook,
  HardDrive,
  AlertTriangle,
  Pause,
  Play,
  CheckCircle2,
} from "lucide-react";
import { useApi } from "../api";
import { bytes, enc, text, time, type Page, type Row } from "../lib";
import {
  Badge,
  DateFilters,
  Detail,
  Filters,
  FilterSelect,
  Heading,
  Inspect,
  Json,
  LoadState,
  Meter,
  Pagination,
  Section,
  tone,
  Table,
  useFilters,
} from "../components/shared";
function uptime(sec: number) {
  const d = Math.floor(sec / 86400);
  const h = Math.floor((sec % 86400) / 3600);
  const m = Math.floor((sec % 3600) / 60);
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
}
function age(sec: number | null | undefined) {
  if (sec === null || sec === undefined) return null;
  return sec < 120 ? `${sec}s` : sec < 7200 ? `${Math.round(sec / 60)}m` : `${Math.round(sec / 3600)}h`;
}
export function Overview() {
  const q = useApi("/overview");
  const r = q.data;
  const counts: Record<string, number> = r?.mailboxes_by_state || {};
  const total = Object.values(counts).reduce((a, v) => a + Number(v), 0);
  const healthy = Object.entries(counts)
    .filter(([k]) => tone(k) === "good")
    .reduce((a, [, v]) => a + Number(v), 0);
  const segs = Object.entries(counts)
    .filter(([, v]) => Number(v) > 0)
    .sort(([a], [b]) => order(a) - order(b));
  return (
    <>
      <Heading
        title="Overview"
        description="A current view of your mail infrastructure."
      >
        <span className="updated">
          <i />
          Live · refreshes every 10s
        </span>
      </Heading>
      <LoadState loading={q.isPending} error={q.error}>
        {r && (
          <>
            {!r.ready && (
              <div className="error" role="alert">
                <AlertTriangle size={16} />
                Rustbox is not ready. Database {r.database}.
              </div>
            )}
            <div className="metric-grid overview-metrics">
              <Link className="metric" to="/mailboxes">
                <div className="metric-top">
                  <label>Healthy mailboxes</label>
                  <Mail size={16} />
                </div>
                <strong>
                  {healthy} <small className="muted">of {total}</small>
                </strong>
                <div className="meter" role="img" aria-label={segs.map(([k, v]) => `${v} ${k}`).join(", ")}>
                  {segs.map(([k, v]) => (
                    <span
                      key={k}
                      className={meterTone(k)}
                      style={{ width: `${(Number(v) / Math.max(1, total)) * 100}%` }}
                    />
                  ))}
                </div>
                <small>
                  {segs.map(([k, v]) => `${v} ${k.replaceAll("_", " ")}`).join(" · ") || "No mailboxes yet"}
                </small>
              </Link>
              <Link className="metric" to="/outgoing?status=queued">
                <div className="metric-top">
                  <label>Queued sends</label>
                  <Send size={16} />
                </div>
                <strong>{Number(r.sends_queued).toLocaleString()}</strong>
                <small>
                  {r.sends_unknown > 0 ? (
                    <span className="badge warn">
                      <i />
                      {r.sends_unknown} unknown
                    </span>
                  ) : (
                    "No uncertain outcomes"
                  )}
                </small>
              </Link>
              <Link className="metric" to="/events?tab=deliveries">
                <div className="metric-top">
                  <label>Webhook backlog</label>
                  <Webhook size={16} />
                </div>
                <strong>{Number(r.webhook_backlog).toLocaleString()}</strong>
                <small>Pending and delivering</small>
              </Link>
              <Link className="metric" to="/system">
                <div className="metric-top">
                  <label>Content cache</label>
                  <HardDrive size={16} />
                </div>
                <strong>{bytes(r.cache_bytes)}</strong>
                <Meter value={r.cache_bytes} max={r.cache_capacity_bytes} />
                <small>
                  of {bytes(r.cache_capacity_bytes)} · {r.content_pending} pending
                </small>
              </Link>
            </div>
            <div className="cols">
              <Section
                title="Needs attention"
                flush
                aside={
                  r.attention?.length ? (
                    <span className="badge bad">
                      <i />
                      {r.attention.length}
                    </span>
                  ) : undefined
                }
              >
                {r.attention?.length ? (
                  r.attention.map((a: Row) => {
                    const t = tone(a.state || a.error) || "warn";
                    const Icon =
                      a.kind === "mailbox" ? Mail : a.kind === "credential" ? KeyRound : Send;
                    return (
                      <Link
                        className="attention-row"
                        key={a.kind + a.id}
                        to={
                          a.kind === "mailbox"
                            ? "/mailboxes/" + enc(a.id)
                            : a.kind === "credential"
                              ? "/credentials?q=" + enc(a.id)
                              : "/outgoing/" + enc(a.id)
                        }
                      >
                        <span className={`ico-box ${t === "bad" ? "bad" : "warn"}`}>
                          <Icon size={16} />
                        </span>
                        <div>
                          <strong>{a.name || a.id}</strong>
                          <small className={a.error ? "mono" : ""}>{a.error || a.kind}</small>
                        </div>
                        {a.state && <Badge value={a.state} />}
                        <ChevronRight size={16} />
                      </Link>
                    );
                  })
                ) : (
                  <div className="empty">
                    <CheckCircle2 size={28} />
                    <strong>All clear</strong>
                    <span>No mailboxes, credentials or sends need action.</span>
                  </div>
                )}
              </Section>
              <Section title="Runtime and freshness" flush>
                <div className="freshness">
                  <span className={`ico-box ${r.ready ? "good" : "bad"}`}>
                    <Database size={16} />
                  </span>
                  <div>
                    <strong>Database {r.database}</strong>
                    <p>
                      v{r.version} · up {uptime(r.uptime_seconds)}
                    </p>
                  </div>
                </div>
                <div className="freshness">
                  <span className="ico-box">
                    <Activity size={16} />
                  </span>
                  <div>
                    <strong>Native providers</strong>
                    <p>
                      {age(r.native_sync_age_seconds)
                        ? `Oldest sync ${age(r.native_sync_age_seconds)} ago`
                        : "No successful sync recorded"}
                    </p>
                  </div>
                </div>
                <div className="freshness">
                  <span className="ico-box">
                    <Mail size={16} />
                  </span>
                  <div>
                    <strong>IMAP checkpoints</strong>
                    <p>
                      {age(r.imap_checkpoint_age_seconds)
                        ? `Oldest checkpoint ${age(r.imap_checkpoint_age_seconds)} ago`
                        : "No checkpoints recorded"}
                    </p>
                  </div>
                </div>
                <div className="freshness">
                  <div>
                    <div className="provider-counts">
                      {Object.entries(r.mailboxes_by_provider || {}).map(([k, v]) => (
                        <span key={k}>
                          <b>{text(v)}</b> {k}
                        </span>
                      ))}
                    </div>
                    <p>Observed {time(r.observed_at)}</p>
                  </div>
                </div>
              </Section>
            </div>
            <section className="frame">
              <div className="frame-head">
                <h2>Recent events</h2>
                <Link className="btn btn-ghost btn-sm" to="/events">
                  View all <ArrowUpRight size={14} />
                </Link>
              </div>
              <div className="panel flush">
                {r.recent_events?.length ? (
                  <Table headers={["Event", "Type", "Mailbox", "Observed", ""]}>
                    {r.recent_events.map((e: Row) => (
                      <tr key={e.id}>
                        <td className="mono muted">#{e.id}</td>
                        <td>
                          <span className="event-type">{e.type}</span>
                        </td>
                        <td>
                          <Link to={"/mailboxes/" + enc(e.account_id)}>
                            {e.account_name || e.account_id.slice(0, 12)}
                          </Link>
                        </td>
                        <td className="muted nowrap">{time(e.observed_at)}</td>
                        <td>
                          <Inspect title={"Event #" + e.id} value={e} />
                        </td>
                      </tr>
                    ))}
                  </Table>
                ) : (
                  <p className="muted padded">No business events yet.</p>
                )}
              </div>
            </section>
          </>
        )}
      </LoadState>
    </>
  );
}
const STATE_ORDER = ["good", "pending", "warn", "", "bad"];
const order = (k: string) => STATE_ORDER.indexOf(tone(k));
const meterTone = (k: string) =>
  ({ good: "m-success", pending: "m-info", warn: "m-warning", bad: "m-destructive", "": "m-neutral" })[
    tone(k)
  ];
export function Logs() {
  const { params, set, query } = useFilters();
  const [live, setLive] = useState(true);
  const q = useApi<Page & Row>(
    "/logs?" + query({ search: params.get("q") || "" }),
    true,
    live ? 2000 : false,
  );
  return (
    <>
      <Heading
        title="Operational logs"
        description="Sanitized service activity, retained across restarts."
      >
        <span className="switch-label">
          <button
            className={live ? "switch on" : "switch"}
            role="switch"
            aria-checked={live}
            aria-labelledby="follow-label"
            onClick={() => {
              setLive(!live);
              set("page", "1");
            }}
          />
          <span id="follow-label">Follow new records</span>
        </span>
      </Heading>
      <section className="frame" aria-label="Retention">
        <div className="metric-grid g4" style={{ border: 0, padding: 0 }}>
          <div className="metric">
            <label>
              {live ? <Play size={14} className="ic-success" /> : <Pause size={14} />}
              Live tail
            </label>
            <strong style={{ fontSize: 18, lineHeight: "24px" }}>
              {live ? "Every 2s" : "Paused"}
            </strong>
          </div>
          <div className="metric">
            <label>Retention</label>
            <strong style={{ fontSize: 18, lineHeight: "24px" }}>
              {q.data?.retention_days || 7} days
            </strong>
          </div>
          <div className="metric">
            <label>Payload limit</label>
            <strong style={{ fontSize: 18, lineHeight: "24px" }}>
              {bytes(q.data?.capacity_bytes || 1073741824)}
            </strong>
          </div>
          <div className="metric">
            <label>Dropped records</label>
            <strong style={{ fontSize: 18, lineHeight: "24px" }}>
              {q.data?.dropped_records || 0}
            </strong>
          </div>
        </div>
        <div className="frame-foot">
          {q.data?.oldest_available_at
            ? "Oldest available " + time(q.data.oldest_available_at) + ". "
            : ""}
          Records older than these limits are unavailable.
        </div>
      </section>
      {q.isError && (
        <div className="error" role="alert">
          Log updates interrupted. The service will retry automatically while
          live tail is enabled.
        </div>
      )}
      <div className="panel">
        <Filters>
          <FilterSelect
            name="level"
            label="All severities"
            options={["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]}
          />
          <input
            aria-label="Component"
            placeholder="Component"
            value={params.get("component") || ""}
            onChange={(e) => set("component", e.target.value)}
          />
          <DateFilters />
        </Filters>
        <div className="filters secondary">
          {["account_id", "send_id", "sync_id"].map((k) => (
            <input
              key={k}
              aria-label={k.replace("_", " ")}
              placeholder={k.replace("_", " ")}
              value={params.get(k) || ""}
              onChange={(e) => set(k, e.target.value)}
            />
          ))}
        </div>
        <LoadState
          loading={q.isPending}
          error={q.error}
          empty={!q.data?.items.length}
        >
          <Table
            headers={[
              "Time",
              "Severity",
              "Component",
              "Message",
              "Correlation",
              "Fields",
            ]}
          >
            {q.data?.items.map((r) => (
              <tr key={r.id}>
                <td className="nowrap mono">
                  {time(r.observed_at || r.created_at)}
                </td>
                <td>
                  <Badge value={r.level} />
                </td>
                <td className="mono">{r.target || r.component}</td>
                <td className="log-message">{r.message}</td>
                <td>
                  {r.account_id && (
                    <Link to={"/mailboxes/" + enc(r.account_id)}>
                      Mailbox {r.account_id.slice(0, 8)}
                    </Link>
                  )}
                  {r.send_id && (
                    <Link to={"/outgoing/" + enc(r.send_id)}>
                      Send {r.send_id.slice(0, 8)}
                    </Link>
                  )}
                  {r.sync_id && (
                    <span className="mono">Sync {r.sync_id.slice(0, 8)}</span>
                  )}
                </td>
                <td>
                  <Inspect title="Structured log record" value={r} />
                </td>
              </tr>
            ))}
          </Table>
        </LoadState>
        <Pagination data={q.data} />
      </div>
      <p className="muted footnote">
        Live tail refreshes the current page every 2 seconds while this browser
        tab is visible. Business events are available in Events & webhooks.
      </p>
    </>
  );
}
export function System() {
  const q = useApi("/system");
  const r = q.data;
  return (
    <>
      <Heading
        title="System"
        description="Effective configuration and runtime information. Configuration changes require restart."
      />
      <LoadState loading={q.isPending} error={q.error}>
        {r && (
          <>
            <div className="metric-grid">
              <div className="metric">
                <label>Rustbox version</label>
                <strong>{r.version}</strong>
              </div>
              <div className="metric">
                <label>Database</label>
                <Badge value={r.database_ready ? "ready" : "unavailable"} />
              </div>
              <div className="metric">
                <label>Uptime</label>
                <strong>{uptime(r.uptime_seconds)}</strong>
              </div>
              <div className="metric">
                <label>Dropped logs</label>
                <strong>{r.log_dropped_records || 0}</strong>
              </div>
            </div>
            <div className="two-column">
              <Section title="Workers">
                <Detail value={r.workers || {}} />
              </Section>
              <Section title="Content cache">
                <Detail
                  value={{
                    capacity: bytes(r.cache?.capacity_bytes),
                    retention_days: r.cache?.retention_days,
                    max_message_size: bytes(r.cache?.max_message_bytes),
                  }}
                />
              </Section>
              <Section title="Native providers">
                <Detail value={r.native || {}} />
              </Section>
              <Section title="Applications">
                <p className="muted">
                  Configured application names. Every API key has administrative
                  access.
                </p>
                <div className="row" style={{ flexWrap: "wrap", gap: 6 }}>
                  {r.applications?.map((a: string) => (
                    <span className="badge outline mono" key={a}>
                      {a}
                    </span>
                  ))}
                </div>
              </Section>
            </div>
            <Section title="Provider registrations">
              <Table
                headers={["Registration", "Provider", "Mode", "Redirect URIs"]}
              >
                {r.providers?.map((p: Row) => (
                  <tr key={p.id}>
                    <td>{p.id}</td>
                    <td>{p.provider}</td>
                    <td>{p.mode}</td>
                    <td>{p.redirect_uris?.join(", ") || "—"}</td>
                  </tr>
                ))}
              </Table>
            </Section>
            <div className="two-column">
              <Section title="Mail & webhooks">
                <Detail value={{ mail: r.mail, webhooks: r.webhooks }} />
              </Section>
              <Section title="Administration">
                <Json value={r.admin} />
              </Section>
            </div>
            <Section title="Developer resources">
              <div className="resource-links">
                <a href="/openapi.json" target="_blank" rel="noreferrer">
                  OpenAPI specification ↗
                </a>
                <code>/metrics</code>
                <span className="muted">
                  Metrics require bearer authentication with an API key.
                </span>
              </div>
            </Section>
          </>
        )}
      </LoadState>
    </>
  );
}
