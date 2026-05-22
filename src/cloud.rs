//! Cloud credential auto-injection.
//!
//! pq used to require users to drop into the `duckdb` CLI just to
//! `CREATE SECRET ...` for gs:// / s3:// paths. That defeats the entire
//! "jq for Parquet" pitch, so we read a small set of env vars at connection
//! open time and convert them into DuckDB secrets transparently.
//!
//! ┌────────────────────────────────┬───────────────────────────────────────┐
//! │ env var(s)                     │ what we create                        │
//! ├────────────────────────────────┼───────────────────────────────────────┤
//! │ PQ_GCS_HMAC_KEY                │ TYPE GCS, KEY_ID + SECRET             │
//! │ PQ_GCS_HMAC_SECRET             │   (GCS S3-compatible HMAC, primary)   │
//! ├────────────────────────────────┼───────────────────────────────────────┤
//! │ PQ_GCS_BEARER_TOKEN            │ TYPE GCS, BEARER_TOKEN (best-effort)  │
//! │                                │   may fail on DuckDB <1.2 — we shrug  │
//! │                                │   and continue without it.            │
//! ├────────────────────────────────┼───────────────────────────────────────┤
//! │ AWS_ACCESS_KEY_ID              │ TYPE S3, KEY_ID + SECRET (+ optional  │
//! │ AWS_SECRET_ACCESS_KEY          │   SESSION_TOKEN, REGION, ENDPOINT,    │
//! │ AWS_SESSION_TOKEN  (optional)  │   URL_STYLE) — same vars boto3 reads. │
//! │ AWS_REGION / AWS_DEFAULT_REGION│                                       │
//! │ AWS_ENDPOINT_URL_S3            │                                       │
//! └────────────────────────────────┴───────────────────────────────────────┘
//!
//! Errors here are deliberately swallowed (warned to stderr if PQ_DEBUG=1).
//! The motivation: a half-working credential should never block someone
//! reading a *local* file. Cloud paths will fail loudly downstream with the
//! actual DuckDB error message, which is what the user needs to see.

use duckdb::Connection;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

/// Inspect the environment and create DuckDB secrets for any cloud
/// credentials we recognise. Idempotent — safe to call repeatedly on the
/// same connection.
pub fn inject_credentials(conn: &Connection) {
    DEBUG.store(
        env::var("PQ_DEBUG").ok().as_deref() == Some("1"),
        Ordering::Relaxed,
    );

    install_gcs_hmac(conn);
    install_gcs_bearer(conn);
    install_s3(conn);
}

fn install_gcs_hmac(conn: &Connection) {
    let key = match env::var("PQ_GCS_HMAC_KEY") {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    let secret = match env::var("PQ_GCS_HMAC_SECRET") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            warn("PQ_GCS_HMAC_KEY set without PQ_GCS_HMAC_SECRET — skipping GCS HMAC secret");
            return;
        }
    };

    let sql = format!(
        "CREATE OR REPLACE SECRET pq_gcs_hmac (TYPE GCS, KEY_ID '{}', SECRET '{}');",
        sql_escape(&key),
        sql_escape(&secret),
    );
    if let Err(e) = conn.execute_batch(&sql) {
        warn(&format!("GCS HMAC secret rejected by DuckDB: {e}"));
    } else {
        debug("registered GCS HMAC secret from PQ_GCS_HMAC_*");
    }
}

