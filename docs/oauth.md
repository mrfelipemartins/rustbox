# OAuth

Rustbox supports Google and Microsoft mail authentication without requiring a public Rustbox endpoint. Your application owns user identity, its consent experience, and the OAuth callback. Its backend calls Rustbox using an API key. All configured API applications retain access to all accounts and credentials; this is not a tenant isolation boundary.

Configure named registrations under `oauth.providers` in the single `config.yaml`. See [configuration](configuration.md) and `config.example.yaml`. Provider IDs must remain stable after credentials reference them. Changing client IDs behind an existing registration will invalidate existing grants; create a new registration when switching OAuth applications. Configuration is loaded on restart. Secrets can be supplied through `${VARIABLE}` and an adjacent `.env`.

## Registrations

Google delegated mode requires a Google OAuth web application, client ID/secret, and registered redirect URIs. Rustbox requests the `https://www.googleapis.com/auth/gmail.modify` scope and offline access. Public applications may need Google's restricted-scope verification. Requesting consent again may be necessary to obtain a refresh token. See [Google's OAuth guide](https://developers.google.com/identity/protocols/oauth2/web-server) and [Gmail XOAUTH2 documentation](https://developers.google.com/workspace/gmail/imap/xoauth2-protocol).

Microsoft delegated mode requires an Entra application with delegated `https://graph.microsoft.com/Mail.ReadWrite`, `https://graph.microsoft.com/Mail.Send`, and `offline_access`. Choose `common`, `organizations`, `consumers`, or your tenant ID according to the accounts the registration supports. Use a web/server registration for unattended renewal. For shared delegated mailboxes configure `shared: true` on the registration to request Graph `Mail.ReadWrite.Shared` and `Mail.Send.Shared`. Graph permissions and mailbox policies determine access. See [Microsoft's mail OAuth guide](https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth).

Google Workspace domain-wide delegation uses a service account JSON key file and an administrator-approved mail scope. Create a credential with `subject` set to the impersonated mailbox. This is available for authorized Workspace organizations, not personal Gmail. Rustbox signs the JWT assertion and obtains tokens automatically. [Google's delegation guide](https://developers.google.com/identity/protocols/oauth2/service-account) describes the administrator setup.

Microsoft application mode requires a concrete tenant, client ID, and either a client secret or certificate/private-key files. Configure Microsoft Graph application `Mail.ReadWrite` and `Mail.Send`, administrator consent, an Exchange service principal, and access to the intended mailboxes; sending as a mailbox may also require SendAs permission. Rustbox obtains tokens using `https://graph.microsoft.com/.default`. This mode is for Exchange Online organizations, not personal Outlook.com accounts. See [Microsoft's application-access instructions](https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth).

Rustbox does not provision provider applications, administrator consent, mailbox permissions, or provider verification. The release supports commercial-cloud provider endpoints; sovereign clouds, generic providers, device-code helpers, and workload federation are excluded.

## Assisted consent

1. Discover configured registrations with `GET /v1/oauth/clients`.
2. Your backend calls `POST /v1/oauth/authorizations` with:

```json
{
  "client_id": "google-main",
  "redirect_uri": "https://app.example.com/mail/oauth/google/callback",
  "login_hint": "person@example.com"
}
```

3. Store the returned session `id` with your logged-in user's onboarding attempt and redirect the browser to `authorization_url`.
4. Google or Microsoft redirects to your application's callback. Your backend submits `POST /v1/oauth/authorizations/{id}/complete` using the same Rustbox API application:

```json
{"state":"callback-state","code":"callback-code"}
```

If the provider denies consent, submit `{"state":"callback-state","error":"access_denied"}` instead. Do not expose your Rustbox API key in the browser.

5. The completed session includes `credential_id`. Use it in a native account configuration:

```json
{
  "name": "My mailbox",
  "backend": "google",
  "credential_id": "credential-uuid",
  "target_mailbox": "person@example.com",
  "sync_from": "2026-01-01T00:00:00Z"
}
```

Use backend `microsoft` for Graph accounts. A successful OAuth exchange is separate from successful mailbox access, send authorization, and initial synchronization. Account creation requires an `Idempotency-Key` header. A login hint is not proof of mailbox identity.

Sessions last ten minutes and use state and PKCE. Redirect URIs must exactly match a configured value. Read status with `GET /v1/oauth/authorizations/{id}`; cancel unfinished sessions with `DELETE`. Repeating completion of a successful session returns its existing result. An interrupted code exchange may have an uncertain outcome; begin a new authorization when instructed. Rustbox does not automatically exchange a consumed code again.

## Import tokens or use service credentials

Your application may run its own consent flow and import the resulting refresh token:

```http
POST /v1/oauth/credentials
```

```json
{"client_id":"google-main","refresh_token":"provider-refresh-token"}
```

The token must belong to that configured OAuth client. Rustbox obtains an access token to validate the import. Access-token-only imports are not supported. Token material is never returned by read APIs.

For Workspace delegation:

```json
{"client_id":"google-workspace","subject":"person@example.com"}
```

For Microsoft application authorization:

```json
{"client_id":"microsoft-organization"}
```

These modes reference credentials configured on the server and do not use user refresh tokens.

## Renewal and recovery

Rustbox obtains tokens before authentication and renews near expiry. Concurrent requests share renewal work. An authenticated IMAP IDLE session does not reconnect simply because the token used to establish it has expired.

`GET /v1/oauth/credentials/{id}` returns the registration, revision, subject, health, and sanitized failure code. Account reads include authorization health independently of synchronization, subscriptions, and sending state. Webhooks report transitions through `account.reauthorization_required`, `account.authorization_failed`, and `account.authorization_restored`; successful routine refreshes do not generate events.

For assisted reauthorization, include `credential_id` in the authorization-session request. For app-managed reauthorization, send `PUT /v1/oauth/credentials/{id}` with `{"refresh_token":"replacement"}`. The registration cannot change through replacement. Account IDs and sync checkpoints remain intact; unpaused accounts resume monitoring and mailbox access is revalidated. Paused monitoring stays paused.

After fixing application credentials in YAML, restart Rustbox and explicitly resume affected accounts with `POST /v1/accounts/{id}/resume` to retry authorization. Temporary provider outages back off. Revoked grants require consent again; invalid application credentials and mailbox permissions require operator action. OAuth failures before submission do not produce an unknown delivery outcome. Existing ambiguous-submission safeguards still apply.

Delete unused credentials with `DELETE /v1/oauth/credentials/{id}`. Credentials referenced by accounts cannot be deleted. Deleting an account does not revoke a potentially shared provider grant.

## Live-provider acceptance

CI uses local peers and requires no provider credentials. Before production, verify one delegated Google account, one delegated Microsoft account, Workspace delegation, and Microsoft application authorization in an organization configured for each mode. Exercise mailbox access and sending, synchronization after token expiry, revocation, credential replacement, and paused-account recovery. Record unsupported tenant policies separately from service failures. Never commit credentials or capture token responses in test logs.
