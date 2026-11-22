//! The book's state as API objects: orders, groups, positions, balances.

use super::{now_str, num, Book, Group, Order};
use connecttrade::models::{
    BalanceResponse, Fill, Instrument, OrderGroup, OrderGroupMember, OrderResponse, PositionResponse,
};
use connecttrade::stream::ServerMessage;
use rust_decimal::Decimal;

impl Book {
    /// Queue an order update for the stream.
    pub fn emit_order(&mut self, order_id: &str) {
        if let Some(o) = self.orders.get(order_id) {
            let msg = ServerMessage::Orders { data: Box::new(self.order_response(o)) };
            self.outbox.push((o.user_id.clone(), msg));
        }
    }

    /// Queue an account's positions and balance for the stream.
    pub fn emit_account(&mut self, user_id: &str, account_id: &str) {
        let positions = self.position_rows(Some(account_id));
        self.outbox.push((user_id.to_string(), ServerMessage::Positions { data: positions }));
        if let Some(b) = self.balance_row(account_id) {
            self.outbox.push((user_id.to_string(), ServerMessage::Balances { data: b }));
        }
    }

    /// An order as the API returns it.
    #[must_use]
    pub fn order_response(&self, o: &Order) -> OrderResponse {
        OrderResponse {
            user_id: Some(o.user_id.clone()),
            connection_id: Some(o.connection_id.clone()),
            account_id: o.account_id.clone(),
            order_id: o.order_id.clone(),
            account_number: Some(o.account_number.clone()),
            cl_order_id: Some(o.cl_order_id.clone()),
            platform: Some("mock".into()),
            broker: Some(o.broker.as_str().to_string()),
            normalized_status: Some(o.status.clone()),
            broker_status: Some(o.broker_status.clone()),
            side: Some(o.side.clone()),
            order_qty: Some(num(o.qty)),
            order_notional: None,
            order_type: Some(o.order_type.clone()),
            broker_order_type: Some(o.order_type.as_str().to_ascii_lowercase()),
            order_tif: Some(o.tif.clone()),
            order_limit_price: Some(o.limit.map_or_else(|| "0".into(), num)),
            price_effect: None,
            symbol: o.symbol.clone(),
            instrument: Some(o.instrument.clone()),
            exec_dest: None,
            entry_time: Some(o.entry_time.clone()),
            last_exec_time: o.last_exec.clone(),
            last_modified_time: Some(o.last_modified.clone()),
            stop_price: o.stop.map(num),
            broker_text: o.broker_text.clone(),
            fills: Some(
                o.fills
                    .iter()
                    .map(|f| Fill {
                        fill_id: Some(f.fill_id.clone()),
                        time: Some(f.time.clone()),
                        quantity: Some(num(f.qty)),
                        price: Some(num(f.price)),
                    })
                    .collect(),
            ),
            legs: None,
            order_class: Some(o.class.clone()),
            order_group_id: o.group_id.clone(),
            group_role: o.role.clone(),
            parent_order_id: o.parent.clone(),
            system_time: Some(now_str()),
        }
    }

    /// A group as the API returns it, members entry first.
    #[must_use]
    pub fn group_response(&self, g: &Group) -> OrderGroup {
        let account_number = self.accounts.get(&g.account_id).map(|a| a.account_number.clone());
        OrderGroup {
            user_id: Some(g.user_id.clone()),
            connection_id: Some(g.connection_id.clone()),
            account_id: g.account_id.clone(),
            account_number,
            platform: Some("mock".into()),
            broker: Some(g.broker.as_str().to_string()),
            order_group_id: g.group_id.clone(),
            order_class: Some(g.class.clone()),
            cl_order_id: Some(g.cl_order_id.clone()),
            group_status: Some(g.status.clone()),
            broker_data: None,
            created_time: Some(g.created.clone()),
            last_modified_time: Some(g.modified.clone()),
            orders: Some(
                g.members
                    .iter()
                    .filter_map(|m| self.orders.get(m))
                    .map(|o| OrderGroupMember {
                        group_role: o.role.clone(),
                        order_id: Some(o.order_id.clone()),
                        cl_order_id: Some(o.cl_order_id.clone()),
                        parent_order_id: o.parent.clone(),
                        side: Some(o.side.clone()),
                        order_qty: Some(num(o.qty)),
                        order_type: Some(o.order_type.clone()),
                        order_limit_price: o.limit.map(num),
                        stop_price: o.stop.map(num),
                        order_tif: Some(o.tif.clone()),
                        normalized_status: Some(o.status.clone()),
                        broker_status: Some(o.broker_status.clone()),
                    })
                    .collect(),
            ),
        }
    }

    /// Open positions, of one account or all.
    #[must_use]
    pub fn position_rows(&self, account: Option<&str>) -> Vec<PositionResponse> {
        self.positions
            .iter()
            .filter(|((a, _), p)| account.is_none_or(|x| x == a) && !p.qty.is_zero())
            .filter_map(|((a, sym), p)| {
                let acct = self.accounts.get(a)?;
                let last = self.price(sym);
                let upl = (last - p.avg) * p.qty;
                Some(PositionResponse {
                    connection_id: Some(acct.connection_id.clone()),
                    account_id: Some(a.clone()),
                    user_id: Some(acct.user_id.clone()),
                    account_number: Some(acct.account_number.clone()),
                    broker_position_id: None,
                    symbol: Some(sym.clone()),
                    quantity: Some(num(p.qty)),
                    last_price: Some(num(last)),
                    position_average_price: Some(num(p.avg)),
                    unrealized_pl: Some(num(upl)),
                    daily_pl: Some(num(upl)),
                    total_pl: Some(num(upl)),
                    instrument: Some(Instrument::Stk),
                    system_time: Some(now_str()),
                })
            })
            .collect()
    }

    /// One account's balance: cash, and buying power at twice equity.
    #[must_use]
    pub fn balance_row(&self, account_id: &str) -> Option<BalanceResponse> {
        let a = self.accounts.get(account_id)?;
        let held: Decimal = self
            .positions
            .iter()
            .filter(|((acc, _), _)| acc == account_id)
            .map(|((_, s), p)| p.qty * self.price(s))
            .sum();
        let equity = a.cash + held;
        Some(BalanceResponse {
            account_number: Some(a.account_number.clone()),
            connection_id: Some(a.connection_id.clone()),
            account_id: Some(a.account_id.clone()),
            user_id: Some(a.user_id.clone()),
            currency_code: Some("USD".into()),
            cash: Some(num(a.cash)),
            buying_power: Some(num(equity * Decimal::TWO)),
            initial_margin: Some("0".into()),
            maintenance_margin: Some("0".into()),
            reg_t_buying_power: Some(num(equity * Decimal::TWO)),
            system_time: Some(now_str()),
        })
    }
}
