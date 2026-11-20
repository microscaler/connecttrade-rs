//! The checks Connect Trade makes before an order reaches a broker, and the broker rules applied on arrival.

use super::{bad, err};
use crate::book::{Book, Order, Session};
use crate::brokers::Profile;
use axum::http::StatusCode;
use axum::response::Response;
use connecttrade::models::{
    ComplexOrderClass, ComplexOrderParameters, ConnectionStatus, Instrument, NormalizedStatus, OrderCreateRequest,
    OrderReplaceRequest, OrderType,
};

pub(crate) const BROKEN: &str = "Connection is BROKEN; repair it before trading";

fn check_ids(req: &OrderCreateRequest) -> Result<(), Response> {
    if req.cl_order_id.is_empty() || req.cl_order_id.len() > 36 {
        return Err(bad("INVALID_PARAMETERS: cl_order_id must be 1 to 36 characters"));
    }
    if req.order_qty.is_none() && req.order_notional.is_none() {
        return Err(bad("INVALID_PARAMETERS: order_qty or order_notional is required"));
    }
    Ok(())
}

fn check_prices(req: &OrderCreateRequest) -> Result<(), Response> {
    let oco = req.complex_order_parameters.as_ref().is_some_and(|c| c.order_class == ComplexOrderClass::Oco);
    if oco {
        if req.order_type.is_some() || req.order_limit_price.is_some() || req.stop_price.is_some() {
            return Err(bad("INVALID_PARAMETERS: OCO orders must omit order_type, order_limit_price and stop_price"));
        }
        return Ok(());
    }
    let t = req.order_type.as_ref().ok_or_else(|| bad("INVALID_PARAMETERS: order_type is required"))?;
    if matches!(t, OrderType::Lmt | OrderType::Stplmt) && req.order_limit_price.is_none() {
        return Err(bad("INVALID_PARAMETERS: order_limit_price is required for LMT and STPLMT"));
    }
    if matches!(t, OrderType::Stp | OrderType::Stplmt) && req.stop_price.is_none() {
        return Err(bad("INVALID_PARAMETERS: stop_price is required for STP and STPLMT"));
    }
    Ok(())
}

fn check_complex(c: Option<&ComplexOrderParameters>) -> Result<(), Response> {
    let Some(c) = c else { return Ok(()) };
    let both = c.take_profit.is_some() && c.stop_loss.is_some();
    let one = c.take_profit.is_some() != c.stop_loss.is_some();
    match c.order_class {
        ComplexOrderClass::Bracket | ComplexOrderClass::Oco if !both => {
            Err(bad("INVALID_PARAMETERS: BRACKET and OCO need both take_profit and stop_loss"))
        }
        ComplexOrderClass::Oto if !one => {
            Err(bad("INVALID_PARAMETERS: OTO needs exactly one of take_profit or stop_loss"))
        }
        _ => Ok(()),
    }
}

fn check_legs(req: &OrderCreateRequest) -> Result<(), Response> {
    if req.instrument.as_ref() == Some(&Instrument::MlegOpt) {
        let n = req.legs.as_ref().map_or(0, Vec::len);
        if !(2..=4).contains(&n) {
            return Err(bad("INVALID_PARAMETERS: multi-leg orders need 2 to 4 legs"));
        }
        return Ok(());
    }
    if req.symbol.is_none() {
        return Err(bad("INVALID_PARAMETERS: symbol is required"));
    }
    if req.side.is_none() {
        return Err(bad("INVALID_PARAMETERS: side is required"));
    }
    Ok(())
}

/// The checks Connect Trade makes before a create reaches the broker.
pub(crate) fn check_shape(req: &OrderCreateRequest) -> Result<(), Response> {
    check_ids(req)?;
    check_prices(req)?;
    check_complex(req.complex_order_parameters.as_ref())?;
    check_legs(req)
}

/// The broker rejects some orders after accepting them, as `TradeZero` does with plain stops outside regular hours.
pub(crate) fn session_rejection(p: &Profile, session: Session, t: Option<&OrderType>) -> Option<String> {
    if !p.stops_regular_hours_only || session == Session::Regular {
        return None;
    }
    match t {
        Some(OrderType::Stp) => Some("R79: Stop orders are not allowed at this time".into()),
        Some(OrderType::Mkt) => Some("R78: Market orders are not allowed at this time".into()),
        _ => None,
    }
}

pub(crate) fn check_replace(
    b: &Book,
    o: &Order,
    aid: &str,
    req: &OrderReplaceRequest,
    p: &Profile,
) -> Result<(), Response> {
    if connection_broken(b, &o.connection_id) {
        return Err(err(StatusCode::CONFLICT, BROKEN));
    }
    if o.group_id.is_some() {
        return Err(bad("GROUP_LEG_NOT_REPLACEABLE: a member of an order group cannot be replaced"));
    }
    if req.broker != o.broker {
        return Err(bad("INVALID_PARAMETERS: broker does not match the order"));
    }
    if !matches!(o.status, NormalizedStatus::Pending | NormalizedStatus::Working | NormalizedStatus::PartiallyFilled) {
        return Err(bad(format!("Order {} cannot be replaced ({})", o.order_id, o.status)));
    }
    if req.new_cl_order_id.is_empty() || req.new_cl_order_id.len() > 36 {
        return Err(bad("INVALID_PARAMETERS: new_cl_order_id must be 1 to 36 characters"));
    }
    if b.cl_ids.contains_key(&(aid.to_string(), req.new_cl_order_id.clone())) {
        return Err(err(StatusCode::CONFLICT, format!("Duplicate cl_order_id {}", req.new_cl_order_id)));
    }
    if req.order_qty.is_some() && !p.replace_quantity {
        let code = p.code;
        return Err(bad(format!(
            "INVALID_PARAMETERS: {code} cannot change an order's quantity; cancel and place a new order"
        )));
    }
    Ok(())
}

/// True when the order's connection must be repaired before it trades again.
pub(crate) fn connection_broken(b: &Book, connection_id: &str) -> bool {
    b.connections.get(connection_id).is_some_and(|c| c.status == ConnectionStatus::Broken)
}
