//! Schedules: drift detection, recurring tasks and applies, and destroy
//! ("delete") schedules.
//!
//! A schedule has either a 5-field cron expression (UTC) or a one-shot `at`
//! time. The timer task sleeps until the earliest `next_run_at` and is woken
//! early whenever schedules change — there is no polling interval. If the
//! server was down when a schedule was due, it fires once at startup.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::app::AppRef;
use crate::util::{fmt_ts, new_id, now};

pub const KINDS: &[&str] = &["drift", "tracked", "task", "destroy"];

// ---------------------------------------------------------------- cron

/// A parsed 5-field cron expression: minute hour day-of-month month day-of-week.
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    minutes: BTreeSet<u8>,
    hours: BTreeSet<u8>,
    dom: BTreeSet<u8>,
    months: BTreeSet<u8>,
    dow: BTreeSet<u8>,
    dom_any: bool,
    dow_any: bool,
}

fn field(spec: &str, min: u8, max: u8, names: &[&str]) -> Result<(BTreeSet<u8>, bool)> {
    let mut out = BTreeSet::new();
    let any = spec == "*" || spec == "?";
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u8>().ok().filter(|s| *s > 0).with_context(|| format!("bad step in {part:?}"))?),
            None => (part, 1),
        };
        let val = |s: &str| -> Result<u8> {
            if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
                return Ok(i as u8 + min);
            }
            let v: u8 = s.parse().with_context(|| format!("bad value {s:?}"))?;
            // Day-of-week 7 is Sunday too.
            let v = if max == 6 && v == 7 { 0 } else { v };
            if v < min || v > max {
                bail!("{v} out of range {min}-{max}");
            }
            Ok(v)
        };
        let (lo, hi) = match range {
            "*" | "?" => (min, max),
            r => match r.split_once('-') {
                Some((a, b)) => (val(a)?, val(b)?),
                None if step > 1 => (val(r)?, max),
                None => {
                    let v = val(r)?;
                    (v, v)
                }
            },
        };
        if lo > hi {
            bail!("empty range {range:?}");
        }
        out.extend((lo..=hi).step_by(step as usize));
    }
    Ok((out, any))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron> {
        let expr = match expr.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            e => e,
        };
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            bail!("cron needs 5 fields (minute hour day-of-month month day-of-week), got {}", f.len());
        }
        const MON: &[&str] = &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        const DOW: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let (minutes, _) = field(f[0], 0, 59, &[])?;
        let (hours, _) = field(f[1], 0, 23, &[])?;
        let (dom, dom_any) = field(f[2], 1, 31, &[])?;
        let (months, _) = field(f[3], 1, 12, MON)?;
        let (dow, dow_any) = field(f[4], 0, 6, DOW)?;
        Ok(Cron { minutes, hours, dom, months, dow, dom_any, dow_any })
    }

    fn day_matches(&self, d: OffsetDateTime) -> bool {
        if !self.months.contains(&(d.month() as u8)) {
            return false;
        }
        let dom = self.dom.contains(&d.day());
        let dow = self.dow.contains(&d.weekday().number_days_from_sunday());
        // Classic cron: when both are restricted, either may match.
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// First matching minute strictly after `t`.
    pub fn next_after(&self, t: OffsetDateTime) -> Option<OffsetDateTime> {
        let start = t.replace_second(0).ok()?.replace_nanosecond(0).ok()? + time::Duration::minutes(1);
        let mut day = start.replace_time(time::Time::MIDNIGHT);
        for _ in 0..(366 * 5) {
            if self.day_matches(day) {
                for &h in &self.hours {
                    for &m in &self.minutes {
                        let c = day.replace_hour(h).ok()?.replace_minute(m).ok()?;
                        if c >= start {
                            return Some(c);
                        }
                    }
                }
            }
            day += time::Duration::days(1);
        }
        None
    }
}

pub fn parse_ts(s: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).with_context(|| format!("{s:?} is not an RFC 3339 time"))
}

// ---------------------------------------------------------------- storage

#[derive(Debug, Clone, Serialize)]
pub struct Schedule {
    pub id: String,
    pub stack_id: String,
    pub kind: String,
    pub cron: Option<String>,
    pub at: Option<String>,
    pub params: Value,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_run_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
}

const COLS: &str = "id, stack_id, kind, cron, at, params, next_run_at, last_run_at, last_run_id, last_error, created_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?,
        stack_id: r.get(1)?,
        kind: r.get(2)?,
        cron: r.get(3)?,
        at: r.get(4)?,
        params: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
        next_run_at: r.get(6)?,
        last_run_at: r.get(7)?,
        last_run_id: r.get(8)?,
        last_error: r.get(9)?,
        created_at: r.get(10)?,
    })
}

/// When a schedule fires next (None: never again).
pub fn next_time(cron: Option<&str>, at: Option<&str>, after: OffsetDateTime) -> Result<Option<OffsetDateTime>> {
    match (cron, at) {
        (Some(c), None) => Ok(Cron::parse(c)?.next_after(after)),
        (None, Some(a)) => {
            let t = parse_ts(a)?;
            Ok((t > after).then_some(t))
        }
        _ => bail!("give exactly one of cron or at"),
    }
}

pub fn create(c: &Connection, stack_id: &str, kind: &str, cron: Option<&str>, at: Option<&str>, params: &Value) -> Result<Schedule> {
    let next = next_time(cron, at, OffsetDateTime::now_utc() - time::Duration::seconds(1))?;
    let at = at.map(|a| parse_ts(a).map(fmt_ts)).transpose()?;
    let id = new_id("sch");
    c.execute(
        &format!("INSERT INTO schedules ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, ?8)"),
        params![id, stack_id, kind, cron, at, params.to_string(), next.map(fmt_ts), now()],
    )?;
    Ok(get(c, &id)?.expect("just inserted"))
}