fn install_gcs_bearer(conn: &Connection) {
    // Two reasons to keep this even though the released `gcs/config` provider
    // doesn't accept `bearer_token` yet: (a) once #22413 lands and we bump
    // duckdb-rs, this Just Works; (b) some local builds patch it in. We try
    // and silently bail if rejected.
    let token = match env::var("PQ_GCS_BEARER_TOKEN") {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };

    // Try the future-proof spelling first (PROVIDER oauth2), then fall back to
    // the bare config-provider spelling that DuckDB's error hint mentions.
    let attempts = [
        format!(
            "CREATE OR REPLACE SECRET pq_gcs_bearer (TYPE GCS, PROVIDER OAUTH2, BEARER_TOKEN '{}');",
            sql_escape(&token)
        ),
        format!(
            "CREATE OR REPLACE SECRET pq_gcs_bearer (TYPE GCS, BEARER_TOKEN '{}');",
            sql_escape(&token)
        ),
    ];
    for sql in &attempts {
        if conn.execute_batch(sql).is_ok() {
            debug("registered GCS bearer-token secret from PQ_GCS_BEARER_TOKEN");
            return;
        }
    }
    warn(
        "PQ_GCS_BEARER_TOKEN was set but DuckDB rejected both forms — \
          this DuckDB build doesn't support GCS OAuth2 yet (see issue #22413). \
          Use PQ_GCS_HMAC_KEY / PQ_GCS_HMAC_SECRET instead.",
    );
}

fn install_s3(conn: &Connection) {
    let key = match env::var("AWS_ACCESS_KEY_ID") {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    let secret = match env::var("AWS_SECRET_ACCESS_KEY") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            warn("AWS_ACCESS_KEY_ID set without AWS_SECRET_ACCESS_KEY — skipping S3 secret");
            return;
        }
    };

    let mut params = vec![("KEY_ID".to_string(), key), ("SECRET".to_string(), secret)];

    // STS / role-chain users — boto convention.
    if let Ok(t) = env::var("AWS_SESSION_TOKEN") {
        if !t.is_empty() {
            params.push(("SESSION_TOKEN".into(), t));
        }
    }
    // AWS_REGION wins over AWS_DEFAULT_REGION (matches boto3 precedence).
    let region = env::var("AWS_REGION")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            env::var("AWS_DEFAULT_REGION")
                .ok()
                .filter(|s| !s.is_empty())
        });
    if let Some(r) = region {
        params.push(("REGION".into(), r));
    }
    // AWS_ENDPOINT_URL_S3 lets users point at MinIO / GCS-as-S3 / Cloudflare R2.
    if let Ok(ep) = env::var("AWS_ENDPOINT_URL_S3") {
        if !ep.is_empty() {
            // Strip scheme — DuckDB's S3 endpoint param is bare host[:port].
            let bare = ep
                .strip_prefix("https://")
                .or_else(|| ep.strip_prefix("http://"))
                .unwrap_or(&ep)
                .trim_end_matches('/');
            params.push(("ENDPOINT".into(), bare.to_string()));
            // Path-style is the safe default for non-AWS S3 endpoints.
            params.push(("URL_STYLE".into(), "path".into()));
            // http endpoints need use_ssl=false explicitly.
            if ep.starts_with("http://") {
                params.push(("USE_SSL".into(), "false".into()));
            }
        }
    }

    let body = params
        .iter()
        .map(|(k, v)| {
            // URL_STYLE / USE_SSL take bare keywords, not strings; everything
            // else needs single-quoting + escape.
            if k == "URL_STYLE" || k == "USE_SSL" {
                format!("{k} {v}")
            } else {
                format!("{k} '{}'", sql_escape(v))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!("CREATE OR REPLACE SECRET pq_s3 (TYPE S3, {body});");
    if let Err(e) = conn.execute_batch(&sql) {
        warn(&format!("S3 secret rejected by DuckDB: {e}"));
    } else {
        debug("registered S3 secret from AWS_* env vars");
    }
}

/// SQL string-literal escape: double up single quotes. Good enough because
/// every value flowing through here has already been split out of env, and we
/// always wrap in single-quoted SQL literals.
fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn debug(msg: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("pq cloud: {msg}");
    }
}

fn warn(msg: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("pq cloud (warn): {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_escape_doubles_single_quotes() {
        assert_eq!(sql_escape("alice's"), "alice''s");
        assert_eq!(sql_escape("plain"), "plain");
        assert_eq!(sql_escape(""), "");
        assert_eq!(sql_escape("a''b"), "a''''b");
    }
}
