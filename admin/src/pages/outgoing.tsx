import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";
import { operation, useApi } from "../api";
import { enc, time, type Page } from "../lib";
import {
  Badge,
  DateFilters,
  Detail,
  ErrorBox,
  Field,
  Filters,
  FilterSelect,
  Heading,
  LoadState,
  Pagination,
  Section,
  Table,
  useFilters,
} from "../components/shared";
import { Button } from "../components/ui/button";
import { Modal } from "../components/ui/dialog";
export function Outgoing() {
  return (
    <>
      <Heading
        title="Outgoing mail"
        description="Track submissions, inspect uncertain outcomes, and recover Sent copies."
      />
      <OutgoingList />
    </>
  );
}
export function OutgoingList({ accountId }: { accountId?: string }) {
  const { query } = useFilters();
  const q = useApi<Page>(
    "/sends?" + query(accountId ? { account_id: accountId } : {}),
  );
  return (
    <div className="panel">
      <Filters search={false}>
        {!accountId && <MailboxFilter />}
        <FilterSelect
          name="provider"
          label="All providers"
          options={["google", "microsoft", "imap"]}
        />
        <FilterSelect
          name="status"
          label="All statuses"
          options={["queued", "sending", "accepted", "failed", "unknown"]}
        />
        <DateFilters />
      </Filters>
      <LoadState
        loading={q.isPending}
        error={q.error}
        empty={!q.data?.items.length}
      >
        <Table
          headers={[
            "Send job",
            "Mailbox",
            "Submission",
            "Sent copy",
            "Attempts",
            "Created",
            "",
          ]}
        >
          {q.data?.items.map((r) => (
            <tr key={r.id}>
              <td>
                <Link
                  className="record-name mono"
                  to={"/outgoing/" + enc(r.id)}
                >
                  {r.id.slice(0, 12)}…
                </Link>
                <small className="mono">{r.message_id}</small>
              </td>
              <td>
                <Link to={"/mailboxes/" + enc(r.account_id)}>
                  {r.account_name || r.account_id.slice(0, 12)}
                </Link>
              </td>
              <td>
                <Badge value={r.status} />
              </td>
              <td>
                <Badge value={r.append_status} />
              </td>
              <td>{r.attempts}</td>
              <td>{time(r.created_at)}</td>
              <td>
                <Link to={"/outgoing/" + enc(r.id)}>Inspect →</Link>
              </td>
            </tr>
          ))}
        </Table>
      </LoadState>
      <Pagination data={q.data} />
    </div>
  );
}
export function MailboxFilter() {
  const { params, set } = useFilters();
  const [search, setSearch] = useState("");
  const q = useApi<Page>("/mailboxes?per_page=100&q=" + enc(search));
  return (
    <>
      <input
        aria-label="Find mailbox"
        placeholder="Find mailbox…"
        value={search}
        onChange={(e) => setSearch(e.target.value)}
      />
      <select
        aria-label="Mailbox"
        value={params.get("account_id") || ""}
        onChange={(e) => set("account_id", e.target.value)}
      >
        <option value="">All mailboxes</option>
        {q.data?.items.map((r) => (
          <option key={r.id} value={r.id}>
            {r.name}
          </option>
        ))}
      </select>
    </>
  );
}
export function SendDetail() {
  const { id = "" } = useParams();
  const q = useApi("/v1/sends/" + enc(id));
  const qc = useQueryClient();
  const [action, setAction] = useState("");
  const [reason, setReason] = useState("");
  const [evidence, setEvidence] = useState("");
  const [decision, setDecision] = useState("unresolved");
  const [ack, setAck] = useState(false);
  const mutation = useMutation({
    mutationFn: () =>
      operation(
        "/sends/" + enc(id) + "/" + action,
        "POST",
        action === "resolve"
          ? { decision, reason, evidence: { note: evidence } }
          : action === "append/retry"
            ? { allow_duplicate_copy: ack, ...(ack ? { reason } : {}) }
            : {},
      ),
    onSuccess: () => {
      qc.invalidateQueries();
      setAction("");
      setReason("");
      setAck(false);
    },
  });
  const r = q.data;
  const canAppend =
    r?.status === "accepted" &&
    ["failed", "unknown"].includes(r?.append_status) &&
    (!r?.append_recoverable_until ||
      new Date(
        /Z$|[+-]\d\d:\d\d$/.test(r.append_recoverable_until)
          ? r.append_recoverable_until
          : r.append_recoverable_until + "Z",
      ).getTime() > Date.now());
  return (
    <>
      <Link className="back" to="/outgoing">
        <ArrowLeft size={14} />
        Outgoing mail
      </Link>
      <Heading
        eyebrow="SEND JOB"
        title={id.slice(0, 12) + "…"}
        description="Provider acceptance confirms submission; recipient delivery is not tracked."
      />
      <LoadState loading={q.isPending} error={q.error}>
        {r && (
          <>
            <div className="metric-grid">
              {[
                ["Submission", r.status],
                ["Reconciliation", r.reconciliation_state],
                ["Operator resolution", r.resolution],
                ["Sent copy", r.append_status],
              ].map(([k, v]) => (
                <div className="metric" key={k}>
                  <label>{k}</label>
                  <Badge value={v} />
                </div>
              ))}
            </div>
            <div className="actions mb">
              {r.status === "unknown" && !r.resolution && (
                <>
                  <Button
                    variant="outline"
                    onClick={() => {
                      mutation.reset();
                      setAction("reconcile");
                    }}
                  >
                    Reconcile submission
                  </Button>
                  <Button
                    variant="outline"
                    onClick={() => {
                      mutation.reset();
                      setAction("resolve");
                    }}
                  >
                    Record resolution
                  </Button>
                </>
              )}
              {canAppend && (
                <>
                  <Button
                    variant="outline"
                    onClick={() => {
                      mutation.reset();
                      setAction("append/reconcile");
                    }}
                  >
                    Reconcile Sent copy
                  </Button>
                  <Button
                    variant="outline"
                    disabled={
                      r.append_status === "unknown" &&
                      r.append_reconciliation_state !== "not_found"
                    }
                    onClick={() => {
                      mutation.reset();
                      setAction("append/retry");
                    }}
                  >
                    Retry Sent copy
                  </Button>
                </>
              )}
            </div>
            {r.status === "unknown" && (
              <div className="notice mb">
                This submission may already have been accepted. Reconciliation
                and operator resolution never resend the email.
              </div>
            )}
            <div className="two-column">
              <Section title="Submission">
                <Detail
                  value={{
                    id: r.id,
                    account_id: r.account_id,
                    message_id: r.message_id,
                    provider_id: r.provider_id,
                    graph_phase: r.graph_phase,
                    created_at: r.created_at,
                    submitted_at: r.submitted_at,
                    attempts: r.attempts,
                    last_error: r.last_error,
                  }}
                />
              </Section>
              <Section title="Evidence & recovery">
                <Detail
                  value={{
                    reconciliation_state: r.reconciliation_state,
                    reconciliation_evidence: r.reconciliation_evidence,
                    resolution: r.resolution,
                    resolution_reason: r.resolution_reason,
                    resolution_evidence: r.resolution_evidence,
                    append_status: r.append_status,
                    append_attempts: r.append_attempts,
                    append_last_error: r.append_last_error,
                    append_reconciliation_state: r.append_reconciliation_state,
                    append_reconciliation_evidence:
                      r.append_reconciliation_evidence,
                    append_recoverable_until: r.append_recoverable_until,
                  }}
                />
              </Section>
            </div>
          </>
        )}
      </LoadState>
      <Modal
        open={!!action}
        onOpenChange={(v) => {
          if (!v) setAction("");
        }}
        title={
          action === "resolve"
            ? "Record operator resolution"
            : action === "append/retry"
              ? "Retry Sent copy"
              : "Reconcile send evidence"
        }
        description={
          action.startsWith("append")
            ? "This action only concerns the Sent-folder copy. It never submits another email."
            : "These actions preserve the original submission outcome and never submit mail."
        }
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            mutation.mutate();
          }}
        >
          {action === "resolve" && (
            <>
              <Field label="Decision">
                <select
                  value={decision}
                  onChange={(e) => setDecision(e.target.value)}
                >
                  <option value="unresolved">Unresolved</option>
                  <option
                    value="sent"
                    disabled={r?.reconciliation_state !== "matched"}
                  >
                    Sent (requires matched evidence)
                  </option>
                </select>
              </Field>
              <Field label="Evidence / investigation reference">
                <input
                  required
                  value={evidence}
                  onChange={(e) => setEvidence(e.target.value)}
                />
              </Field>
              <Field label="Reason">
                <textarea
                  required
                  maxLength={1024}
                  value={reason}
                  onChange={(e) => setReason(e.target.value)}
                />
              </Field>
            </>
          )}
          {action === "append/retry" && r?.append_status === "unknown" && (
            <>
              <label className="check">
                <input
                  type="checkbox"
                  required
                  checked={ack}
                  onChange={(e) => setAck(e.target.checked)}
                />
                I accept that retrying may create a duplicate Sent copy.
              </label>
              <Field label="Reason">
                <textarea
                  required
                  value={reason}
                  onChange={(e) => setReason(e.target.value)}
                />
              </Field>
            </>
          )}
          <ErrorBox error={mutation.error} />
          <div className="dialog-footer">
            <Button
              variant="outline"
              type="button"
              onClick={() => setAction("")}
            >
              Cancel
            </Button>
            <Button disabled={mutation.isPending} type="submit">
              {mutation.isPending ? "Working…" : "Confirm"}
            </Button>
          </div>
        </form>
      </Modal>
    </>
  );
}