pub fn get(c: &Connection, id: &str) -> Result<Option<Schedule>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM schedules WHERE id = ?1"))?.query_row([id], row).optional()?)
}

pub fn list(c: &Connection, stack_id: &str) -> Result<Vec<Schedule>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM schedules WHERE stack_id = ?1 ORDER BY created_at"))?;
    let rows = st.query_map([stack_id], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete(c: &Connection, stack_id: &str, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM schedules WHERE stack_id = ?1 AND id = ?2", params![stack_id, id])? > 0)
}

fn due(c: &Connection, at: &str) -> Result<Vec<Schedule>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM schedules WHERE next_run_at IS NOT NULL AND next_run_at <= ?1 ORDER BY next_run_at"))?;
    let rows = st.query_map([at], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn earliest(c: &Connection) -> Result<Option<String>> {
    Ok(c.query_row("SELECT MIN(next_run_at) FROM schedules", [], |r| r.get(0))?)
}

fn mark_fired(c: &Connection, s: &Schedule, run_id: Option<&str>, error: Option<&str>, next: Option<OffsetDateTime>) -> Result<()> {
    c.execute(
        "UPDATE schedules SET last_run_at = ?2, last_run_id = COALESCE(?3, last_run_id), last_error = ?4, next_run_at = ?5 WHERE id = ?1",
        params![s.id, now(), run_id, error, next.map(fmt_ts)],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- timer

/// Background task: fire due schedules, then sleep until the next one (or
/// until `app.schedule_changed` is notified).
pub async fn timer(app: AppRef) {
    loop {
        let fired = fire_due(&app).await;
        if let Err(e) = fired {
            tracing::warn!("schedules: {e:#}");
        }
        let wait = match app.db.read(earliest) {
            Ok(Some(t)) => parse_ts(&t)
                .map(|t| (t - OffsetDateTime::now_utc()).max(time::Duration::ZERO))
                .map(|d| Duration::from_millis(d.whole_milliseconds().clamp(0, 86_400_000) as u64))
                .unwrap_or(Duration::from_secs(60)),
            _ => Duration::from_secs(86_400),
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = app.schedule_changed.notified() => {}
        }
    }
}

async fn fire_due(app: &AppRef) -> Result<()> {
    let now_t = OffsetDateTime::now_utc();
    for s in app.db.read(|c| due(c, &fmt_ts(now_t)))? {
        // Next occurrence after now: a server that was down fires once, not once per missed slot.
        let next = if s.cron.is_some() { next_time(s.cron.as_deref(), None, now_t).ok().flatten() } else { None };
        match crate::api::schedules::fire(app, &s).await {
            Ok(run_id) => {
                tracing::info!(schedule = %s.id, kind = %s.kind, run = ?run_id, "schedule fired");
                app.db.write(|c| mark_fired(c, &s, run_id.as_deref(), None, next))?;
            }
            Err(e) => {
                let msg = e.detail();
                tracing::warn!(schedule = %s.id, "schedule failed: {msg}");
                app.db.write(|c| mark_fired(c, &s, None, Some(&msg), next))?;
            }
        }
    }
    Ok(())
}

pub fn params_json(reconcile: bool, command: Option<&[String]>, auto_apply: Option<bool>) -> Value {
    let mut p = json!({});
    if reconcile {
        p["reconcile"] = json!(true);
    }
    if let Some(c) = command {
        p["command"] = json!(c);
    }
    if let Some(a) = auto_apply {
        p["auto_apply"] = json!(a);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> OffsetDateTime {
        parse_ts(s).unwrap()
    }

    #[test]
    fn cron_next() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(fmt_ts(c.next_after(t("2026-01-01T10:07:30Z")).unwrap()), "2026-01-01T10:15:00.000Z");
        assert_eq!(fmt_ts(c.next_after(t("2026-01-01T10:15:00Z")).unwrap()), "2026-01-01T10:30:00.000Z");
        let c = Cron::parse("30 2 * * mon-fri").unwrap();
        // 2026-01-03 is a Saturday -> Monday the 5th.
        assert_eq!(fmt_ts(c.next_after(t("2026-01-03T00:00:00Z")).unwrap()), "2026-01-05T02:30:00.000Z");
        let c = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(fmt_ts(c.next_after(t("2026-03-01T00:00:00Z")).unwrap()), "2028-02-29T00:00:00.000Z");
        let c = Cron::parse("@daily").unwrap();
        assert_eq!(fmt_ts(c.next_after(t("2026-12-31T23:59:00Z")).unwrap()), "2027-01-01T00:00:00.000Z");
        // dom and dow both restricted: either matches (1st of month or any Sunday).
        let c = Cron::parse("0 12 1 * 0").unwrap();
        assert_eq!(fmt_ts(c.next_after(t("2026-01-02T00:00:00Z")).unwrap()), "2026-01-04T12:00:00.000Z");
        assert_eq!(Cron::parse("0 0 * * 7").unwrap(), Cron::parse("0 0 * * sun").unwrap());
    }

    #[test]
    fn cron_errors() {
        for bad in ["* * * *", "60 * * * *", "* 24 * * *", "*/0 * * * *", "5-1 * * * *", "x * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn one_shot() {
        let now = t("2026-01-01T00:00:00Z");
        assert!(next_time(None, Some("2025-01-01T00:00:00Z"), now).unwrap().is_none());
        assert!(next_time(None, Some("2026-06-01T00:00:00+02:00"), now).unwrap().is_some());
        assert!(next_time(Some("* * * * *"), Some("2026-06-01T00:00:00Z"), now).is_err());
    }
}
