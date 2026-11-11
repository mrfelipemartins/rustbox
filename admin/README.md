# Rustbox admin frontend

React + TypeScript, Vite, TanStack Query, React Router, and local shadcn/ui components built on Radix primitives. The production build is served by Rustbox at `/admin`; there is no Node runtime in production.

## Development

Use Node.js 22.12+ and npm:

```sh
cd admin
npm ci
npm run dev
```

Run Rustbox on `127.0.0.1:8080`. Open Vite's `/admin/` URL. Vite proxies `/admin/api` to Rustbox, preserving same-origin session cookies and CSRF checks. Use `127.0.0.1`/`localhost` loopback HTTP for development; remote deployments require HTTPS.

```sh
npm run build
npm test
```

Build `admin/dist` before building a production Rustbox binary. Docker and CI perform this step automatically. Frontend assets use `/admin/` as their public base. Disabling `admin.enabled` removes the panel and its dedicated API endpoints.

## Authentication and rendering

API keys are exchanged for HttpOnly server-side sessions and are held only in the login field until submission succeeds. CSRF tokens stay in memory. Browser storage only retains the theme preference and the non-secret ID needed to complete an OAuth authorization.

Email bodies default to text when available, with an HTML preview toggle. HTML previews pass through DOMPurify and are rendered in a sandboxed iframe with a restrictive CSP; scripts, forms, embedded resources, external links, and remote tracking resources are removed.

The frontend does not resend uncertain sends. Recovery actions call Rustbox's existing reconciliation and Sent-copy endpoints and retain their backend preconditions.
