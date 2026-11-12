use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use spreadsheet_to_json::error::GenericError;

use crate::sheet::mapping::MappingError;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    /// No acceptable identity (service mode without the forwarded reference or token).
    Unauthorized(String),
    NotFound(String),
    Forbidden(String),
    Conflict(String),
    /// A rule enforced by the schema (plan limits, immutable versions ...) rejected the write.
    Rejected(String),
    Unprocessable(String),
    /// The mapping document doesn't fit the request or the worksheet.
    InvalidMapping(Vec<MappingError>),
    /// Rate limited; retry after this many seconds.
    TooManyRequests(u64),
    /// A sign-in provider or source URL failed.
    BadGateway(String),
    Internal(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, &str) {
        match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m),
            Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m),
            Self::Conflict(m) => (StatusCode::CONFLICT, "conflict", m),
            Self::Rejected(m) => (StatusCode::UNPROCESSABLE_ENTITY, "rejected", m),
            Self::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, "unprocessable", m),
            Self::InvalidMapping(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_mapping", "the mapping has errors"),
            Self::TooManyRequests(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many requests; slow down"),
            Self::BadGateway(m) => (StatusCode::BAD_GATEWAY, "bad_gateway", m),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.parts().2)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("{message}");
        }
        let mut body = json!({ "error": code, "message": message });
        if let Self::InvalidMapping(errors) = &self {
            body["errors"] = json!(errors);
        }
        let mut response = (status, Json(body)).into_response();
        if let Self::TooManyRequests(seconds) = self {
            response.headers_mut().insert(axum::http::header::RETRY_AFTER, seconds.into());
        }
        response
    }
}

impl From<surrealdb::Error> for ApiError {
    fn from(err: surrealdb::Error) -> Self {
        let text = err.to_string();
        // Schema events THROW plain-English messages ("Hourly import limit reached ..."):
        // surface those as rule rejections rather than opaque 500s.
        // (possibly wrapped, e.g. "Error while processing event ...: An error occurred: ...")
        match text.split_once("An error occurred: ") {
            Some((_, thrown)) => ApiError::Rejected(thrown.to_string()),
            None => ApiError::Internal(format!("database error: {text}")),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        ApiError::Internal(format!("file error: {err}"))
    }
}

impl From<GenericError> for ApiError {
    fn from(err: GenericError) -> Self {
        let message = match err.0 {
            "file_unavailable" | "file_not_found" => "the source file is no longer available; upload it again",
            "unsupported_format" => "unsupported file format; use xlsx, xlsm, xls, xlsb, ods, csv, tsv or semicolon-separated values",
            "workbook_with_no_sheets" => "the workbook has no readable worksheets",
            "cannot_open_workbook" => "could not open the workbook; the file may be corrupt or not a spreadsheet",
            "unreadable_csv_file" | "unreadable_tsv_file" => "could not read the delimited text file",
            "xlsx_error" => "the Excel file appears to be corrupt or invalid",
            "ods_error" => "the OpenDocument file appears to be corrupt or invalid",
            "row_limit_exceeded" => "the worksheet has more rows than your plan allows per import",
            other => return ApiError::Unprocessable(format!("could not read the spreadsheet ({other})")),
        };
        ApiError::Unprocessable(message.to_string())
    }
}
