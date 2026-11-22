//! The order engine: acknowledgements, cancels, replaces, stop triggers, fills, and the group rules that follow.

use super::{id, now_str, Book, FillRec, Order, Session, Txn, FEE_PER_SHARE};
use crate::brokers::{profile, Profile, ReplaceMode};
use connecttrade::models::{
    GroupRole, GroupStatus, NormalizedStatus, OrderBroker, OrderTif, OrderType, TransactionType,
};
use rust_decimal::Decimal;
use std::time::{Duration, Instant};

/// The first part of a partial fill: 60% of the quantity.
const FIRST_PART: Decimal = Decimal::from_parts(6, 0, 0, false, 1);

impl Book {
    /// One engine step: acknowledgements, cancels, replaces, triggers and fills.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let ids: Vec<String> = self.order_seq.clone();
        for oid in ids {
            let Some(o) = self.orders.get(&oid).cloned() else { continue };
            // cancel-then-place replace: when the cancel lands, settle the replacement, also when the old order
            // filled in the gap (it is then no longer open, so this comes before that check)
            if let Some((new_id, at)) = &o.replaced_by {
                if now >= *at || !o.open() {
                    self.finish_replace(&oid, new_id);
                    continue;
                }
                // until then the old order can still fill
            }
            if o.contingent || !o.open() {
                continue;
            }
            if o.status == NormalizedStatus::PendingCancel {
                if o.cancel_at.is_some_and(|t| now >= t) {
                    self.set_status(&oid, NormalizedStatus::Cancelled, "cancelled");
                    self.after_terminal(&oid);
                }
                continue;
            }
            if o.status == NormalizedStatus::Pending {
                if now < o.ack_at {
                    continue;
                }
                if let Some(text) = o.reject {
                    if let Some(x) = self.orders.get_mut(&oid) {
                        x.broker_text = Some(text);
                    }
                    self.set_status(&oid, NormalizedStatus::Rejected, "rejected");
                    self.after_terminal(&oid);
                    continue;
                }
                self.set_status(&oid, NormalizedStatus::Working, "accepted");
            }
            // on a tape, only a trade fills
            if !self.tape {
                self.try_fill(&oid, now);
            }
        }
    }

    /// Whether `o` may trade in the session the market is in: outside regular hours only an order whose time in
    /// force says so (`DAY+`, `GTC+`).
    fn in_session(&self, o: &Order) -> bool {
        self.session == Session::Regular || matches!(o.tif, OrderTif::DayPlus | OrderTif::GtcPlus)
    }

    /// The market trades once at `price` (`POST /mock/trade`): every order the price reaches fills, oldest first,
    /// for at most `size` shares between them (no limit when `None`). Orders a fill releases (a bracket's exits)
    /// wait for the next trade. A profile that fills in parts fills each order in two parts, both at this trade.
    pub fn trade(&mut self, symbol: &str, price: Decimal, size: Option<Decimal>) {
        self.prices.insert(symbol.to_string(), price);
        // what is due now (acknowledgements, cancels, replaces) happens before the trade
        self.tick();
        let candidates: Vec<String> = self
            .order_seq
            .iter()
            .filter(|oid| {
                self.orders.get(*oid).is_some_and(|o| {
                    o.symbol.as_deref() == Some(symbol)
                        && !o.contingent
                        && matches!(
                            o.status,
                            NormalizedStatus::Working
                                | NormalizedStatus::PartiallyFilled
                                | NormalizedStatus::PendingReplace
                        )
                })
            })
            .cloned()
            .collect();
        let mut left = size;
        for oid in candidates {
            if left.is_some_and(|l| l <= Decimal::ZERO) {
                break;
            }
            let Some(o) = self.orders.get(&oid).cloned() else { continue };
            if !self.in_session(&o) {
                continue;
            }
            let Some(px) = self.fill_price(&oid, &o) else { continue };
            let remaining = o.qty - o.filled_qty();
            let qty = left.map_or(remaining, |l| l.min(remaining));
            if qty <= Decimal::ZERO {
                continue;
            }
            let p = profile(&o.broker);
            let first = if p.partial_fills && o.fills.is_empty() && qty >= Decimal::TWO {
                (qty * FIRST_PART).floor()
            } else {
                qty
            };
            self.fill(&oid, first, px, &p);
            if first < qty {
                self.fill(&oid, qty - first, px, &p);
            }
            left = left.map(|l| l - qty);
        }
    }

    /// A new trading day: every open day order expires (`DAY`, `DAY+`, and what came without a time in force).
    pub fn new_day(&mut self) {
        let expiring: Vec<String> = self
            .order_seq
            .iter()
            .filter(|oid| {
                self.orders.get(*oid).is_some_and(|o| o.open() && !matches!(o.tif, OrderTif::Gtc | OrderTif::GtcPlus))
            })
            .cloned()
            .collect();
        for oid in expiring {
            self.set_status(&oid, NormalizedStatus::Expired, "expired");
            self.after_terminal(&oid);
        }
    }

    /// The price an order would fill at now, or `None` when it would not fill. Triggers a stop as a side effect.
    fn fill_price(&mut self, oid: &str, o: &Order) -> Option<Decimal> {
        let px = self.price(o.symbol.as_deref()?);
        let buy = o.buys();
        if !o.triggered && matches!(o.order_type, OrderType::Stp | OrderType::Stplmt) {
            let stop = o.stop.unwrap_or(px);
            let triggered = if buy { px >= stop } else { px <= stop };
            if !triggered {
                return None;
            }
            if let Some(x) = self.orders.get_mut(oid) {
                x.triggered = true;
            }
        }
        match o.order_type {
            OrderType::Mkt | OrderType::Stp => Some(px),
            OrderType::Lmt | OrderType::Stplmt => {
                let l = o.limit.unwrap_or(px);
                match (buy, px <= l, px >= l) {
                    (true, true, _) => Some(px.min(l)),
                    (false, _, true) => Some(px.max(l)),
                    _ => None,
                }
            }
            OrderType::Unknown(_) => None,
        }
    }

    fn try_fill(&mut self, oid: &str, now: Instant) {
        let Some(o) = self.orders.get(oid).cloned() else { return };
        let fillable = matches!(
            o.status,
            NormalizedStatus::Working | NormalizedStatus::PartiallyFilled | NormalizedStatus::PendingReplace
        );
        if !fillable || !self.in_session(&o) || self.next_partial.get(oid).is_some_and(|t| now < *t) {
            return;
        }
        let Some(price) = self.fill_price(oid, &o) else { return };
        let p = profile(&o.broker);
        let remaining = o.qty - o.filled_qty();
        let part = if p.partial_fills && o.fills.is_empty() && remaining >= Decimal::TWO {
            (remaining * FIRST_PART).floor()
        } else {
            remaining
        };
        self.fill(oid, part, price, &p);
        if part < remaining {
            self.next_partial.insert(oid.to_string(), now + self.timings.partial_gap);
        }
    }

    fn fill(&mut self, oid: &str, qty: Decimal, price: Decimal, p: &Profile) {
        let t = now_str();
        let Some(o) = self.orders.get_mut(oid) else { return };
        let before = o.fills.clone();
        o.fills.push(FillRec { fill_id: id(), time: t.clone(), qty, price });
        // TradeZero's fees are its schedule's, per order: this fill pays what the order now owes less what it paid
        let fee = if p.broker == OrderBroker::Tradezero {
            let sell = !o.buys();
            let owed = |f: &[FillRec]| crate::fees::tradezero(sell, f).unwrap_or(Decimal::ZERO);
            owed(&o.fills) - owed(&before)
        } else {
            FEE_PER_SHARE * qty
        };
        o.last_exec = Some(t.clone());
        o.last_modified.clone_from(&t);
        let done = o.filled_qty() >= o.qty;
        o.status = if done { NormalizedStatus::Filled } else { NormalizedStatus::PartiallyFilled };
        o.broker_status = if done { "filled".into() } else { "partially_filled".into() };
        let (account_id, symbol, buy, user_id) =
            (o.account_id.clone(), o.symbol.clone().unwrap_or_default(), o.buys(), o.user_id.clone());

        let signed = if buy { qty } else { -qty };
        self.book_position(&account_id, &symbol, signed, price);
        if let Some(a) = self.accounts.get_mut(&account_id) {
            a.cash -= signed * price + fee;
        }
        if p.transactions {
            self.book_transactions(oid, &account_id, &symbol, signed, price, fee, &t);
        }
        self.emit_order(oid);
        self.emit_account(&user_id, &account_id);
        if done {
            self.after_terminal(oid);
        }
    }

    fn book_position(&mut self, account_id: &str, symbol: &str, signed: Decimal, price: Decimal) {
        let pos = self.positions.entry((account_id.to_string(), symbol.to_string())).or_default();
        let adding = pos.qty.is_zero() || pos.qty.is_sign_positive() == signed.is_sign_positive();
        if adding {
            let total = pos.qty.abs() + signed.abs();
            pos.avg = (pos.avg * pos.qty.abs() + price * signed.abs()).checked_div(total).unwrap_or(price);
        }
        pos.qty += signed;
        if pos.qty.is_zero() {
            pos.avg = Decimal::ZERO;
        }
    }

    #[allow(clippy::too_many_arguments, reason = "one fill's facts, passed once from fill()")]
    fn book_transactions(
        &mut self,
        oid: &str,
        account_id: &str,
        symbol: &str,
        signed: Decimal,
        price: Decimal,
        fee: Decimal,
        time: &str,
    ) {
        let side = if signed.is_sign_positive() { "BUY" } else { "SELL" };
        self.txns.push(Txn {
            id: id(),
            account_id: account_id.to_string(),
            kind: TransactionType::Fill,
            symbol: Some(symbol.to_string()),
            qty: Some(signed.abs()),
            price: Some(price),
            amount: -signed * price,
            time: time.to_string(),
            order_id: Some(oid.to_string()),
            side: Some(side.into()),
        });
        self.txns.push(Txn {
            id: id(),
            account_id: account_id.to_string(),
            kind: TransactionType::Fee,
            symbol: Some(symbol.to_string()),
            qty: None,
            price: None,
            amount: -fee,
            time: time.to_string(),
            order_id: Some(oid.to_string()),
            side: None,
        });
    }

    /// Change an order's status and queue the update.
    pub fn set_status(&mut self, oid: &str, s: NormalizedStatus, broker_status: &str) {
        if let Some(o) = self.orders.get_mut(oid) {
            o.status = s;
            o.broker_status = broker_status.into();
            o.last_modified = now_str();
        }
        self.emit_order(oid);
    }

    /// After an order reaches a final state: release a bracket's exits, cancel the other side of an OCO, and keep
    /// the group's status.
    fn after_terminal(&mut self, oid: &str) {
        let Some(o) = self.orders.get(oid).cloned() else { return };
        let Some(gid) = o.group_id else { return };
        let Some(members) = self.groups.get(&gid).map(|g| g.members.clone()) else { return };
        let group_status = match (&o.role, &o.status) {
            (Some(GroupRole::Entry), NormalizedStatus::Filled) => {
                self.release_exits(&members);
                Some(GroupStatus::Active)
            }
            (
                Some(GroupRole::Entry),
                NormalizedStatus::Cancelled | NormalizedStatus::Rejected | NormalizedStatus::Expired,
            ) => {
                self.cancel_open(&members, oid, |_| true);
                Some(if o.status == NormalizedStatus::Rejected {
                    GroupStatus::Rejected
                } else {
                    GroupStatus::Cancelled
                })
            }
            (Some(GroupRole::TakeProfit | GroupRole::StopLoss), NormalizedStatus::Filled) => {
                self.cancel_open(&members, oid, |x| {
                    matches!(x.role, Some(GroupRole::TakeProfit | GroupRole::StopLoss))
                });
                Some(GroupStatus::Completed)
            }
            _ => self.settled_status(&members),
        };
        if let (Some(s), Some(g)) = (group_status, self.groups.get_mut(&gid)) {
            g.status = s;
            g.modified = now_str();
        }
    }

    fn release_exits(&mut self, members: &[String]) {
        let ack = Instant::now() + self.timings.ack;
        for m in members {
            if let Some(x) = self.orders.get_mut(m).filter(|x| x.contingent) {
                x.contingent = false;
                x.ack_at = ack;
                x.broker_status = "pending".into();
            }
        }
    }

    fn cancel_open(&mut self, members: &[String], except: &str, which: impl Fn(&Order) -> bool) {
        for m in members {
            let cancel = m != except && self.orders.get(m).is_some_and(|x| x.open() && which(x));
            if cancel {
                self.set_status(m, NormalizedStatus::Cancelled, "cancelled");
            }
        }
    }

    /// Completed or cancelled once nothing in the group is open; `None` while something is.
    fn settled_status(&self, members: &[String]) -> Option<GroupStatus> {
        let all_closed = members.iter().all(|m| self.orders.get(m).is_some_and(|x| !x.open()));
        if !all_closed {
            return None;
        }
        let any_fill = members.iter().any(|m| self.orders.get(m).is_some_and(|x| x.status == NormalizedStatus::Filled));
        Some(if any_fill { GroupStatus::Completed } else { GroupStatus::Cancelled })
    }

    fn finish_replace(&mut self, old: &str, new: &str) {
        let (filled, part_filled) = match self.orders.get_mut(old) {
            Some(o) => {
                o.replaced_by = None;
                (o.status == NormalizedStatus::Filled, o.filled_qty())
            }
            None => (false, Decimal::ZERO),
        };
        let now = Instant::now();
        if filled {
            // the original filled during the gap: the replacement is not placed
            if let Some(n) = self.orders.get_mut(new) {
                n.reject = Some("Original order filled before the replacement could be placed".into());
                n.ack_at = now;
                n.contingent = false;
            }
            return;
        }
        self.set_status(old, NormalizedStatus::Replaced, "replaced");
        if let Some(n) = self.orders.get_mut(new) {
            // what filled on the original is not placed again
            n.qty -= part_filled;
            n.contingent = false;
            n.ack_at = now;
        }
    }

    /// Replace `old` with `new` the way the broker does it; returns the new order's id.
    pub fn start_replace(&mut self, old: &str, mut new: Order, mode: ReplaceMode) -> String {
        let new_id = new.order_id.clone();
        match mode {
            // Unsupported is answered 501 before a replace starts (rest::orders::replace_order); native if reached
            ReplaceMode::Native | ReplaceMode::Unsupported => {
                new.qty -= self.orders.get(old).map_or(Decimal::ZERO, Order::filled_qty);
                self.set_status(old, NormalizedStatus::Replaced, "replaced");
                self.insert_order(new);
            }
            ReplaceMode::CancelThenPlace { gap_ms } => {
                new.contingent = true;
                let at = Instant::now() + Duration::from_millis(gap_ms);
                if let Some(o) = self.orders.get_mut(old) {
                    o.replaced_by = Some((new_id.clone(), at));
                    o.status = NormalizedStatus::PendingReplace;
                    o.broker_status = "pending_cancel".into();
                }
                self.emit_order(old);
                self.insert_order(new);
            }
        }
        new_id
    }

    /// Add an order and queue its first update.
    pub fn insert_order(&mut self, o: Order) {
        let oid = o.order_id.clone();
        self.cl_ids.insert((o.account_id.clone(), o.cl_order_id.clone()), oid.clone());
        self.order_seq.push(oid.clone());
        self.orders.insert(oid.clone(), o);
        self.emit_order(&oid);
    }
}
