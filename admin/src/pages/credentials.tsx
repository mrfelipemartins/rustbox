import { useState } from "react";
import { Link } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus, ExternalLink, KeyRound } from "lucide-react";
import { operation, useApi } from "../api";
import { enc, text, type Page, type Row } from "../lib";
import {
  Badge,
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
export function Credentials() {
  const { query } = useFilters();
  const q = useApi<Page>("/credentials?" + query());
  const clients = useApi("/v1/oauth/clients");
  const [open, setOpen] = useState(false);
  const [target, setTarget] = useState<Row | null>(null);
  const [remove, setRemove] = useState<Row | null>(null);
  const [mode, setMode] = useState("connect");
  const qc = useQueryClient();
  const del = useMutation({
    mutationFn: () =>
      operation("/oauth/credentials/" + enc(remove!.id), "DELETE"),
    onSuccess: () => {
      qc.invalidateQueries();
      setRemove(null);
    },
  });
  function add(t: Row | null, m: string) {
    setTarget(t);
    setMode(m);
    setOpen(true);
  }
  const callback = location.origin + "/admin/oauth/callback";
  return (
    <>
      <Heading
        title="Credentials"
        description="Manage authorization and the credentials shared by your mailboxes."
      >
        <Button onClick={() => add(null, "connect")}>
          <Plus size={15} />
          Add credential
        </Button>
      </Heading>
      <div className="panel">
        <Filters>
          <FilterSelect
            name="provider"
            label="All providers"
            options={["google", "microsoft"]}
          />
          <FilterSelect
            name="state"
            label="All health states"
            options={[
              "healthy",
              "reauthorization_required",
              "authorization_failed",
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
              "Credential",
              "Provider / registration",
              "Health",
              "Linked mailboxes",
              "Actions",
            ]}
          >
            {q.data?.items.map((r) => (
              <tr key={r.id}>
                <td>
                  <span className="record-name">
                    {r.subject || r.client_id}
                  </span>
                  <small className="mono">{r.id}</small>
                </td>
                <td>
                  {r.provider || "OAuth"}
                  <small>
                    {r.client_id} · {r.credential_type}
                  </small>
                </td>
                <td>
                  <Badge value={r.health} />
                  <small>{r.required_action || r.last_error}</small>
                </td>
                <td>
                  {(r.mailboxes || []).map((a: Row | string) =>
                    typeof a === "string" ? (
                      <Link key={a} to={"/mailboxes/" + enc(a)}>
                        {a.slice(0, 8)}{" "}
                      </Link>
                    ) : (
                      <Link key={a.id} to={"/mailboxes/" + enc(a.id)}>
                        {a.name || a.id.slice(0, 8)}{" "}
                      </Link>
                    ),
                  )}
                  {!r.mailboxes?.length && "—"}
                </td>
                <td>
                  <div className="actions">
                    {r.credential_type === "delegated" && (
                      <>
                        <Button
                          size="sm"
                          variant="outline"
                          onClick={() => add(r, "connect")}
                        >
                          Reauthorize
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => add(r, "replace")}
                        >
                          Replace
                        </Button>
                      </>
                    )}
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => {
                        del.reset();
                        setRemove(r);
                      }}
                    >
                      Delete
                    </Button>
                  </div>
                </td>
              </tr>
            ))}
          </Table>
        </LoadState>
        <Pagination data={q.data} />
      </div>
      <Section title="Configured provider registrations">
        <p className="muted">
          Registrations are configured in YAML. Changes require restart.
          Delegated authorization requires this exact callback in the provider
          console and registration redirect_uris:
        </p>
        <code className="inline-code">{callback}</code>
        <ErrorBox error={clients.error} />
        {!clients.data?.clients?.length ? (
          <div className="notice">
            No OAuth registrations configured. Add a Google or Microsoft
            provider registration to connect native mailboxes.
          </div>
        ) : (
          <div className="provider-grid">
            {clients.data.clients.map((c: Row) => (
              <article className="provider-card" key={c.id}>
                <KeyRound size={19} />
                <h3>{c.id}</h3>
                <p>
                  {c.provider} · {c.mode}
                </p>
                {c.mode === "delegated" &&
                  !c.redirect_uris?.includes(callback) && (
                    <p className="error">
                      Admin callback missing from redirect_uris.
                    </p>
                  )}
                <small>
                  {c.mode === "domain_wide_delegation"
                    ? "Workspace admin authorization and a subject are required."
                    : c.mode === "application"
                      ? "Microsoft application permissions must be granted by an administrator."
                      : "Authorize offline access to obtain a refresh token."}
                </small>
              </article>
            ))}
          </div>
        )}
      </Section>
      {open && (
        <CredentialForm
          target={target}
          initialMode={mode}
          clients={clients.data?.clients || []}
          close={() => setOpen(false)}
        />
      )}
      <Modal
        title="Delete credential"
        description="Credentials linked to mailboxes cannot be deleted. Unlink or replace those mailbox credentials first."
        open={!!remove}
        onOpenChange={(v) => {
          if (!v) setRemove(null);
        }}
      >
        <ErrorBox error={del.error} />
        <div className="dialog-footer">
          <Button variant="outline" onClick={() => setRemove(null)}>
            Cancel
          </Button>
          <Button
            variant="destructive"
            disabled={del.isPending}
            onClick={() => del.mutate()}
          >
            Delete credential
          </Button>
        </div>
      </Modal>
    </>
  );
}
function CredentialForm({
  target,
  initialMode,
  clients,
  close,
}: {
  target: Row | null;
  initialMode: string;
  clients: Row[];
  close: () => void;
}) {
  const [client, setClient] = useState(
    target?.client_id || clients[0]?.id || "",
  );
  const [mode, setMode] = useState(initialMode);
  const [token, setToken] = useState("");
  const [subject, setSubject] = useState("");
  const qc = useQueryClient();
  const registration = clients.find((c) => c.id === client);
  const delegated = registration?.mode === "delegated";
  const callback = location.origin + "/admin/oauth/callback";
  const mutation = useMutation({
    mutationFn: async () => {
      if (mode === "connect" && delegated) {
        const r = await operation("/oauth/authorizations", "POST", {
          client_id: client,
          redirect_uri: callback,
          ...(target ? { credential_id: target.id } : {}),
        });
        sessionStorage.setItem("rustbox-oauth-id", r.id);
        location.assign(r.authorization_url);
        return r;
      }
      if (mode === "replace")
        return operation("/oauth/credentials/" + enc(target!.id), "PUT", {
          refresh_token: token,
        });
      return operation("/oauth/credentials", "POST", {
        client_id: client,
        ...(delegated
          ? { refresh_token: token }
          : registration?.mode === "domain_wide_delegation"
            ? { subject }
            : {}),
      });
    },
    onSuccess: () => {
      setToken("");
      qc.invalidateQueries();
      close();
    },
  });
  return (
    <Modal
      open
      onOpenChange={(v) => {
        if (!v) close();
      }}
      title={target ? "Update authorization" : "Add credential"}
      description="Secrets are used for this request and are never displayed after storage."
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          mutation.mutate();
        }}
      >
        <Field label="Provider registration">
          <select
            required
            disabled={!!target}
            value={client}
            onChange={(e) => setClient(e.target.value)}
          >
            <option value="">Select registration</option>
            {clients.map((c) => (
              <option key={c.id} value={c.id}>
                {c.id} · {c.provider} · {c.mode}
              </option>
            ))}
          </select>
        </Field>
        {delegated && mode !== "replace" && (
          <Field label="Authorization method">
            <select value={mode} onChange={(e) => setMode(e.target.value)}>
              <option value="connect">Connect in browser</option>
              <option value="import">Import refresh token</option>
            </select>
          </Field>
        )}
        {delegated && mode === "connect" ? (
          <div className="notice">
            You will be redirected to the provider. Ensure {callback} is
            registered as an allowed redirect URI.
          </div>
        ) : delegated ? (
          <Field label="Refresh token">
            <textarea
              required
              autoComplete="off"
              value={token}
              onChange={(e) => setToken(e.target.value)}
            />
          </Field>
        ) : registration?.mode === "domain_wide_delegation" ? (
          <Field label="Workspace subject">
            <input
              required
              type="email"
              value={subject}
              onChange={(e) => setSubject(e.target.value)}
            />
          </Field>
        ) : (
          <p className="muted">
            The application certificate and permissions are configured in YAML.
            This creates a credential linked to that registration.
          </p>
        )}
        <ErrorBox error={mutation.error} />
        <div className="dialog-footer">
          <Button type="button" variant="outline" onClick={close}>
            Cancel
          </Button>
          <Button
            disabled={
              !registration ||
              mutation.isPending ||
              (delegated &&
                mode === "connect" &&
                !registration.redirect_uris?.includes(callback))
            }
            type="submit"
          >
            {mutation.isPending ? (
              "Working…"
            ) : delegated && mode === "connect" ? (
              <>
                <ExternalLink size={15} />
                Continue to provider
              </>
            ) : (
              "Save credential"
            )}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
