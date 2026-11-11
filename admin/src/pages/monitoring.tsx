import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Activity,
  ArrowUpRight,
  Database,
  Mail,
  Send,
  Webhook,
  HardDrive,
  AlertTriangle,
  Terminal,
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
  Pagination,
  Section,
  Table,
  useFilters,
} from "../components/shared";
import { Button } from "../components/ui/button";
export function Overview() {
  const q = useApi("/overview");
  const r = q.data;
  const counts = r?.mailboxes_by_state || {};
  return (
    <>
      <Heading
        eyebrow="OPERATIONS"
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
            <div className="health-strip">
              <div className="health-title">
                <CheckCircle2 size={20} />
                <strong>
                  {r.ready ? "Rustbox is ready" : "Rustbox needs attention"}
                </strong>
              </div>
              <span>
                <Database size={14} />
                Database {r.database}
              </span>
              <span>v{r.version}</span>
              <span>
                Uptime {Math.floor(r.uptime_seconds / 3600)}h{" "}
                {Math.floor((r.uptime_seconds % 3600) / 60)}m
              </span>
              <small>Observed {time(r.observed_at)}</small>
            </div>
            <div className="metric-grid overview-metrics">
              {[
                {
                  label: "Mailboxes",
                  value: Object.values(counts).reduce(
                    (a: number, v) => a + Number(v),
                    0,
                  ),
                  icon: Mail,
                  to: "/mailboxes",
                  hint: Object.entries(counts)
                    .map(([k, v]) => `${v} ${k}`)
                    .join(" · "),
                },
                {
                  label: "Queued sends",
                  value: r.sends_queued,
                  icon: Send,
                  to: "/outgoing?status=queued",
                  hint: `${r.sends_unknown} uncertain outcomes`,
                },
                {
                  label: "Webhook backlog",
                  value: r.webhook_backlog,
                  icon: Webhook,
                  to: "/events?tab=deliveries",
                  hint: "Pending and delivering",
                },
                {
                  label: "Content cache",
                  value: bytes(r.cache_bytes),
                  icon: HardDrive,
                  to: "/system",
                  hint: `of ${bytes(r.cache_capacity_bytes)} · ${r.content_pending} pending`,
                },
              ].map((m) => (
                <Link className="metric" to={m.to} key={m.label}>
                  <div className="metric-top">
                    <label>{m.label}</label>
                    <m.icon size={17} />
                  </div>
                  <strong>{m.value}</strong>
                  <small>{m.hint}</small>
                </Link>
              ))}
            </div>
            <div className="two-column">
              <Section title="Needs attention">
                <LoadState
                  loading={false}
                  error={null}
                  empty={!r.attention?.length}
                >
                  {r.attention?.map((a: Row) => (
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
                      <AlertTriangle size={17} />
                      <div>
                        <strong>{a.name || a.id}</strong>
                        <small>{a.error || a.state}</small>
                      </div>
                      <ArrowUpRight size={16} />
                    </Link>
                  ))}
                </LoadState>
              </Section>
              <Section title="Synchronization freshness">
                <div className="freshness">
                  <Activity size={19} />
                  <div>
                    <strong>Native providers</strong>
                    <p>
                      {r.native_sync_age_seconds === null
                        ? "No successful sync recorded"
                        : `${r.native_sync_age_seconds}s maximum sync age`}
                    </p>
                  </div>
                </div>
                <div className="freshness">
                  <Mail size={19} />
                  <div>
                    <strong>IMAP checkpoints</strong>
                    <p>
                      {r.imap_checkpoint_age_seconds === null
                        ? "No checkpoints recorded"
                        : `${r.imap_checkpoint_age_seconds}s maximum checkpoint age`}
                    </p>
                  </div>
                </div>
                <p className="muted">
                  Connection state and synchronization freshness describe
                  different health signals.
                </p>
                <div className="provider-counts">
                  {Object.entries(r.mailboxes_by_provider || {}).map(
                    ([k, v]) => (
                      <span key={k}>
                        <b>{text(v)}</b> {k}
                      </span>
                    ),
                  )}
                </div>
              </Section>
            </div>
            <Section title="Recent events">
              <Table headers={["Event", "Type", "Mailbox", "Observed", ""]}>
                {r.recent_events?.map((e: Row) => (
                  <tr key={e.id}>
                    <td className="mono">#{e.id}</td>
                    <td>{e.type}</td>
                    <td>
                      <Link to={"/mailboxes/" + enc(e.account_id)}>
                        {e.account_name || e.account_id.slice(0, 12)}
                      </Link>
                    </td>
                    <td>{time(e.observed_at)}</td>
                    <td>
                      <Inspect title={"Event #" + e.id} value={e} />
                    </td>
                  </tr>
                ))}
              </Table>
              {!r.recent_events?.length && (
                <p className="muted padded">No business events yet.</p>
              )}
              <Link className="panel-link" to="/events">
                View all events <ArrowUpRight size={14} />
              </Link>
            </Section>
          </>
        )}
      </LoadState>
    </>
  );
}
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
        <Button
          variant={live ? "default" : "outline"}
          onClick={() => {
            setLive(!live);
            set("page", "1");
          }}
        >
          <Terminal size={15} />
          {live ? "Live tail on" : "Live tail paused"}
        </Button>
      </Heading>
      <div className="notice mb">
        {q.data?.retention_days || 7}-day retention ·{" "}
        {bytes(q.data?.capacity_bytes || 1073741824)} record payload limit ·{" "}
        {q.data?.dropped_records || 0} dropped records
        {q.data?.oldest_available_at
          ? " · Oldest available " + time(q.data.oldest_available_at)
          : ""}
        . Records older than these limits are unavailable.
      </div>
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
                <strong>{Math.floor(r.uptime_seconds / 60)} minutes</strong>
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
                {r.applications?.map((a: string) => (
                  <span className="badge" key={a}>
                    {a}
                  </span>
                ))}
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
