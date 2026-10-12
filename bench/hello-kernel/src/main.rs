//! hello-kernel: footprint probe for Sun's kernel stack.
//!
//! Opens a SQLite store in WAL mode, applies one table, and serves
//! `GET /health` (which round-trips the pool) over axum on tokio's
//! multi-thread runtime. `scripts/bench.sh` measures binary size,
//! cold start to first HTTP 200 and idle RSS of this process.
//!
//! Config: `HELLO_DB` (default `hello.db`), `HELLO_ADDR` (default `127.0.0.1:7878`).

use axum::{Router, extract::State, http::StatusCode, routing::get};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::var("HELLO_DB").unwrap_or_else(|_| "hello.db".into());
    let addr = std::env::var("HELLO_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".into());

    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, body TEXT NOT NULL)")
        .execute(&pool)
        .await?;

    let app = Router::new().route("/health", get(health)).with_state(pool);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(pool): State<SqlitePool>) -> Result<&'static str, StatusCode> {
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .map(|_| "ok")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}
