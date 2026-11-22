//! What a broker charges, for brokers whose fees Connect Trade does not report (`TradeZero`: no transactions). The
//! mock charges these to the account's cash, so a client that models the same schedule reconciles with it.
//!
//! `TradeZero` America's published schedule (tradezero.com, Pricing & Fees and the fee schedule PDF, read
//! 22 Nov 2026), on its default smart route:
//!
//! - Commission: none on NYSE, AMEX and NASDAQ stocks of $1 or more from 07:00 to 20:00 ET. $0.005 a share from
//!   04:00 to 07:00 ET, and on stocks under $1 (then at least $0.99 and at most $7.95 an order).
//! - Regulatory reporting fee: $0.005 per 100 shares on every order, rounded up to the cent.
//! - On sells, passed through: FINRA's trading activity fee per share (capped per trade) and the SEC's section 31
//!   fee on the dollar amount. Their rates change by date, and the tables below follow FINRA's and the SEC's
//!   published rates rather than the PDF's, which still shows 2025's TAF.
//!
//! ASSUMPTIONS, to be checked on the paper account: each part is per order and rounded up to the cent; the
//! minimum and maximum apply to the order's whole commission; OTC and direct routes (ECN fees) are not modelled.

use crate::book::FillRec;
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use rust_decimal::Decimal;

const fn dec(num: u32, scale: u32) -> Decimal {
    Decimal::from_parts(num, 0, 0, false, scale)
}

/// $0.005 a share.
const PER_SHARE: Decimal = dec(5, 3);
/// Sub-$1 orders: at least $0.99, at most $7.95.
const MIN_SUB_DOLLAR: Decimal = dec(99, 2);
const MAX_SUB_DOLLAR: Decimal = dec(795, 2);
/// $0.005 per 100 shares.
const REPORTING_PER_SHARE: Decimal = dec(5, 5);

/// FINRA's trading activity fee on covered equity sales, per share and the most per trade, from 1 January of
/// each year (SR-FINRA-2024-019 fee adjustment schedule).
fn taf(year: i32) -> (Decimal, Decimal) {
    match year {
        ..=2025 => (dec(166, 6), dec(830, 2)),
        2026 => (dec(195, 6), dec(979, 2)),
        2027 => (dec(232, 6), dec(1161, 2)),
        2028 => (dec(240, 6), dec(1205, 2)),
        _ => (dec(249, 6), dec(1250, 2)),
    }
}

/// The SEC's section 31 fee per dollar sold (fee rate advisories: $27.80 per million, $0.00 from 14 May 2025,
/// $20.60 from 4 April 2026).
fn sec_rate(day: NaiveDate) -> Decimal {
    let from = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MAX);
    if day >= from(2026, 4, 4) {
        dec(2060, 8)
    } else if day >= from(2025, 5, 14) {
        Decimal::ZERO
    } else {
        dec(2780, 8)
    }
}

/// Up to the next cent.
fn cent_up(x: Decimal) -> Decimal {
    (x * Decimal::ONE_HUNDRED).ceil() / Decimal::ONE_HUNDRED
}

/// The n-th `weekday` of a month (1-based), as a day of the month.
fn nth(year: i32, month: u32, weekday: Weekday, n: u32) -> u32 {
    let first = NaiveDate::from_ymd_opt(year, month, 1).map_or(0, |d| d.weekday().num_days_from_sunday());
    let want = weekday.num_days_from_sunday();
    1 + (7 + want - first) % 7 + 7 * (n - 1)
}

/// Minutes past midnight, New York time: daylight time from 02:00 on the second Sunday of March (07:00 UTC) to
/// 02:00 on the first Sunday of November (06:00 UTC).
#[must_use]
pub fn new_york_minutes(t: DateTime<Utc>) -> u32 {
    let y = t.year();
    let at = |m, d, h| NaiveDate::from_ymd_opt(y, m, d).and_then(|x| x.and_hms_opt(h, 0, 0)).map(|x| x.and_utc());
    let summer_from = at(3, nth(y, 3, Weekday::Sun, 2), 7);
    let summer_to = at(11, nth(y, 11, Weekday::Sun, 1), 6);
    let summer = matches!((summer_from, summer_to), (Some(a), Some(b)) if t >= a && t < b);
    let offset = if summer { 4 * 60 } else { 5 * 60 };
    let utc = t.hour() * 60 + t.minute();
    (utc + 24 * 60 - offset) % (24 * 60)
}

