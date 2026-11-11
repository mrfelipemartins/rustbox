import { useState, type FormEvent } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Plus,
  ArrowLeft,
  Play,
  Pause,
  RefreshCw,
  Trash2,
  Send,
  ChevronRight,
} from "lucide-react";
import { api, operation, useApi } from "../api";
import {
  bytes,
  datetimeLocal,
  enc,
  mailboxAddress,
  safeEmailDocument,
  text,
  time,
  type Page,
  type Row,
} from "../lib";
import {
  Badge,
  Detail,
  ErrorBox,
  Field,
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
import { Modal } from "../components/ui/dialog";
import {
  Tabs,
  TabsList,
  TabsTrigger,
  TabsContent,
} from "../components/ui/tabs";
import { OutgoingList } from "./outgoing";
import { EventsList } from "./events";

export function Mailboxes() {
  const { query } = useFilters();
  const q = useApi<Page>("/mailboxes?" + query());
  const [add, setAdd] = useState(false);
  return (
    <>
      <Heading
        title="Mailboxes"
        description="Connect and monitor the mailboxes powering your integrations."
      >
        <Button onClick={() => setAdd(true)}>
          <Plus size={16} />
          Connect mailbox
        </Button>
      </Heading>
      <div className="panel">
        <Filters>
          <FilterSelect
            name="provider"
            label="All providers"
            options={["google", "microsoft", "imap"]}
          />
          <FilterSelect
            name="state"
            label="All states"
            options={[
              "ready",
              "connected",
              "paused",
              "connecting",
              "disconnected",
              "auth_failed",
            ]}
          />
        </Filters>
        <LoadState
          loading={q.isPending}
          error={q.error}
          empty={!q.data?.items.length}
        >
          <Table
            headers={[
              "Mailbox",
              "Provider",
              "Connection / sync",
              "Authorization",
              "Sending",
              "Last activity",
              "",
            ]}
          >
            {q.data?.items.map((r) => (
              <tr key={r.id}>
                <td>
                  <Link className="record-name" to={`/mailboxes/${enc(r.id)}`}>
                    {r.name}
                  </Link>
                  <small>{mailboxAddress(r)}</small>
                </td>
                <td className="capitalize">{r.backend}</td>
                <td>
                  <Badge value={r.state} />
                </td>
                <td>
                  <Badge
                    value={
                      r.authorization?.health ||
                      r.imap?.authorization?.health ||
                      (r.backend === "imap" ? "password" : "unvalidated")
                    }
                  />
                </td>
                <td>
                  <Badge value={r.sending?.state || r.smtp_state} />
                </td>
                <td>
                  {time(
                    r.synchronization?.last_success_at || r.last_connected_at,
                  )}
                </td>
                <td>
                  <Link
                    className="link"
                    to={`/mailboxes/${enc(r.id)}`}
                    aria-label={`Open ${r.name}`}
                  >
                    <ChevronRight size={18} />
                  </Link>
                </td>
              </tr>
            ))}
          </Table>
        </LoadState>
        <Pagination data={q.data} />
      </div>
      <MailboxForm open={add} onClose={() => setAdd(false)} />
    </>
  );
}

const blankServer = (port: number) => ({
  host: "",
  port,
  username: "",
  password: "",
  tls_mode: "implicit",
});
function initialAccount(r?: Row) {
  return {
    name: r?.name || "",
    backend: r?.backend || "google",
    credential_id: r?.credential_id || "",
    target_mailbox: r?.target_mailbox || "",
    authorizing_identity: r?.authorizing_identity || "",
    shared: r?.shared || false,
    sync_from: r?.sync_from ? datetimeLocal(r.sync_from) : "",
    imap: { ...blankServer(993), ...r?.imap, password: "" },
    smtp: { ...blankServer(465), ...r?.smtp, password: "" },
    watched_folders: (r?.watched_folders || []).join(", "),
    sent_folder: r?.sent_folder || "",
    trash_folder: r?.trash_folder || "",
    webhook_url: r?.webhook_url || "",
    webhook_secret: "",
  };
}
export function MailboxForm({
  open,
  onClose,
  account,
}: {
  open: boolean;
  onClose: () => void;
  account?: Row;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [form, setForm] = useState(() => initialAccount(account));
  const [step, setStep] = useState(0);
  const [key] = useState(() => crypto.randomUUID());
  const [credentialSearch, setCredentialSearch] = useState("");
  const creds = useApi<Page>(
    "/credentials?per_page=100&provider=" +
      enc(form.backend) +
      "&q=" +
      enc(credentialSearch),
    open,
  );
  const update = (k: string, v: unknown) => setForm((f) => ({ ...f, [k]: v }));
  const mutation = useMutation({
    mutationFn: async () => {
      const input = {
        ...form,
        credential_id: form.backend === "imap" ? null : form.credential_id,
        target_mailbox: form.target_mailbox || null,
        authorizing_identity: form.authorizing_identity || null,
        sync_from: account
          ? account.sync_from
          : form.sync_from
            ? new Date(form.sync_from).toISOString()
            : null,
        watched_folders: form.watched_folders
          .split(",")
          .map((s: string) => s.trim())
          .filter(Boolean),
        sent_folder: form.sent_folder || null,
        trash_folder: form.trash_folder || null,
        webhook_url: form.webhook_url || null,
        webhook_secret: form.webhook_secret || null,
      };
      return account
        ? api(`/mailboxes/${enc(account.id)}`, {
            method: "PUT",
            body: JSON.stringify(input),
          })
        : operation("/accounts", "POST", input, { "Idempotency-Key": key });
    },
    onSuccess: (r) => {
      qc.invalidateQueries();
      onClose();
      if (!account) navigate("/mailboxes/" + enc(r.id));
    },
  });
  const labels = [
    "Provider",
    "Credentials",
    "Identity",
    "Synchronization",
    "Webhook",
  ];
  return (
    <Modal
      open={open}
      onOpenChange={(v) => {
        if (!v) onClose();
      }}
      title={account ? "Edit mailbox" : "Connect mailbox"}
      description={
        account
          ? "Leave password and webhook secret fields blank to keep their current values."
          : "Connect a provider, choose synchronization settings, then monitor initialization."
      }
    >
      <div className="steps">
        {labels.map((s, i) => (
          <button
            type="button"
            key={s}
            disabled={i > step && !account}
            onClick={() => setStep(i)}
            className={i === step ? "active" : ""}
          >
            <span>{i + 1}</span>
            {s}
          </button>
        ))}
      </div>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (step < 4) setStep(step + 1);
          else mutation.mutate();
        }}
      >
        <div className="form-body">
          {step === 0 && (
            <>
              <Field label="Provider">
                <select
                  value={form.backend}
                  disabled={!!account}
                  onChange={(e) => update("backend", e.target.value)}
                >
                  <option value="google">Google · Gmail API</option>
                  <option value="microsoft">Microsoft · Graph</option>
                  <option value="imap">Generic IMAP / SMTP</option>
                </select>
              </Field>
              <p className="muted">
                Google and Microsoft use native APIs for synchronization and
                sending. IMAP/SMTP requires verified TLS.
              </p>
            </>
          )}
          {step === 1 &&
            (form.backend === "imap" ? (
              <div className="form-grid">
                {(["imap", "smtp"] as const).map((protocol) => (
                  <Section key={protocol} title={protocol.toUpperCase()}>
                    {(
                      [
                        "host",
                        "port",
                        "username",
                        "password",
                        "tls_mode",
                      ] as const
                    ).map((k) => (
                      <Field key={k} label={k.replace("_", " ")}>
                        {k === "tls_mode" ? (
                          <select
                            value={form[protocol][k]}
                            onChange={(e) =>
                              update(protocol, {
                                ...form[protocol],
                                [k]: e.target.value,
                              })
                            }
                          >
                            <option value="implicit">Implicit TLS</option>
                            <option value="starttls">STARTTLS</option>
                          </select>
                        ) : (
                          <input
                            required={k !== "password" || !account}
                            type={
                              k === "password"
                                ? "password"
                                : k === "port"
                                  ? "number"
                                  : "text"
                            }
                            min={k === "port" ? 1 : undefined}
                            max={k === "port" ? 65535 : undefined}
                            value={form[protocol][k]}
                            autoComplete={
                              k === "password" ? "new-password" : "off"
                            }
                            onChange={(e) =>
                              update(protocol, {
                                ...form[protocol],
                                [k]:
                                  k === "port"
                                    ? Number(e.target.value)
                                    : e.target.value,
                              })
                            }
                          />
                        )}
                      </Field>
                    ))}
                  </Section>
                ))}
              </div>
            ) : (
              <>
                <Field
                  label="Find credential"
                  hint="Search by registration, subject, or ID when the credential is not listed."
                >
                  <input
                    value={credentialSearch}
                    onChange={(e) => setCredentialSearch(e.target.value)}
                    placeholder="Search credentials…"
                  />
                </Field>
                <Field label="OAuth credential">
                  <select
                    required
                    value={form.credential_id}
                    onChange={(e) => update("credential_id", e.target.value)}
                  >
                    <option value="">Select credential</option>
                    {creds.data?.items
                      .filter((c) => !c.provider || c.provider === form.backend)
                      .map((c) => (
                        <option key={c.id} value={c.id}>
                          {c.subject || c.client_id} · {c.id.slice(0, 8)} ·{" "}
                          {c.health}
                        </option>
                      ))}
                  </select>
                </Field>
                <ErrorBox error={creds.error} />
                <p className="muted">
                  Connect or import an authorized credential in{" "}
                  <Link to="/credentials">Credentials</Link> first. Microsoft
                  shared mailboxes require access granted to the authorizing
                  identity; Gmail requires owner access or Workspace
                  impersonation.
                </p>
              </>
            ))}
          {step === 2 && (
            <>
              <Field label="Display name">
                <input
                  required
                  value={form.name}
                  maxLength={255}
                  onChange={(e) => update("name", e.target.value)}
                />
              </Field>
              {form.backend !== "imap" && (
                <>
                  <Field label="Target mailbox">
                    <input
                      required
                      value={form.target_mailbox}
                      disabled={!!account}
                      placeholder="support@example.com"
                      onChange={(e) => update("target_mailbox", e.target.value)}
                    />
                  </Field>
                  <Field label="Authorizing identity (optional)">
                    <input
                      value={form.authorizing_identity}
                      onChange={(e) =>
                        update("authorizing_identity", e.target.value)
                      }
                    />
                  </Field>
                  {form.backend === "microsoft" && (
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={form.shared}
                        onChange={(e) => update("shared", e.target.checked)}
                      />
                      Shared Microsoft mailbox
                    </label>
                  )}
                </>
              )}
            </>
          )}
          {step === 3 && (
            <>
              <Field
                label="Synchronize from"
                hint={
                  account
                    ? "The synchronization start is fixed at creation."
                    : "Inclusive start time in your browser's timezone. Leave empty for the default 30-day window."
                }
              >
                <input
                  type="datetime-local"
                  disabled={!!account}
                  value={form.sync_from}
                  onChange={(e) => update("sync_from", e.target.value)}
                />
              </Field>
              <Field
                label="Watched folders / labels"
                hint="Comma-separated provider folder IDs or IMAP folder names."
              >
                <input
                  value={form.watched_folders}
                  onChange={(e) => update("watched_folders", e.target.value)}
                />
              </Field>
              <div className="form-grid">
                <Field label="Sent folder (optional)">
                  <input
                    value={form.sent_folder}
                    onChange={(e) => update("sent_folder", e.target.value)}
                  />
                </Field>
                <Field label="Trash folder (optional)">
                  <input
                    value={form.trash_folder}
                    onChange={(e) => update("trash_folder", e.target.value)}
                  />
                </Field>
              </div>
            </>
          )}
          {step === 4 && (
            <>
              <Field label="Webhook URL (optional)">
                <input
                  type="url"
                  value={form.webhook_url}
                  placeholder="https://app.example.com/mail-events"
                  onChange={(e) => update("webhook_url", e.target.value)}
                />
              </Field>
              <Field
                label="Webhook signing secret"
                hint={
                  account
                    ? "Leave blank to keep the existing secret."
                    : "At least 32 characters when a webhook URL is provided."
                }
              >
                <input
                  type="password"
                  autoComplete="new-password"
                  minLength={32}
                  required={!!form.webhook_url && !account}
                  value={form.webhook_secret}
                  onChange={(e) => update("webhook_secret", e.target.value)}
                />
              </Field>
              <div className="notice">
                {account
                  ? "Settings are applied to this mailbox."
                  : "Creation starts synchronization in the background. You can monitor progress on the mailbox page."}
              </div>
            </>
          )}
        </div>
        <ErrorBox error={mutation.error} />
        <div className="dialog-footer">
          <Button
            variant="outline"
            type="button"
            onClick={() => (step ? setStep(step - 1) : onClose())}
          >
            {step ? "Back" : "Cancel"}
          </Button>
          <Button disabled={mutation.isPending} type="submit">
            {mutation.isPending
              ? "Saving…"
              : step < 4
                ? "Continue"
                : account
                  ? "Save settings"
                  : "Connect mailbox"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

export function MailboxDetail() {
  const { id = "" } = useParams();
  const { params, set } = useFilters();
  const tab = params.get("tab") || "overview";
  const q = useApi("/v1/accounts/" + enc(id));
  const qc = useQueryClient();
  const [edit, setEdit] = useState(false);
  const [remove, setRemove] = useState(false);
  const mutation = useMutation({
    mutationFn: (action: string) => operation(`/accounts/${enc(id)}/${action}`),
    onSuccess: () => qc.invalidateQueries(),
  });
  const del = useMutation({
    mutationFn: () => operation("/accounts/" + enc(id), "DELETE"),
    onSuccess: () => {
      qc.invalidateQueries();
      window.location.assign("/admin/mailboxes");
    },
  });
  const r = q.data;
  return (
    <>
      <Link className="back" to="/mailboxes">
        <ArrowLeft size={14} />
        Mailboxes
      </Link>
      <Heading
        eyebrow="MAILBOX"
        title={r?.name || "Mailbox"}
        description={r ? mailboxAddress(r) : "Loading mailbox…"}
      >
        {r && (
          <>
            <Button
              variant="outline"
              onClick={() =>
                mutation.mutate(r.state === "paused" ? "resume" : "pause")
              }
            >
              {r.state === "paused" ? <Play size={15} /> : <Pause size={15} />}
              {r.state === "paused" ? "Resume" : "Pause"}
            </Button>
            {r.backend !== "imap" && (
              <Button variant="outline" onClick={() => mutation.mutate("sync")}>
                <RefreshCw size={15} />
                Sync now
              </Button>
            )}
            <Button variant="outline" onClick={() => setEdit(true)}>
              Edit
            </Button>
          </>
        )}
      </Heading>
      <ErrorBox error={mutation.error} />
      <LoadState loading={q.isPending} error={q.error}>
        {r && (
          <Tabs value={tab} onValueChange={(v) => set("tab", v)}>
            <TabsList className="tabs">
              {[
                "overview",
                "messages",
                "synchronization",
                "outgoing",
                "events",
                "settings",
              ].map((t) => (
                <TabsTrigger key={t} value={t}>
                  {t}
                </TabsTrigger>
              ))}
            </TabsList>
            <TabsContent value="overview">
              <div className="metric-grid">
                {[
                  ["Connection / sync", r.state],
                  [
                    "Authorization",
                    r.authorization?.health ||
                      r.imap?.authorization?.health ||
                      (r.backend === "imap" ? "password" : "unvalidated"),
                  ],
                  ["Sending", r.sending?.state || r.smtp_state],
                ].map(([label, value]) => (
                  <div className="metric" key={label}>
                    <label>{label}</label>
                    <Badge value={value} />
                  </div>
                ))}
              </div>
              <div className="two-column">
                <Section title="Activity & recovery">
                  <Detail
                    value={{
                      last_connected_at: r.last_connected_at,
                      last_disconnected_at: r.last_disconnected_at,
                      next_retry_at: r.next_retry_at,
                      retry_count: r.retry_count,
                      last_error: r.last_error,
                      synchronization: r.synchronization,
                    }}
                  />
                </Section>
                <Section title="Provider subscription">
                  <Detail
                    value={
                      r.subscription || {
                        strategy: "IMAP IDLE and reconciliation",
                        note: "Provider push subscriptions apply to native accounts.",
                      }
                    }
                  />
                </Section>
              </div>
            </TabsContent>
            <TabsContent value="messages">
              <Messages accountId={id} />
            </TabsContent>
            <TabsContent value="synchronization">
              <Synchronization account={r} />
            </TabsContent>
            <TabsContent value="outgoing">
              <div className="subheading">
                <h2>Mailbox outgoing mail</h2>
                <TestSend account={r} />
              </div>
              <OutgoingList accountId={id} />
            </TabsContent>
            <TabsContent value="events">
              <EventsList accountId={id} />
            </TabsContent>
            <TabsContent value="settings">
              <Section title="Mailbox configuration">
                <Detail
                  value={{
                    backend: r.backend,
                    target_mailbox: r.target_mailbox,
                    credential_id: r.credential_id,
                    authorizing_identity: r.authorizing_identity,
                    shared: r.shared,
                    imap: r.imap,
                    smtp: r.smtp,
                    sync_from: r.sync_from,
                    watched_folders: r.watched_folders,
                    sent_folder: r.sent_folder,
                    trash_folder: r.trash_folder,
                    webhook_url: r.webhook_url,
                  }}
                />
                <div className="actions">
                  <Button onClick={() => setEdit(true)}>Edit settings</Button>
                  <Button variant="destructive" onClick={() => setRemove(true)}>
                    <Trash2 size={15} />
                    Delete mailbox
                  </Button>
                </div>
              </Section>
            </TabsContent>
          </Tabs>
        )}
      </LoadState>
      {r && edit && (
        <MailboxForm open={edit} onClose={() => setEdit(false)} account={r} />
      )}
      <Modal
        open={remove}
        onOpenChange={setRemove}
        title="Delete mailbox"
        description="This removes Rustbox data and stops monitoring. Provider mail remains. A send already in flight may still complete."
      >
        <ErrorBox error={del.error} />
        <div className="dialog-footer">
          <Button variant="outline" onClick={() => setRemove(false)}>
            Cancel
          </Button>
          <Button
            variant="destructive"
            disabled={del.isPending}
            onClick={() => del.mutate()}
          >
            Delete {r?.name}
          </Button>
        </div>
      </Modal>
    </>
  );
}

function Synchronization({ account }: { account: Row }) {
  const { query } = useFilters();
  const q = useApi<Page & Row>("/syncs?" + query({ account_id: account.id }));
  return (
    <>
      <Section
        title={
          account.backend === "imap"
            ? "IMAP connection & checkpoints"
            : "Native synchronization"
        }
      >
        <Detail
          value={
            account.backend === "imap"
              ? {
                  state: account.state,
                  last_connected_at: account.last_connected_at,
                  last_error: account.last_error,
                }
              : account.synchronization || { state: account.state }
          }
        />
        {account.backend === "imap" && (
          <p className="muted">
            IMAP uses IDLE and checkpoint reconciliation; manually triggered
            native sync runs do not apply.
          </p>
        )}
      </Section>
      {account.backend === "imap" && (
        <Section title="IMAP checkpoints">
          <Table headers={["Folder", "Updated", "Backfill complete"]}>
            {q.data?.imap_checkpoints?.map((c: Row) => (
              <tr key={c.mailbox}>
                <td>{c.mailbox}</td>
                <td>{time(c.updated_at)}</td>
                <td>{c.backfill_complete ? "Yes" : "No"}</td>
              </tr>
            ))}
          </Table>
          {!q.data?.imap_checkpoints?.length && (
            <p className="muted">No checkpoints recorded.</p>
          )}
        </Section>
      )}
      <LoadState
        loading={q.isPending}
        error={q.error}
        empty={!q.data?.items.length}
      >
        <Table headers={["Run", "Status", "Started", "Finished", "Details"]}>
          {q.data?.items.map((r) => (
            <tr key={r.id}>
              <td className="mono">{r.id}</td>
              <td>
                <Badge value={r.state} />
              </td>
              <td>{time(r.started_at || r.created_at)}</td>
              <td>{time(r.completed_at || r.finished_at)}</td>
              <td>
                <Inspect title="Synchronization run" value={r} />
              </td>
            </tr>
          ))}
        </Table>
      </LoadState>
      <Pagination data={q.data} />
    </>
  );
}
function TestSend({ account }: { account: Row }) {
  const [open, setOpen] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [recipient, setRecipient] = useState("");
  const [from, setFrom] = useState(() =>
    /^[^@\s]+@[^@\s]+$/.test(mailboxAddress(account))
      ? mailboxAddress(account)
      : "",
  );
  const [subject, setSubject] = useState("Rustbox test email");
  const [body, setBody] = useState(
    "This is a test email sent from the Rustbox admin panel.",
  );
  const [key, setKey] = useState(() => crypto.randomUUID());
  const qc = useQueryClient();
  const mutation = useMutation({
    mutationFn: () =>
      operation(
        `/accounts/${enc(account.id)}/send`,
        "POST",
        { from, to: [recipient], subject, text: body },
        { "Idempotency-Key": key },
      ),
    onSuccess: () => {
      qc.invalidateQueries();
      setConfirm(false);
      setKey(crypto.randomUUID());
    },
  });
  function submit(e: FormEvent) {
    e.preventDefault();
    if (confirm) mutation.mutate();
    else setConfirm(true);
  }
  return (
    <>
      <Button
        onClick={() => {
          setOpen(true);
          mutation.reset();
        }}
      >
        <Send size={15} />
        Send test email
      </Button>
      <Modal
        open={open}
        onOpenChange={setOpen}
        title={confirm ? "Confirm test email" : "Send test email"}
        description="Provider acceptance does not prove recipient delivery."
      >
        <form onSubmit={submit}>
          <Field label="From address">
            <input
              required
              type="email"
              disabled={confirm}
              value={from}
              onChange={(e) => setFrom(e.target.value)}
            />
          </Field>
          <Field label="Recipient">
            <input
              required
              type="email"
              disabled={confirm}
              value={recipient}
              onChange={(e) => setRecipient(e.target.value)}
            />
          </Field>
          <Field label="Subject">
            <input
              required
              disabled={confirm}
              value={subject}
              onChange={(e) => setSubject(e.target.value)}
            />
          </Field>
          <Field label="Plain-text body">
            <textarea
              required
              disabled={confirm}
              rows={5}
              value={body}
              onChange={(e) => setBody(e.target.value)}
            />
          </Field>
          <ErrorBox error={mutation.error} />
          {mutation.data && (
            <div className="notice">
              Test send queued. Job{" "}
              <Link to={"/outgoing/" + enc(mutation.data.id)}>
                {mutation.data.id}
              </Link>
            </div>
          )}
          <div className="dialog-footer">
            <Button
              type="button"
              variant="outline"
              onClick={() => (confirm ? setConfirm(false) : setOpen(false))}
            >
              {confirm ? "Edit" : "Close"}
            </Button>
            <Button
              disabled={mutation.isPending || !!mutation.data}
              type="submit"
            >
              {mutation.data
                ? "Test email queued"
                : mutation.isPending
                  ? "Submitting…"
                  : confirm
                    ? "Confirm & send"
                    : "Review test email"}
            </Button>
          </div>
        </form>
      </Modal>
    </>
  );
}

function Messages({ accountId }: { accountId: string }) {
  const [folder, setFolder] = useState("");
  const [cursors, setCursors] = useState<string[]>([""]);
  const [message, setMessage] = useState<Row | null>(null);
  const folders = useApi("/v1/accounts/" + enc(accountId) + "/folders");
  const after = cursors[cursors.length - 1];
  const q = useApi(
    "/v1/accounts/" +
      enc(accountId) +
      "/messages?limit=50" +
      (folder ? "&mailbox=" + enc(folder) : "") +
      (after ? "&after=" + enc(after) : ""),
  );
  return (
    <>
      <div className="filters">
        <select
          aria-label="Folder or label"
          value={folder}
          onChange={(e) => {
            setFolder(e.target.value);
            setCursors([""]);
          }}
        >
          <option value="">All folders / labels</option>
          {folders.data?.folders?.map((f: Row | string) => {
            const value = typeof f === "string" ? f : f.id || f.name || f.path;
            return (
              <option key={value} value={value}>
                {typeof f === "string"
                  ? f
                  : f.name || f.display_name || f.displayName || value}
              </option>
            );
          })}
        </select>
        <span className="muted">
          Indexed metadata · content availability varies
        </span>
      </div>
      <ErrorBox error={folders.error} />
      <LoadState
        loading={q.isPending}
        error={q.error}
        empty={!q.data?.messages?.length}
      >
        <Table headers={["Subject", "From", "Received", "Content", ""]}>
          {q.data?.messages?.map((r: Row) => {
            const m = r.metadata || r;
            return (
              <tr key={r.id}>
                <td>
                  <button className="text-button" onClick={() => setMessage(r)}>
                    {m.subject || "(No subject)"}
                  </button>
                  <small className="mono">{r.id}</small>
                </td>
                <td>
                  {m.from?.map((f: Row) => f.address || f.name).join(", ") ||
                    "—"}
                </td>
                <td>{time(r.received_at || m.date)}</td>
                <td>
                  <Badge value={r.content_state} />
                  <small>
                    {r.available_until
                      ? "Until " + time(r.available_until)
                      : ""}
                  </small>
                </td>
                <td>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => setMessage(r)}
                  >
                    Open
                  </Button>
                </td>
              </tr>
            );
          })}
        </Table>
      </LoadState>
      <div className="pagination">
        <span>Page {cursors.length}</span>
        <div>
          <Button
            variant="outline"
            disabled={cursors.length === 1}
            onClick={() => setCursors((c) => c.slice(0, -1))}
          >
            Previous
          </Button>
          <Button
            variant="outline"
            disabled={!q.data?.next_cursor || q.data?.messages?.length < 50}
            onClick={() => setCursors((c) => [...c, q.data!.next_cursor])}
          >
            Next
          </Button>
        </div>
      </div>
      {message && (
        <MessageView
          accountId={accountId}
          message={message}
          close={() => setMessage(null)}
        />
      )}
    </>
  );
}
function MessageView({
  accountId,
  message,
  close,
}: {
  accountId: string;
  message: Row;
  close: () => void;
}) {
  const q = useApi(
    `/v1/accounts/${enc(accountId)}/messages/${enc(message.id)}/content`,
    true,
    false,
  );
  const [bodyView, setBodyView] = useState("text");
  const m = q.data || message.metadata || message;
  return (
    <Modal
      title={m.subject || "Message inspection"}
      open
      onOpenChange={(v) => {
        if (!v) close();
      }}
      description={`Content ${text(message.content_state)} · Available until ${time(message.available_until)}`}
    >
      <Tabs defaultValue="body">
        <TabsList className="tabs">
          {["body", "metadata", "headers", "attachments"].map((t) => (
            <TabsTrigger key={t} value={t}>
              {t}
            </TabsTrigger>
          ))}
        </TabsList>
        <TabsContent value="body">
          {q.data?.text && q.data?.html && (
            <div
              className="actions mb"
              role="group"
              aria-label="Message body format"
            >
              <Button
                variant={bodyView === "text" ? "default" : "outline"}
                size="sm"
                onClick={() => setBodyView("text")}
              >
                Text
              </Button>
              <Button
                variant={bodyView === "html" ? "default" : "outline"}
                size="sm"
                onClick={() => setBodyView("html")}
              >
                HTML preview
              </Button>
              <small>Remote resources are blocked.</small>
            </div>
          )}
          <LoadState loading={q.isPending} error={q.error}>
            {q.data?.text && bodyView === "text" ? (
              <pre className="message-text">{q.data.text}</pre>
            ) : q.data?.html ? (
              <iframe
                title="Sanitized email body"
                sandbox=""
                referrerPolicy="no-referrer"
                srcDoc={safeEmailDocument(q.data.html)}
                className="email-frame"
              />
            ) : (
              <p className="muted">No readable message body.</p>
            )}
          </LoadState>
        </TabsContent>
        <TabsContent value="metadata">
          <Json value={message} />
        </TabsContent>
        <TabsContent value="headers">
          <Json value={m.raw_headers || []} />
        </TabsContent>
        <TabsContent value="attachments">
          <Table headers={["Filename", "Type", "Size", "Disposition"]}>
            {m.attachments?.map((a: Row, i: number) => (
              <tr key={i}>
                <td>{a.filename || "Unnamed"}</td>
                <td>{a.content_type}</td>
                <td>{bytes(a.size)}</td>
                <td>{a.disposition || "attachment"}</td>
              </tr>
            ))}
          </Table>
        </TabsContent>
      </Tabs>
    </Modal>
  );
}
