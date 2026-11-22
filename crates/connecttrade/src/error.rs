//! Errors, shaped around the one question an order path must answer: did the change happen?

use crate::models::ErrorResponse;

/// Connect Trade's error codes that a caller acts on. Connect Trade puts them at the start of `message`
/// (`"UNSUPPORTED_ORDER_CLASS: TASTYTRADE does not support OCO orders for CRYPTO."`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The broker or instrument does not take this bracket, OTO or OCO.
    UnsupportedOrderClass,
    /// A member of a bracket or OCO group cannot be replaced; replace means cancelling the whole group.
    GroupLegNotReplaceable,
    /// A member of a group cannot be cancelled on its own; cancel the group.
    GroupLegNotCancellable,
    InvalidParameters,
    /// Any other upper-case code at the start of the message.
    Other(String),
}

impl ErrorCode {
    fn parse(code: &str) -> ErrorCode {
        match code {
            "UNSUPPORTED_ORDER_CLASS" => ErrorCode::UnsupportedOrderClass,
            "GROUP_LEG_NOT_REPLACEABLE" => ErrorCode::GroupLegNotReplaceable,
            "GROUP_LEG_NOT_CANCELLABLE" => ErrorCode::GroupLegNotCancellable,
            "INVALID_PARAMETERS" => ErrorCode::InvalidParameters,
            other => ErrorCode::Other(other.to_string()),
        }
    }
}

/// A non-2xx answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub operation: &'static str,
    pub status: u16,
    /// The code at the start of the message, or in a `code` field.
    pub code: Option<ErrorCode>,
    pub message: String,
    /// The body, credentials redacted and capped (#17), for logs.
    pub body: String,
    /// How long to wait before asking again, from `Retry-After` on a 429 or 503 (#19).
    pub retry_after: Option<std::time::Duration>,
}

impl ApiError {
    pub(crate) fn from_response(operation: &'static str, status: u16, body: &[u8]) -> ApiError {
        // never a credential, never more than redact::KEEP bytes (#17)
        let kept = crate::redact::body(operation, body);
        let parsed: Option<ErrorResponse> = serde_json::from_slice(body).ok();
        let message = parsed
            .as_ref()
            .and_then(|e| e.message.clone())
            .map_or_else(|| kept.text.clone(), |m| crate::redact::scrub(&m, &kept.secrets));
        let code = parsed
            .as_ref()
            .and_then(|e| e.code.clone())
            .or_else(|| leading_code(&message))
            .map(|c| ErrorCode::parse(&c));
        ApiError { operation, status, code, message, body: kept.text, retry_after: None }
    }

    /// `409` on a create, replace or cancel: the duplicate `cl_order_id` case or a `BROKEN` connection. The two are
    /// told apart by the message; a broken connection needs the user to log in at the broker again
    /// (`repair_connection`), never a retry.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        self.status == 409
    }

    /// `409` because the account's broker connection is `BROKEN`: the user must log in at the broker again. Read from
    /// Connect Trade's wording ("Connection is BROKEN") or a `BROKEN` / `CONNECTION_BROKEN` code - never from the
    /// word anywhere in the message, which echoes the caller's own `cl_order_id` (#14).
    #[must_use]
    pub fn is_broken_connection(&self) -> bool {
        self.status == 409
            && (matches!(&self.code, Some(ErrorCode::Other(c)) if c == "BROKEN" || c == "CONNECTION_BROKEN")
                || self.message.to_ascii_lowercase().contains("connection is broken"))
    }

    /// `409` for a `cl_order_id` already recorded on the account: the order (or an earlier send of it) is there.
    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        self.status == 409 && !self.is_broken_connection()
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429
    }

    /// `501` on a `PATCH`: the broker cannot modify an order. Change it with a cancel and a new create instead
    /// (Connect Trade, 22 Nov 2026; docs/connect-trade-answers.md).
    #[must_use]
    pub fn is_not_supported(&self) -> bool {
        self.status == 501
    }
}

impl std::fmt::Display for ApiError {
    /// One line: Connect Trade's text with control characters escaped, capped (#22).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.operation, self.status, crate::redact::one_line(&self.message))
    }
}

/// `Retry-After`: a number of seconds, or an HTTP date (IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`) as the time
/// left until then (zero once past).
pub(crate) fn parse_retry_after(value: &str) -> Option<std::time::Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(std::time::Duration::from_secs(secs));
    }
    let at = http_date(v)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(std::time::Duration::from_secs(at.saturating_sub(now)))
}

