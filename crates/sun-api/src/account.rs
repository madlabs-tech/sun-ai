//! The ecosystem's account at Sun:
//! `/api/v1/account/...` over the kernel's [`sun_core::CloudAccount`].
//!
//! - `GET /account`: plan, subscription, points balance, catalog, support target.
//! - `POST /account/subscription` (admin) `{plan, seats, currency, method}`
//!   → 201 the invoice with its `checkout_url`.
//! - `POST /account/top-ups` (admin) `{points, currency, method}` → 201 invoice.
//! - `GET /account/invoices`.
//! - `GET /account/support`; `POST /account/support` `{subject, body}` → 201.
//!
//! Without an account (self-hosted, off Sun Cloud) every route answers 503.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sun_core::CloudAccount;

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::routes::Body;

const MAX_SUBJECT: usize = 200;
const MAX_MESSAGE: usize = 5000;
const MAX_SEATS: u32 = 10_000;

fn account(api: &Api) -> ApiResult<Arc<dyn CloudAccount>> {
    api.account.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Plans and support need Sun Cloud: this ecosystem isn't connected to Sun.",
        )
    })
}

/// IDR pays by QRIS, bank transfer or card; USD by card.
fn payment(currency: &str, method: &str) -> ApiResult<()> {
    match (currency, method) {
        ("idr", "qris" | "bank_transfer" | "card") | ("usd", "card") => Ok(()),
        ("idr" | "usd", _) => Err(ApiError::invalid(format!(
            "`{method}` isn't offered in {}: IDR takes qris, bank_transfer or card; USD takes card.",
            currency.to_uppercase()
        ))),
        _ => Err(ApiError::invalid("`currency` must be `idr` or `usd`.")),
    }
}

pub(crate) async fn get(State(api): State<Api>) -> ApiResult<Json<Value>> {
    Ok(Json(account(&api)?.account().await?))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewSubscription {
    plan: String,
    seats: u32,
    currency: String,
    method: String,
}

pub(crate) async fn subscribe(
    State(api): State<Api>,
    Body(body): Body<NewSubscription>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let account = account(&api)?;
    if !matches!(body.plan.as_str(), "individual" | "business") {
        return Err(ApiError::invalid(
            "`plan` must be `individual` or `business`.",
        ));
    }
    if !(1..=MAX_SEATS).contains(&body.seats) {
        return Err(ApiError::invalid(format!("`seats` must be 1–{MAX_SEATS}.")));
    }
    payment(&body.currency, &body.method)?;
    let invoice = account.subscribe(serde_json::json!(body)).await?;
    Ok((StatusCode::CREATED, Json(invoice)))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTopUp {
    points: i64,
    currency: String,
    method: String,
}

pub(crate) async fn top_up(
    State(api): State<Api>,
    Body(body): Body<NewTopUp>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let account = account(&api)?;
    if body.points <= 0 {
        return Err(ApiError::invalid("`points` must be above 0."));
    }
    payment(&body.currency, &body.method)?;
    let invoice = account.top_up(serde_json::json!(body)).await?;
    Ok((StatusCode::CREATED, Json(invoice)))
}

pub(crate) async fn invoices(State(api): State<Api>) -> ApiResult<Json<Value>> {
    Ok(Json(account(&api)?.invoices().await?))
}

pub(crate) async fn tickets(State(api): State<Api>) -> ApiResult<Json<Value>> {
    Ok(Json(account(&api)?.tickets().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTicket {
    subject: String,
    body: String,
}

pub(crate) async fn open_ticket(
    State(api): State<Api>,
    Body(body): Body<NewTicket>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let account = account(&api)?;
    let subject = body.subject.trim();
    if subject.is_empty()
        || subject.chars().count() > MAX_SUBJECT
        || subject.contains(char::is_control)
    {
        return Err(ApiError::invalid(format!(
            "The subject must be 1–{MAX_SUBJECT} characters on one line."
        )));
    }
    let message = body.body.trim();
    if message.is_empty() || message.chars().count() > MAX_MESSAGE {
        return Err(ApiError::invalid(format!(
            "The message must be 1–{MAX_MESSAGE} characters."
        )));
    }
    let ticket = account.open_ticket(subject, message).await?;
    Ok((StatusCode::CREATED, Json(ticket)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn account_payment_is_idr_any_method_or_usd_card() {
        for (currency, method) in [
            ("idr", "qris"),
            ("idr", "bank_transfer"),
            ("idr", "card"),
            ("usd", "card"),
        ] {
            assert!(
                super::payment(currency, method).is_ok(),
                "{currency} {method}"
            );
        }
        for (currency, method) in [
            ("usd", "qris"),
            ("usd", "bank_transfer"),
            ("eur", "card"),
            ("idr", "cash"),
        ] {
            assert!(
                super::payment(currency, method).is_err(),
                "{currency} {method}"
            );
        }
    }
}
