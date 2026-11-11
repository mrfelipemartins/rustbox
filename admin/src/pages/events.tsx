import { Link } from "react-router-dom";
import { useApi } from "../api";
import { enc, time, type Page } from "../lib";
import {
  Badge,
  DateFilters,
  Filters,
  FilterSelect,
  Heading,
  Inspect,
  LoadState,
  Pagination,
  Table,
  useFilters,
} from "../components/shared";
import {
  Tabs,
  TabsList,
  TabsTrigger,
  TabsContent,
} from "../components/ui/tabs";
import { MailboxFilter } from "./outgoing";
export function Events() {
  const { params, set } = useFilters();
  return (
    <>
      <Heading
        title="Events & webhooks"
        description="Business events and outgoing application webhook delivery summaries."
      />
      <Tabs
        value={params.get("tab") || "events"}
        onValueChange={(v) => set("tab", v)}
      >
        <TabsList className="tabs">
          <TabsTrigger value="events">Events</TabsTrigger>
          <TabsTrigger value="deliveries">Webhook deliveries</TabsTrigger>
        </TabsList>
        <TabsContent value="events">
          <EventsList />
        </TabsContent>
        <TabsContent value="deliveries">
          <EventsList deliveries />
        </TabsContent>
      </Tabs>
    </>
  );
}
export function EventsList({
  accountId,
  deliveries = false,
}: {
  accountId?: string;
  deliveries?: boolean;
}) {
  const { params, set, query } = useFilters();
  const q = useApi<Page>(
    "/" +
      (deliveries ? "deliveries" : "events") +
      "?" +
      query(accountId ? { account_id: accountId } : {}),
  );
  return (
    <div className="panel">
      <Filters search={false}>
        {!accountId && <MailboxFilter />}
        <input
          aria-label="Event type"
          placeholder="Event type, e.g. send.unknown"
          value={params.get("type") || ""}
          onChange={(e) => set("type", e.target.value)}
        />
        {deliveries && (
          <FilterSelect
            name="state"
            label="All delivery states"
            options={[
              "pending",
              "delivering",
              "delivered",
              "expired",
              "disabled",
            ]}
          />
        )}
        <DateFilters />
      </Filters>
      <LoadState
        loading={q.isPending}
        error={q.error}
        empty={!q.data?.items.length}
      >
        <Table
          headers={
            deliveries
              ? [
                  "Event",
                  "Destination",
                  "Delivery",
                  "Attempts",
                  "Latest response",
                  "Next / delivered",
                  "",
                ]
              : ["Event", "Type", "Mailbox", "Observed", "Related record", ""]
          }
        >
          {q.data?.items.map((r) => (
            <tr key={r.id}>
              <td className="mono">#{r.id}</td>
              {deliveries ? (
                <>
                  <td className="truncate">
                    {r.webhook_url || r.destination || "No destination"}
                  </td>
                  <td>
                    <Badge value={r.delivery_state} />
                  </td>
                  <td>{r.attempt_count}</td>
                  <td>
                    {r.last_status || "—"}
                    {r.last_error && <small>{r.last_error}</small>}
                  </td>
                  <td>{time(r.delivered_at || r.next_attempt_at)}</td>
                </>
              ) : (
                <>
                  <td>
                    <span className="event-type">{r.type}</span>
                  </td>
                  <td>
                    <Link to={"/mailboxes/" + enc(r.account_id)}>
                      {r.account_name || r.account_id?.slice(0, 12)}
                    </Link>
                  </td>
                  <td>{time(r.observed_at)}</td>
                  <td>
                    {r.data?.send_id ? (
                      <Link to={"/outgoing/" + enc(r.data.send_id)}>
                        Send {r.data.send_id.slice(0, 8)} →
                      </Link>
                    ) : r.data?.message_id ? (
                      <span className="mono">
                        {r.data.message_id.slice(0, 12)}
                      </span>
                    ) : (
                      "—"
                    )}
                  </td>
                </>
              )}
              <td>
                <Inspect title={"Event #" + r.id} value={r} />
              </td>
            </tr>
          ))}
        </Table>
      </LoadState>
      <Pagination data={q.data} />
    </div>
  );
}
