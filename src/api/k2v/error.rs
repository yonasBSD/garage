use err_derive::Error;
use hyper::header::HeaderValue;
use hyper::{HeaderMap, StatusCode};

use crate::common_error::CommonError;
pub use crate::common_error::{CommonErrorDerivative, OkOrBadRequest, OkOrInternalError};
use crate::generic_server::ApiError;
use crate::helpers::*;
use crate::signature::error::Error as SignatureError;

/// Errors of this crate
#[derive(Debug, Error)]
pub enum Error {
	#[error(display = "{}", _0)]
	/// Error from common error
	Common(CommonError),

	// Category: cannot process
	/// Authorization Header Malformed
	#[error(display = "Authorization header malformed, unexpected scope: {}", _0)]
	AuthorizationHeaderMalformed(String),

	/// The object requested don't exists
	#[error(display = "Key not found")]
	NoSuchKey,

	/// Some base64 encoded data was badly encoded
	#[error(display = "Invalid base64: {}", _0)]
	InvalidBase64(#[error(source)] base64::DecodeError),

	/// The client asked for an invalid return format (invalid Accept header)
	#[error(display = "Not acceptable: {}", _0)]
	NotAcceptable(String),

	/// The request contained an invalid UTF-8 sequence in its path or in other parameters
	#[error(display = "Invalid UTF-8: {}", _0)]
	InvalidUtf8Str(#[error(source)] std::str::Utf8Error),
}

impl<T> From<T> for Error
where
	CommonError: From<T>,
{
	fn from(err: T) -> Self {
		Error::Common(CommonError::from(err))
	}
}

impl CommonErrorDerivative for Error {}

impl From<SignatureError> for Error {
	fn from(err: SignatureError) -> Self {
		match err {
			SignatureError::Common(c) => Self::Common(c),
			SignatureError::AuthorizationHeaderMalformed(c) => {
				Self::AuthorizationHeaderMalformed(c)
			}
			SignatureError::InvalidUtf8Str(i) => Self::InvalidUtf8Str(i),
		}
	}
}

impl Error {
	/// This returns a keyword for the corresponding error.
	/// Here, these keywords are not necessarily those from AWS S3,
	/// as we are building a custom API
	fn code(&self) -> &'static str {
		match self {
			Error::Common(c) => c.aws_code(),
			Error::NoSuchKey => "NoSuchKey",
			Error::NotAcceptable(_) => "NotAcceptable",
			Error::AuthorizationHeaderMalformed(_) => "AuthorizationHeaderMalformed",
			Error::InvalidBase64(_) => "InvalidBase64",
			Error::InvalidUtf8Str(_) => "InvalidUtf8String",
		}
	}
}

impl ApiError for Error {
	/// Get the HTTP status code that best represents the meaning of the error for the client
	fn http_status_code(&self) -> StatusCode {
		match self {
			Error::Common(c) => c.http_status_code(),
			Error::NoSuchKey => StatusCode::NOT_FOUND,
			Error::NotAcceptable(_) => StatusCode::NOT_ACCEPTABLE,
			Error::AuthorizationHeaderMalformed(_)
			| Error::InvalidBase64(_)
			| Error::InvalidUtf8Str(_) => StatusCode::BAD_REQUEST,
		}
	}

	fn add_http_headers(&self, header_map: &mut HeaderMap<HeaderValue>) {
		use hyper::header;
		header_map.append(header::CONTENT_TYPE, "application/json".parse().unwrap());
	}

	fn http_body(&self, garage_region: &str, path: &str) -> ErrorBody {
		let error = CustomApiErrorBody {
			code: self.code().to_string(),
			message: format!("{}", self),
			path: path.to_string(),
			region: garage_region.to_string(),
		};
		let error_str = serde_json::to_string_pretty(&error).unwrap_or_else(|_| {
			r#"
{
	"code": "InternalError",
	"message": "JSON encoding of error failed"
}
			"#
			.into()
		});
		error_body(error_str)
	}
}
