-- Optional custom origin (e.g. https://newsletter.example.com) for a site's subscriber-facing links:
-- confirm, unsubscribe and the signup form endpoint. '' = use the server's BASE_URL.
ALTER TABLE sites ADD COLUMN public_url TEXT NOT NULL DEFAULT '';

-- Every hostname a site has used as its link domain. Emails already sent keep linking to old ones,
-- so the reverse proxy's on-demand TLS check (/tls/ask) keeps approving them until the site is deleted.
CREATE TABLE link_domains (
  host TEXT NOT NULL,
  site_id INTEGER NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  PRIMARY KEY (host, site_id)
);
