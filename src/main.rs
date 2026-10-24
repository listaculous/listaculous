mod api;
mod api_keys;
mod auth;
mod aws;
mod crypto;
mod db;
mod dns;
mod error;
mod favicon;
mod integrations;
mod mailer;
mod models;
mod openapi;
mod passkeys;
mod public;
mod setup;
mod worker;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

/// Build version: `YYYY.MM.DD.HHMM` (UTC) for release images, set by the release workflow at
/// compile time; `dev` for local builds.
pub const VERSION: &str = match option_env!("LISTACULOUS_VERSION") {
    Some(v) => v,
    None => "dev",
};

#[derive(Clone)]
pub struct Config {
    pub bind: String,
    pub database_url: String,
    /// Public URL where this server is reachable; used for confirm/unsubscribe links.
    pub base_url: String,
    pub static_dir: String,
    /// When true, client IP is read from X-Forwarded-For (set when behind a reverse proxy).
    pub trust_proxy: bool,
}

impl Config {
    fn from_env() -> Self {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Self {
            bind: env("LISTACULOUS_BIND", "0.0.0.0:8080"),
            database_url: env("DATABASE_URL", "sqlite://data/listaculous.db?mode=rwc"),
            base_url: env("BASE_URL", "http://localhost:8080")
                .trim_end_matches('/')
                .to_string(),
            static_dir: env("STATIC_DIR", "web/dist"),
            trust_proxy: matches!(env("TRUST_PROXY", "false").as_str(), "1" | "true" | "yes"),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: db::Db,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub limiter: Arc<RateLimiter>,
    pub auth_limiter: Arc<RateLimiter>,
    pub webauthn: Arc<webauthn_rs::Webauthn>,
    pub openapi: Arc<serde_json::Value>,
    /// Random per-installation id (namespaces AWS resources).
    pub instance_id: Arc<String>,
}

/// Fixed-window per-key rate limiter for public endpoints.
pub struct RateLimiter {
    hits: Mutex<HashMap<String, (Instant, u32)>>,
    max: u32,
    window: Duration,
}

impl RateLimiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
            max,
            window,
        }
    }

    pub fn check(&self, key: &str) -> bool {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();
        if hits.len() > 10_000 {
            hits.retain(|_, (t, _)| now.duration_since(*t) < self.window);
        }
        let entry = hits.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.max
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "listaculous=info,tower_http=warn".into()),
        )
        .init();

    // One TLS crypto provider for every rustls user (reqwest, SMTP, Postgres): ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = Arc::new(Config::from_env());
    // `listaculous backup <file>`: consistent online copy of the SQLite database (includes WAL).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("backup") {
        let out = args
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("usage: listaculous backup <output.db>"))?;
        return db::backup(&cfg.database_url, out).await;
    }
    crypto::init(&cfg.database_url)?;
    let db = db::connect(&cfg.database_url).await?;
    crypto::migrate_plaintext(&db).await?;
    let instance_id = db::instance_id(&db).await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let state = AppState {
        db,
        cfg: cfg.clone(),
        http,
        limiter: Arc::new(RateLimiter::new(20, Duration::from_secs(60))),
        auth_limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(300))),
        webauthn: Arc::new(passkeys::build(&cfg.base_url)?),
        openapi: Arc::new(openapi::load(&cfg.base_url)?),
        instance_id: Arc::new(instance_id),
    };

    tokio::spawn(worker::run(state.clone()));
    tokio::spawn(favicon::backfill(state.clone()));

    let admin = Router::new()
        .route("/openapi.json", get(openapi::json))
        .route("/openapi.yaml", get(openapi::yaml))
        .route("/auth/status", get(auth::status))
        .route("/auth/setup", post(auth::setup))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/password", post(auth::change_password))
        .route("/auth/api-keys", get(api_keys::list).post(api_keys::create))
        .route("/auth/api-keys/{id}", delete(api_keys::revoke))
        .route("/auth/passkeys", get(passkeys::list))
        .route(
            "/auth/passkeys/{id}",
            axum::routing::put(passkeys::rename).delete(passkeys::remove),
        )
        .route(
            "/auth/passkey/register/start",
            post(passkeys::register_start),
        )
        .route(
            "/auth/passkey/register/finish",
            post(passkeys::register_finish),
        )
        .route("/auth/passkey/login/start", post(passkeys::login_start))
        .route("/auth/passkey/login/finish", post(passkeys::login_finish))
        .route("/dashboard", get(api::dashboard))
        .route("/sites", get(api::list_sites).post(api::create_site))
        .route(
            "/sites/{id}",
            get(api::get_site)
                .put(api::update_site)
                .delete(api::delete_site),
        )
        .route("/sites/{id}/test", post(api::test_site))
        .route(
            "/integrations",
            get(integrations::list).post(integrations::create),
        )
        .route("/integrations/{id}", delete(integrations::remove))
        .route("/integrations/aws-policy", get(integrations::aws_policy))
        .route(
            "/integrations/aws-setup-script",
            get(integrations::aws_script),
        )
        .route("/sites/{id}/setup", get(setup::status))
        .route("/sites/{id}/setup/plan", post(setup::plan))
        .route("/sites/{id}/setup/apply", post(setup::apply))
        .route("/sites/{id}/setup/cleanup", post(setup::cleanup))
        .route(
            "/sites/{id}/setup/production-access",
            get(setup::production_access_draft).post(setup::request_production_access),
        )
        .route("/sites/{id}/icon", get(favicon::get_icon))
        .route("/sites/{id}/icon/refresh", post(favicon::refresh_icon))
        .route(
            "/sites/{id}/lists",
            get(api::list_lists).post(api::create_list),
        )
        .route("/sites/{id}/events", get(api::site_events))
        .route(
            "/lists/{id}",
            get(api::get_list)
                .put(api::update_list)
                .delete(api::delete_list),
        )
        .route(
            "/lists/{id}/subscribers",
            get(api::list_subscribers).post(api::add_subscriber),
        )
        .route("/lists/{id}/import", post(api::import_subscribers))
        .route("/lists/{id}/export", get(api::export_subscribers))
        .route(
            "/lists/{id}/campaigns",
            get(api::list_campaigns).post(api::create_campaign),
        )
        .route("/subscriptions/{id}", delete(api::delete_subscription))
        .route(
            "/subscriptions/{id}/unsubscribe",
            post(api::admin_unsubscribe),
        )
        .route(
            "/campaigns/{id}",
            get(api::get_campaign)
                .put(api::update_campaign)
                .delete(api::delete_campaign),
        )
        .route("/campaigns/{id}/preview", post(api::preview_campaign))
        .route("/campaigns/{id}/test", post(api::test_campaign))
        .route("/campaigns/{id}/send", post(api::send_campaign))
        .route("/campaigns/{id}/pause", post(api::pause_campaign));

    // Public endpoints: embedded signup forms post here from other domains.
    let public_api = Router::new()
        .route("/subscribe/{public_id}", post(public::subscribe))
        .layer(CorsLayer::permissive());

    let index = format!("{}/index.html", cfg.static_dir);
    let app = Router::new()
        .nest("/api", admin)
        .merge(public_api)
        .route("/confirm/{token}", get(public::confirm))
        .route(
            "/unsubscribe/{token}",
            get(public::unsubscribe_page).post(public::unsubscribe),
        )
        .route(
            "/webhooks/ses/{site_id}/{secret}",
            post(public::ses_webhook),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route_service(
            "/docs",
            ServeFile::new(format!("{}/docs.html", cfg.static_dir)),
        )
        .fallback_service(ServeDir::new(&cfg.static_dir).fallback(ServeFile::new(index)))
        .layer(axum::middleware::from_fn(cache_headers))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            20 * 1024 * 1024,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(
        "listaculous {VERSION} listening on {} (public url {})",
        cfg.bind,
        cfg.base_url
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

/// Hashed build assets never change, so they cache forever; everything else (index.html, API,
/// public pages) must be revalidated so upgrades show up immediately.
async fn cache_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header::{HeaderValue, CACHE_CONTROL};
    let immutable = req.uri().path().starts_with("/assets/");
    let brand = req.uri().path().starts_with("/brand/");
    let mut res = next.run(req).await;
    if !res.headers().contains_key(CACHE_CONTROL) {
        let v = if immutable {
            "public, max-age=31536000, immutable"
        } else if brand {
            "public, max-age=86400"
        } else {
            "no-cache"
        };
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static(v));
    }
    res
}
