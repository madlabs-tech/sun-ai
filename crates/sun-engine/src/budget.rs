//! Spend caps: an ecosystem's monthly cap in USD and a task's
//! own cost budget, both kept in `kernel_setting`.
//!
//! - **Ecosystem cap** (`spend_cap` = `{monthly_usd}`): once the `usage` cost
//!   journalled since the 1st of the month (UTC) reaches it, the queue holds
//!   new runs, an agent step about to start ends `budget_exceeded`, and the
//!   watchdog ends a running one at its next cost report.
//! - **Task budget** (`budget:task:<id>` = `{cost_usd}`): caps each run of
//!   the task; the smaller of it and the workflow's `budget.cost_usd` wins.
//!
//! The kernel only sees the cost runtimes report (`Usage.cost_usd`), so a
//! subscription runtime that reports none never counts against a cap.

use chrono::{Datelike, TimeZone, Utc};
use serde_json::json;
use sun_core::{Error, Result, TaskId, Timestamp};
use sun_store::{EventQuery, SettingRepo, TaskRepo, Tx};

/// `kernel_setting` key of the ecosystem's monthly cap.
const SPEND_CAP: &str = "spend_cap";

fn task_budget_key(task: TaskId) -> String {
    format!("budget:task:{task}")
}

/// The ecosystem's spend this month against its cap (`GET /settings/budget`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Spend {
    /// The monthly cap in USD; `None`: no cap.
    pub monthly_usd: Option<f64>,
    /// Cost journalled since `since`.
    pub spent_usd: f64,
    /// The start of this month (UTC), when the count resets.
    pub since: Timestamp,
}

/// Midnight UTC on the 1st of `now`'s month.
pub(crate) fn month_start(now: Timestamp) -> Timestamp {
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now)
}

/// Midnight UTC on the 1st of the month after `now`'s: when a reached cap
/// frees up again.
pub(crate) fn next_month_start(now: Timestamp) -> Timestamp {
    let start = month_start(now);
    start
        .checked_add_months(chrono::Months::new(1))
        .unwrap_or(start)
}

/// The ecosystem's monthly cap, if one is set.
pub(crate) async fn monthly_cap(tx: &mut Tx) -> Result<Option<f64>> {
    Ok(tx
        .setting(SPEND_CAP)
        .await?
        .and_then(|v| v["monthly_usd"].as_f64()))
}

/// Cost of every `usage` event since `since`.
pub(crate) async fn spent_since(tx: &mut Tx, since: Timestamp) -> Result<f64> {
    let rows = tx.usage_by_runtime(Some(&since)).await?;
    Ok(rows.iter().map(|(_, usage)| usage.cost_usd).sum())
}

/// Why `spent` is over `cap`, if it is (the cap is reached at equality).
pub(crate) fn over_cap(cap: f64, spent: f64) -> Option<String> {
    (spent >= cap).then(|| format!("Ecosystem spend ${spent:.2} reached the ${cap:.2} monthly cap"))
}

/// Why no new agent work may start at `now`: the cap is reached. `None` when
/// there is no cap or room is left.
pub(crate) async fn ecosystem_over(tx: &mut Tx, now: Timestamp) -> Result<Option<String>> {
    let Some(cap) = monthly_cap(tx).await? else {
        return Ok(None);
    };
    let spent = spent_since(tx, month_start(now)).await?;
    Ok(over_cap(cap, spent))
}

/// The task's own cost budget, if one is set.
pub(crate) async fn task_budget(tx: &mut Tx, task: TaskId) -> Result<Option<f64>> {
    Ok(tx
        .setting(&task_budget_key(task))
        .await?
        .and_then(|v| v["cost_usd"].as_f64()))
}

/// The smaller of two optional caps.
pub(crate) fn tighter(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A cap from the API: `None` clears it; else finite and not negative.
fn checked(usd: Option<f64>, field: &str) -> Result<Option<f64>> {
    match usd {
        Some(v) if !v.is_finite() || v < 0.0 => Err(Error::InvalidInput(format!(
            "{field}: use an amount in USD of 0 or more"
        ))),
        other => Ok(other),
    }
}

impl crate::Engine {
    /// The ecosystem's cap and what it spent this month.
    pub async fn spend(&self) -> Result<Spend> {
        let mut tx = self.store.read().await?;
        let since = month_start(Utc::now());
        Ok(Spend {
            monthly_usd: monthly_cap(&mut tx).await?,
            spent_usd: spent_since(&mut tx, since).await?,
            since,
        })
    }

    /// Sets (`Some`) or clears (`None`) the ecosystem's monthly cap. The
    /// caller wakes the queue so held runs start when it was raised.
    pub async fn set_spend_cap(&self, monthly_usd: Option<f64>) -> Result<()> {
        let monthly_usd = checked(monthly_usd, "monthly_usd")?;
        let mut tx = self.store.begin().await?;
        tx.put_setting(SPEND_CAP, &json!({ "monthly_usd": monthly_usd }))
            .await?;
        Ok(tx.commit().await?)
    }

    /// The task's own cost budget, if one is set.
    pub async fn task_budget(&self, task: TaskId) -> Result<Option<f64>> {
        let mut tx = self.store.read().await?;
        tx.get_task(task).await?;
        task_budget(&mut tx, task).await
    }

    /// Sets (`Some`) or clears (`None`) the task's cost budget. It applies
    /// from the task's next agent step.
    pub async fn set_task_budget(&self, task: TaskId, cost_usd: Option<f64>) -> Result<()> {
        let cost_usd = checked(cost_usd, "cost_usd")?;
        let mut tx = self.store.begin().await?;
        tx.get_task(task).await?;
        tx.put_setting(&task_budget_key(task), &json!({ "cost_usd": cost_usd }))
            .await?;
        Ok(tx.commit().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_reset_monthly_and_the_tighter_wins() {
        let now = Utc.with_ymd_and_hms(2026, 10, 7, 13, 5, 0).unwrap();
        assert_eq!(
            month_start(now),
            Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
        );
        let december = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();
        assert_eq!(
            next_month_start(december),
            Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(tighter(Some(5.0), Some(1.0)), Some(1.0));
        assert_eq!(tighter(None, Some(1.0)), Some(1.0));
        assert_eq!(tighter(Some(5.0), None), Some(5.0));
        assert_eq!(tighter(None, None), None);
        assert!(over_cap(10.0, 9.99).is_none());
        assert_eq!(
            over_cap(10.0, 10.0).as_deref(),
            Some("Ecosystem spend $10.00 reached the $10.00 monthly cap")
        );
        assert!(checked(Some(-1.0), "monthly_usd").is_err());
        assert!(checked(Some(f64::NAN), "monthly_usd").is_err());
        assert_eq!(checked(Some(0.0), "x").unwrap(), Some(0.0));
    }
}
