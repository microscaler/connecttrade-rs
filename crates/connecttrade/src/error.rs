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
    /// The body as received, for logs.
    pub body: String,
}

impl ApiError {
    pub(crate) fn from_response(operation: &'static str, status: u16, body: &[u8]) -> ApiError {
        let raw = String::from_utf8_lossy(body).into_owned();
        let parsed: Option<ErrorResponse> = serde_json::from_slice(body).ok();
        let message = parsed.as_ref().and_then(|e| e.message.clone()).unwrap_or_else(|| raw.clone());
        let code = parsed
            .as_ref()
            .and_then(|e| e.code.clone())
            .or_else(|| leading_code(&message))
            .map(|c| ErrorCode::parse(&c));
        ApiError { operation, status, code, message, body: raw }
    }

    /// `409` on a create, replace or cancel: the duplicate `cl_order_id` case or a `BROKEN` connection. The two are
    /// told apart by the message; a broken connection needs the user to log in at the broker again
    /// (`repair_connection`), never a retry.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        self.status == 409
    }

    #[must_use]
    pub fn is_broken_connection(&self) -> bool {
        self.status == 409 && self.message.to_ascii_lowercase().contains("broken")
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
pub enum Error {
    #[error("{operation}: needs {missing}")]
    Credentials { operation: &'static str, missing: &'static str },
    /// The request body could not be encoded; nothing was sent.
    #[error("{operation}: body did not encode: {reason}")]
    Encode { operation: &'static str, reason: String },
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("{} {}: {}", .0.operation, .0.status, .0.message)]
    Api(ApiError),
    #[error("{operation}: {status} answer did not decode: {reason}")]
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
    fn what_may_have_happened() {
        assert!(Error::Transport(TransportError::Timeout("t".into())).may_have_happened());
        assert!(!Error::Transport(TransportError::NotSent("dns".into())).may_have_happened());
        assert!(Error::Api(ApiError::from_response("x", 500, b"{}")).may_have_happened());
        assert!(!Error::Api(ApiError::from_response("x", 400, b"{}")).may_have_happened());
    }
}