export function OAuthCallback() {
  const [id] = useState(() => sessionStorage.getItem("rustbox-oauth-id"));
  const [done, setDone] = useState<Row | null>(null);
  const mutation = useMutation({
    mutationFn: () => {
      const p = new URLSearchParams(location.search);
      if (!id || !p.get("state"))
        throw new Error(
          "No matching OAuth authorization in this browser. Begin again from Credentials.",
        );
      return operation(
        "/oauth/authorizations/" + enc(id) + "/complete",
        "POST",
        {
          state: p.get("state"),
          ...(p.has("error")
            ? { error: p.get("error") }
            : { code: p.get("code") }),
        },
      );
    },
    onSuccess: (r) => {
      setDone(r);
      sessionStorage.removeItem("rustbox-oauth-id");
      history.replaceState(null, "", location.pathname);
    },
  });
  return (
    <>
      <Heading
        title="Complete authorization"
        description="Finish connecting your provider credential to Rustbox."
      />
      <Section title="Provider response">
        <ErrorBox error={mutation.error} />
        {done ? (
          <>
            <Badge value={done.state} />
            <p>
              {done.state === "completed"
                ? "Authorization completed. Your credential is ready."
                : "Provider authorization ended with state: " +
                  text(done.state)}
            </p>
            <Button asChild>
              <Link to="/credentials">Back to credentials</Link>
            </Button>
          </>
        ) : (
          <>
            <p>
              Confirm to exchange the provider response for a stored credential.
            </p>
            <Button
              disabled={mutation.isPending}
              onClick={() => mutation.mutate()}
            >
              {mutation.isPending ? "Completing…" : "Complete authorization"}
            </Button>
          </>
        )}
      </Section>
    </>
  );
}