/// What `TradeZero` charges for an order with these fills: commission, the reporting fee, and on a sell the TAF
/// and the SEC fee. `None` when a fill's time cannot be read.
#[must_use]
pub fn tradezero(sell: bool, fills: &[FillRec]) -> Option<Decimal> {
    if fills.is_empty() {
        return Some(Decimal::ZERO);
    }
    let (mut qty, mut notional, mut charged, mut sub_dollar) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, false);
    let mut first: Option<DateTime<Utc>> = None;
    for f in fills {
        let t = DateTime::parse_from_rfc3339(&f.time).ok()?.with_timezone(&Utc);
        first = Some(first.map_or(t, |x| x.min(t)));
        qty += f.qty;
        notional += f.qty * f.price;
        let under = f.price < Decimal::ONE;
        sub_dollar |= under;
        if under || new_york_minutes(t) < 7 * 60 {
            charged += f.qty;
        }
    }
    let mut commission = cent_up(PER_SHARE * charged);
    if sub_dollar {
        commission = commission.clamp(MIN_SUB_DOLLAR, MAX_SUB_DOLLAR);
    }
    let mut total = commission + cent_up(REPORTING_PER_SHARE * qty);
    if sell {
        let day = first?.date_naive();
        let (per_share, most) = taf(day.year());
        total += cent_up((per_share * qty).min(most)) + cent_up(notional * sec_rate(day));
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(time: &str, qty: i64, price: Decimal) -> FillRec {
        FillRec { fill_id: String::new(), time: time.into(), qty: Decimal::from(qty), price }
    }

    #[test]
    fn the_schedules_own_examples() {
        // Buy 100 IBM $118 @ 6 AM ET: $0.50 (and the reporting fee, $0.01)
        assert_eq!(tradezero(false, &[fill("2026-06-01T10:00:00Z", 100, dec(118, 0))]), Some(dec(51, 2)));
        // Buy 100 SHIP $0.90: $0.99
        assert_eq!(tradezero(false, &[fill("2026-06-01T15:00:00Z", 100, dec(90, 2))]), Some(dec(100, 2)));
        // Buy 200,000 FNMA $0.80: $7.95, and $10.00 reporting (2,000 hundreds at $0.005)
        assert_eq!(tradezero(false, &[fill("2026-06-01T15:00:00Z", 200_000, dec(80, 2))]), Some(dec(1795, 2)));
    }

    #[test]
    fn a_listed_stock_in_the_day_pays_only_the_pass_throughs() {
        let buy = [fill("2026-06-01T14:00:00Z", 100, dec(50, 0))];
        assert_eq!(tradezero(false, &buy), Some(dec(1, 2)));
        // sell $5,000: TAF 0.0195 -> 0.02, SEC 0.103 -> 0.11, reporting 0.01
        let sell = [fill("2026-06-01T14:00:00Z", 60, dec(50, 0)), fill("2026-06-01T14:00:01Z", 40, dec(50, 0))];
        assert_eq!(tradezero(true, &sell), Some(dec(14, 2)));
        // before 4 April 2026 the SEC rate was nil, and 2025's TAF applied
        assert_eq!(tradezero(true, &[fill("2025-12-01T15:00:00Z", 100, dec(50, 0))]), Some(dec(3, 2)));
    }

    #[test]
    fn new_york_time_follows_daylight_saving() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert_eq!(new_york_minutes(t("2026-01-15T12:00:00Z")), 7 * 60);
        assert_eq!(new_york_minutes(t("2026-07-15T12:00:00Z")), 8 * 60);
        assert_eq!(new_york_minutes(t("2026-03-08T06:59:00Z")), 60 + 59); // 01:59 EST
        assert_eq!(new_york_minutes(t("2026-03-08T07:00:00Z")), 3 * 60); // 03:00 EDT
        assert_eq!(new_york_minutes(t("2026-11-01T05:59:00Z")), 60 + 59); // 01:59 EDT
        assert_eq!(new_york_minutes(t("2026-11-01T06:00:00Z")), 60); // 01:00 EST
        assert_eq!(new_york_minutes(t("2026-06-01T02:00:00Z")), 22 * 60);
    }
}