/// Seconds since the epoch of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date(v: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let rest = v.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|m| *m == month_name)? + 1).ok()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let (h, m, sec): (i64, i64, i64) =
        (hms.next()?.parse().ok()?, hms.next()?.parse().ok()?, hms.next()?.parse().ok()?);
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    // days from 1970-01-01 to the civil date (Howard Hinnant's days_from_civil)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3_600 + m * 60 + sec).ok()
}

/// `"ABC_DEF: text"` or a bare `"ABC_DEF"` → `Some("ABC_DEF")`. The docs show both forms.
fn leading_code(message: &str) -> Option<String> {
    let head = message.split_once(':').map_or(message, |(h, _)| h).trim();
    let ok = head.len() >= 6
        && head.contains('_')
        && head.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
    ok.then(|| head.to_string())
}

/// How a transport failed. The variants differ in the only thing that matters for a change: whether the request may
/// have reached Connect Trade.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// Nothing was sent (resolve, connect or TLS failed). Safe to retry.
    #[error("not sent: {0}")]
    NotSent(String),
    /// Sent, then no answer in time. A change may have happened.
    #[error("timed out after sending: {0}")]
    Timeout(String),
    /// Sent, then the connection broke. A change may have happened.
    #[error("connection lost after sending: {0}")]
    Broken(String),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("{operation}: needs {missing}")]
    Credentials { operation: &'static str, missing: &'static str },
    /// Credentials would go over plain HTTP to a host that is not this machine; nothing was sent (#16).
    #[error("{operation}: refusing to send credentials in clear to {host}; use https://")]
    Insecure { operation: &'static str, host: String },
    /// The request body could not be encoded; nothing was sent.
    #[error("{operation}: body did not encode: {reason}")]
    Encode { operation: &'static str, reason: String },
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("{0}")]
    Api(ApiError),
    /// A 2xx answer that did not decode; `body` is the answer with credentials redacted and capped.
    #[error("{operation}: {status} answer did not decode: {}", crate::redact::one_line(.reason))]
    Decode { operation: &'static str, status: u16, reason: String, body: String },
}

impl Error {
    /// For a change (create, replace, cancel): true when it may have happened even though this call failed.
    /// The caller must then look the order up by its `cl_order_id` before doing anything else.
    #[must_use]
    pub fn may_have_happened(&self) -> bool {
        matches!(self, Error::Transport(TransportError::Timeout(_) | TransportError::Broken(_)) | Error::Decode { .. })
            // a 501 is Connect Trade saying the broker cannot do the change at all: nothing happened
            || matches!(self, Error::Api(e) if e.status >= 500 && e.status != 501)
    }

    #[must_use]
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Error::Api(e) => Some(e),
            _ => None,
        }
    }

    #[must_use]
    pub fn code(&self) -> Option<&ErrorCode> {
        self.api().and_then(|e| e.code.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_read_from_the_message() {
        let e = ApiError::from_response(
            "create_order",
            400,
            br#"{"message":"UNSUPPORTED_ORDER_CLASS: TASTYTRADE does not support OCO orders for CRYPTO."}"#,
        );
        assert_eq!(e.code, Some(ErrorCode::UnsupportedOrderClass));
        let e = ApiError::from_response("replace_order", 400, br#"{"message":"GROUP_LEG_NOT_REPLACEABLE"}"#);
        assert_eq!(e.code, Some(ErrorCode::GroupLegNotReplaceable));
        let e = ApiError::from_response("x", 400, br#"{"message":"Order not found: abc"}"#);
        assert_eq!(e.code, None);
        let e = ApiError::from_response("replace_order", 400, br#"{"message":"x","code":"GROUP_LEG_NOT_REPLACEABLE"}"#);
        assert_eq!(e.code, Some(ErrorCode::GroupLegNotReplaceable));
        let e = ApiError::from_response("x", 502, b"<html>bad gateway</html>");
        assert_eq!(e.message, "<html>bad gateway</html>");
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        assert_eq!(parse_retry_after("2"), Some(std::time::Duration::from_secs(2)));
        assert_eq!(http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"), Some(std::time::Duration::ZERO));
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(http_date("Sun, 06 Nov 1994 08:49:37 UTC"), None);
    }

    #[test]
    fn what_may_have_happened() {
        assert!(Error::Transport(TransportError::Timeout("t".into())).may_have_happened());
        assert!(!Error::Transport(TransportError::NotSent("dns".into())).may_have_happened());
        assert!(Error::Api(ApiError::from_response("x", 500, b"{}")).may_have_happened());
        assert!(!Error::Api(ApiError::from_response("x", 400, b"{}")).may_have_happened());
    }
}
