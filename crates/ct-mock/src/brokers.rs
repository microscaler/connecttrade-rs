//! How each broker behaves behind Connect Trade, as far as the documents say, and our assumptions where they are
//! silent. Every assumption is marked `ASSUMPTION` and is to be checked against a paper account; the mock is
//! corrected, not the client, when they disagree.

use connecttrade::models::{ComplexOrderClass, Instrument, OrderBroker};

/// What happens on `PATCH` (replace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceMode {
    /// The broker replaces in place: the old order becomes `replaced`, a new one is working.
    Native,
    /// Cancel, then place, done by Connect Trade behind one `PATCH`. The old order can still fill in the gap.
    /// No profile uses it since 22 Nov 2026: Connect Trade confirmed it does not do this (see `Unsupported`). Kept
    /// for a broker that turns out to replace this way underneath.
    CancelThenPlace { gap_ms: u64 },
    /// The broker cannot modify an order (`TradeZero`). Connect Trade, 22 Nov 2026: `PATCH` answers `501`, and the
    /// client changes the order with `DELETE` and a new `POST` itself (docs/connect-trade-answers.md).
    Unsupported,
}

/// One broker's behaviour behind Connect Trade.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "independent facts about a broker, read one at a time")]
pub struct Profile {
    pub broker: OrderBroker,
    pub code: &'static str,
    /// Order classes accepted, and for which instruments.
    pub complex: &'static [ComplexOrderClass],
    pub complex_excluded_instruments: &'static [Instrument],
    pub replace: ReplaceMode,
    /// Whether `order_qty` can be changed by a replace.
    pub replace_quantity: bool,
    /// Trading calls (create, replace, cancel) per minute per account; `None` for no limit.
    /// ASSUMPTION: Connect Trade passes the broker's limit through as a 429.
    pub trading_calls_per_minute: Option<usize>,
    /// Plain stops only in regular hours (`TradeZero` rejects them outside with R79/R100).
    pub stops_regular_hours_only: bool,
    /// Fills of 2 or more shares arrive in two parts.
    pub partial_fills: bool,
    /// Connect Trade offers this broker's transactions (fees). `TradeZero` and Lightspeed: no.
    pub transactions: bool,
    /// Account number prefix for mock accounts.
    pub account_prefix: &'static str,
}

const ALL: &[ComplexOrderClass] = &[ComplexOrderClass::Bracket, ComplexOrderClass::Oto, ComplexOrderClass::Oco];

/// The profile of `broker`; brokers without one get a permissive default.
#[must_use]
pub fn profile(broker: &OrderBroker) -> Profile {
    let base = Profile {
        broker: broker.clone(),
        code: "OTHER",
        complex: &[],
        complex_excluded_instruments: &[],
        replace: ReplaceMode::Native,
        replace_quantity: true,
        trading_calls_per_minute: None,
        stops_regular_hours_only: false,
        partial_fills: true,
        transactions: true,
        account_prefix: "MK",
    };
    match broker {
        // No native groups or replace (TradeZero's own docs). ASSUMPTION: Connect Trade rejects groups rather than
        // emulating them. Place 10/s natively; well above what a test sends, so not limited here.
        OrderBroker::Tradezero => Profile {
            code: "TRADEZERO",
            replace: ReplaceMode::Unsupported,
            stops_regular_hours_only: true,
            transactions: false,
            account_prefix: "TZP",
            ..base
        },
        // Native OCO/OTOCO; Connect Trade's own example rejects OCO for crypto.
        OrderBroker::Tastytrade => Profile {
            code: "TASTYTRADE",
            complex: ALL,
            complex_excluded_instruments: &[Instrument::Crypto],
            account_prefix: "5WT",
            ..base
        },
        // Native OCO/OTO/OTOCO; no quantity change on replace; 60 trading calls a minute.
        // ASSUMPTION: a quantity replace is rejected rather than turned into cancel and place.
        OrderBroker::Tradier => Profile {
            code: "TRADIER",
            complex: ALL,
            replace_quantity: false,
            trading_calls_per_minute: Some(60),
            account_prefix: "VA",
            ..base
        },
        // Order model undocumented. ASSUMPTION: no groups.
        OrderBroker::Lightspeed => Profile { code: "LIGHTSPEED", transactions: false, account_prefix: "LS", ..base },
        // Connect Trade's spec example: "Broker ALPACA does not support order groups".
        OrderBroker::Alpaca => Profile { code: "ALPACA", account_prefix: "PA", ..base },
        _ => base,
    }
}

impl Profile {
    /// Whether this broker takes `class` orders for `instrument`.
    #[must_use]
    pub fn accepts(&self, class: &ComplexOrderClass, instrument: &Instrument) -> bool {
        self.complex.contains(class) && !self.complex_excluded_instruments.contains(instrument)
    }
}
