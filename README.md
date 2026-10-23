<p align="center"><img src="web/public/brand/logo-256.png" width="112" alt="Listaculous hawk logo"></p>

# Listaculous

Lightweight, self-hosted newsletter manager. Rust + Svelte + SQLite (or Postgres), ships as one small Docker image and idles at ~15–20 MB RAM.

- **Multiple sites, domains & newsletters** under one account
- **Pluggable providers:** Amazon SES (native API, no SDK), any SMTP service, or a `log` provider for testing
- **Deliverability & compliance built in**
  - Double opt-in confirmation (per newsletter), with consent IP/source/timestamp recorded
  - RFC 8058 **one-click unsubscribe** (`List-Unsubscribe` + `List-Unsubscribe-Post`) — required by Gmail/Yahoo for bulk senders
  - `List-Id`, unique `Message-ID`, multipart HTML + plain text
  - Mandatory footer with unsubscribe link and **physical address** (CAN-SPAM); sends are blocked until an address is set
  - Unsubscribe page never acts on GET (link scanners can't unsubscribe people); "unsubscribe from everything" option
  - Automatic **bounce & complaint suppression** via SES → SNS webhook
  - Honeypot + per-IP rate limiting on signup; identical responses so list membership can't be probed
  - Suppressed addresses can't be re-added by forms or imports
- Campaign editor with live preview, test sends, rate-limited background sending that resumes after restarts, pause/resume
- CSV import/export, subscriber search

## Quick start (Docker)

```sh
make up        # builds, starts, waits for health, opens the browser
make help      # every other target: logs, backup, dev, check, …
```

The first `make up` creates `.env` from `.env.example`. It also picks a random free port (saved as `PORT` in `.env`; `make new-port` picks another). Links default to `http://localhost:$PORT`; set `BASE_URL` in `.env` to your public HTTPS URL before real use. On the first visit you create the admin account.

Data lives in the `listaculous-data` Docker volume. Back it up with `make backup`, which saves the database **and** `secret.key`, the key that encrypts stored provider and integration credentials. Restore with `make restore FILE=backups/…db`. If you set `LISTACULOUS_SECRET_KEY` yourself, keep that value safe instead.

Put it behind a TLS reverse proxy (e.g. Caddy: `news.example.com { reverse_proxy listaculous:8080 }`) and set `TRUST_PROXY=true`.

## Automatic SES + DNS setup

Connect AWS (and optionally Porkbun) under **Integrations**, then open a site → **Deliverability → Automatic setup**. Listaculous previews every change, and after you click **Apply** it:

- verifies your sending subdomain in SES with Easy DKIM,
- sets a custom MAIL FROM (`bounce.<subdomain>`) for SPF alignment,
- adds a DMARC record (`p=none`) if the domain has none (an existing policy is never replaced),
- routes bounces and complaints through a configuration set and SNS to Listaculous (needs a public HTTPS `BASE_URL`),
- creates a **send-only** IAM user for the site and switches the site to its key,
- creates the DNS records in Porkbun, or lists them for you to add by hand.

It only ever *adds* DNS records, never edits or deletes them, and stops if one conflicts. Re-running it is safe. If your account is in the SES sandbox, Listaculous drafts a production access request for you to edit and submit.

**No AWS console needed:** on the Integrations page choose **AWS CLI → Get the setup command** and paste the one-liner into a terminal where the AWS CLI is signed in. It creates the `listaculous-setup` IAM user with the exact policy, makes an access key and connects it (using a temporary API key that's revoked once connected). Prefer to read it first? The script is also at `GET /api/integrations/aws-setup-script`; run it without `LISTACULOUS_API_KEY` and it prints the key to paste in.

The AWS key you connect is only used for setup; the exact IAM policy it needs is shown on the Integrations page (and at `GET /api/integrations/aws-policy`). Once your sites are set up you can delete that key in AWS. Note that the setup policy can create IAM users under `/listaculous/`, so treat the key as sensitive.

For Porkbun, create an API key and turn on **API Access** for each domain Listaculous should manage.

## Setting up a site with Amazon SES manually

1. In SES, verify your domain and publish the DKIM CNAMEs. Add SPF (`include:amazonses.com`) and a DMARC record (`v=DMARC1; p=none; rua=mailto:you@yourdomain`) — start with `p=none`, tighten later. Optionally configure a custom MAIL FROM domain for SPF alignment.
2. Request production access (leave the SES sandbox).
3. Create an IAM user with `ses:SendEmail`/`ses:SendRawEmail`; paste the keys into the site (or set `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` in `.env`).
4. Site → *Settings & sending* shows a webhook URL. Create an SNS topic, add an HTTPS subscription to that URL (auto-confirmed), and point SES Bounce + Complaint notifications at the topic.
5. Set the send rate to your SES per-second quota.
6. Create a newsletter, paste the signup form from the *Signup form* tab into your site.

## API

Everything in the app is available over a JSON API, built so scripts and AI agents can run your newsletters.

- **Docs:** open **API docs** in the sidebar, or go to `/docs` (powered by [Scalar](https://scalar.com), themed to match the app and following its Light / System / Dark switch).
- **Spec:** `/api/openapi.json` and `/api/openapi.yaml` (OpenAPI 3.1; the source is [`openapi.yaml`](openapi.yaml)). A test fails the build if a route is missing from the spec.
- **Auth:** create a key under **Account → API keys** and send `Authorization: Bearer lc_…`. Keys can do everything except manage passwords, passkeys and keys.

```sh
curl -H "Authorization: Bearer $LISTACULOUS_KEY" http://localhost:8080/api/sites
```

## Configuration

| Env var | Default | |
|---|---|---|
| `BASE_URL` | `http://localhost:8080` | Public URL used in confirm/unsubscribe links |
| `DATABASE_URL` | `sqlite://data/listaculous.db?mode=rwc` | `sqlite://…` or `postgres://…` |
| `LISTACULOUS_BIND` | `0.0.0.0:8080` | |
| `STATIC_DIR` | `web/dist` | Built admin UI |
| `TRUST_PROXY` | `false` | Use `X-Forwarded-For` for client IP |
| `LISTACULOUS_SECRET_KEY` | generated | Base64 32-byte key encrypting stored secrets. Auto-generated as `secret.key` next to a SQLite DB; **required** with Postgres. Back it up with your data. |

## Development

```sh
cargo run                      # API on :8080 (uses ./data/listaculous.db)
cd web && npm install && npm run dev   # UI on :5173, proxies /api
```

SQL is written to run on both SQLite and Postgres; migrations live in `migrations/sqlite` and `migrations/postgres`. To add a provider, implement `mailer::Provider` and register it in `mailer::provider_for`.

## Design

- Icons: [Lucide](https://lucide.dev) (`@lucide/svelte`, ISC license). Import per icon: `import Mail from '@lucide/svelte/icons/mail'`.
- Font: Inter (bundled via `@fontsource-variable/inter`, OFL; no external font requests).
- Theme: Light / System / Dark switcher (default System), stored per browser; tokens live in `web/src/app.css`.
- Brand art (hawk mascot) was generated with Codex image generation. Full-resolution originals are in `assets/brand/`; web-optimized versions in `web/public/brand/`.

## Roadmap ideas

MCP server, scheduled sends, SNS signature verification, per-site branded link domains, open/click tracking (opt-in), WYSIWYG/Markdown editor, multi-user accounts & billing for SaaS, provider webhooks for Postmark/Mailgun, encryption of stored provider secrets.

## License

[GNU AGPL v3.0 or later](LICENSE). You can use, modify and self-host Listaculous freely; if you run a modified version as a network service, you must make your source available to its users.
