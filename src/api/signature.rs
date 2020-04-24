use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use hmac::{Hmac, Mac};
use hyper::{Body, Method, Request};
use sha2::{Digest, Sha256};

use garage_table::*;
use garage_util::error::Error;

use garage_core::garage::Garage;
use garage_core::key_table::*;

const SHORT_DATE: &str = "%Y%m%d";
const LONG_DATETIME: &str = "%Y%m%dT%H%M%SZ";

type HmacSha256 = Hmac<Sha256>;

pub async fn check_signature(garage: &Garage, request: &Request<Body>) -> Result<Key, Error> {
	let mut headers = HashMap::new();
	for (key, val) in request.headers() {
		headers.insert(key.to_string(), val.to_str()?.to_string());
	}
	if let Some(query) = request.uri().query() {
		let query_pairs = url::form_urlencoded::parse(query.as_bytes());
		for (key, val) in query_pairs {
			headers.insert(key.to_lowercase(), val.to_string());
		}
	}

	let authorization = if let Some(authorization) = headers.get("authorization") {
		parse_authorization(authorization, &headers)?
	} else {
		parse_query_authorization(&headers)?
	};

	let date = headers
		.get("x-amz-date")
		.ok_or(Error::BadRequest("Missing X-Amz-Date field".into()))?;
	let date: NaiveDateTime = NaiveDateTime::parse_from_str(date, LONG_DATETIME)
		.map_err(|e| Error::BadRequest(format!("Invalid date: {}", e)))?
		.into();
	let date: DateTime<Utc> = DateTime::from_utc(date, Utc);

	if Utc::now() - date > Duration::hours(24) {
		return Err(Error::BadRequest(format!("Date is too old")));
	}

	let scope = format!(
		"{}/{}/s3/aws4_request",
		date.format(SHORT_DATE),
		garage.config.s3_api.s3_region
	);
	if authorization.scope != scope {
		return Err(Error::BadRequest(format!(
			"Invalid scope in authorization field, expected: {}",
			scope
		)));
	}

	let key = garage
		.key_table
		.get(&EmptyKey, &authorization.key_id)
		.await?
		.filter(|k| !k.deleted)
		.ok_or(Error::Forbidden(format!(
			"No such key: {}",
			authorization.key_id
		)))?;

	let canonical_request = canonical_request(
		request.method(),
		&request.uri().path().to_string(),
		&canonical_query_string(&request.uri()),
		&headers,
		&authorization.signed_headers,
		&authorization.content_sha256,
	);
	let string_to_sign = string_to_sign(&date, &scope, &canonical_request);

	let mut hmac = signing_hmac(
		&date,
		&key.secret_key,
		&garage.config.s3_api.s3_region,
		"s3",
	)
	.map_err(|e| Error::Message(format!("Unable to build signing HMAC: {}", e)))?;
	hmac.input(string_to_sign.as_bytes());
	let signature = hex::encode(hmac.result().code());

	if authorization.signature != signature {
		return Err(Error::Forbidden(format!("Invalid signature")));
	}

	Ok(key)
}

struct Authorization {
	key_id: String,
	scope: String,
	signed_headers: String,
	signature: String,
	content_sha256: String,
}

fn parse_authorization(
	authorization: &str,
	headers: &HashMap<String, String>,
) -> Result<Authorization, Error> {
	let first_space = authorization
		.find(' ')
		.ok_or(Error::BadRequest("Authorization field too short".into()))?;
	let (auth_kind, rest) = authorization.split_at(first_space);

	if auth_kind != "AWS4-HMAC-SHA256" {
		return Err(Error::BadRequest("Unsupported authorization method".into()));
	}

	let mut auth_params = HashMap::new();
	for auth_part in rest.split(',') {
		let auth_part = auth_part.trim();
		let eq = auth_part.find('=').ok_or(Error::BadRequest(format!(
			"Missing =value in authorization field {}",
			auth_part
		)))?;
		let (key, value) = auth_part.split_at(eq);
		auth_params.insert(key.to_string(), value.trim_start_matches('=').to_string());
	}

	let cred = auth_params
		.get("Credential")
		.ok_or(Error::BadRequest(format!(
			"Could not find Credential in Authorization field"
		)))?;
	let (key_id, scope) = parse_credential(cred)?;

	let content_sha256 = headers
		.get("x-amz-content-sha256")
		.ok_or(Error::BadRequest(
			"Missing X-Amz-Content-Sha256 field".into(),
		))?;

	let auth = Authorization {
		key_id,
		scope,
		signed_headers: auth_params
			.get("SignedHeaders")
			.ok_or(Error::BadRequest(format!(
				"Could not find SignedHeaders in Authorization field"
			)))?
			.to_string(),
		signature: auth_params
			.get("Signature")
			.ok_or(Error::BadRequest(format!(
				"Could not find Signature in Authorization field"
			)))?
			.to_string(),
		content_sha256: content_sha256.to_string(),
	};
	Ok(auth)
}

fn parse_query_authorization(headers: &HashMap<String, String>) -> Result<Authorization, Error> {
	let algo = headers
		.get("x-amz-algorithm")
		.ok_or(Error::BadRequest(format!(
			"X-Amz-Algorithm not found in query parameters"
		)))?;
	if algo != "AWS4-HMAC-SHA256" {
		return Err(Error::BadRequest(format!(
			"Unsupported authorization method"
		)));
	}

	let cred = headers
		.get("x-amz-credential")
		.ok_or(Error::BadRequest(format!(
			"X-Amz-Credential not found in query parameters"
		)))?;
	let (key_id, scope) = parse_credential(cred)?;
	let signed_headers = headers
		.get("x-amz-signedheaders")
		.ok_or(Error::BadRequest(format!(
			"X-Amz-SignedHeaders not found in query parameters"
		)))?;
	let signature = headers
		.get("x-amz-signature")
		.ok_or(Error::BadRequest(format!(
			"X-Amz-Signature not found in query parameters"
		)))?;

	Ok(Authorization {
		key_id,
		scope,
		signed_headers: signed_headers.to_string(),
		signature: signature.to_string(),
		content_sha256: "UNSIGNED-PAYLOAD".to_string(),
	})
}

fn parse_credential(cred: &str) -> Result<(String, String), Error> {
	let first_slash = cred.find('/').ok_or(Error::BadRequest(format!(
		"Credentials does not contain / in authorization field"
	)))?;
	let (key_id, scope) = cred.split_at(first_slash);
	Ok((
		key_id.to_string(),
		scope.trim_start_matches('/').to_string(),
	))
}

fn string_to_sign(datetime: &DateTime<Utc>, scope_string: &str, canonical_req: &str) -> String {
	let mut hasher = Sha256::default();
	hasher.input(canonical_req.as_bytes());
	[
		"AWS4-HMAC-SHA256",
		&datetime.format(LONG_DATETIME).to_string(),
		scope_string,
		&hex::encode(hasher.result().as_slice()),
	]
	.join("\n")
}

fn signing_hmac(
	datetime: &DateTime<Utc>,
	secret_key: &str,
	region: &str,
	service: &str,
) -> Result<HmacSha256, crypto_mac::InvalidKeyLength> {
	let secret = String::from("AWS4") + secret_key;
	let mut date_hmac = HmacSha256::new_varkey(secret.as_bytes())?;
	date_hmac.input(datetime.format(SHORT_DATE).to_string().as_bytes());
	let mut region_hmac = HmacSha256::new_varkey(&date_hmac.result().code())?;
	region_hmac.input(region.as_bytes());
	let mut service_hmac = HmacSha256::new_varkey(&region_hmac.result().code())?;
	service_hmac.input(service.as_bytes());
	let mut signing_hmac = HmacSha256::new_varkey(&service_hmac.result().code())?;
	signing_hmac.input(b"aws4_request");
	let hmac = HmacSha256::new_varkey(&signing_hmac.result().code())?;
	Ok(hmac)
}

fn canonical_request(
	method: &Method,
	url_path: &str,
	canonical_query_string: &str,
	headers: &HashMap<String, String>,
	signed_headers: &str,
	content_sha256: &str,
) -> String {
	[
		method.as_str(),
		url_path,
		canonical_query_string,
		&canonical_header_string(&headers, signed_headers),
		"",
		signed_headers,
		content_sha256,
	]
	.join("\n")
}

fn canonical_header_string(headers: &HashMap<String, String>, signed_headers: &str) -> String {
	let signed_headers_vec = signed_headers.split(';').collect::<Vec<_>>();
	let mut items = headers
		.iter()
		.filter(|(key, _)| signed_headers_vec.contains(&key.as_str()))
		.map(|(key, value)| key.to_lowercase() + ":" + value.trim())
		.collect::<Vec<_>>();
	items.sort();
	items.join("\n")
}

fn canonical_query_string(uri: &hyper::Uri) -> String {
	if let Some(query) = uri.query() {
		let query_pairs = url::form_urlencoded::parse(query.as_bytes());
		let mut items = query_pairs
			.filter(|(key, _)| key != "X-Amz-Signature")
			.map(|(key, value)| uri_encode(&key, true) + "=" + &uri_encode(&value, true))
			.collect::<Vec<_>>();
		items.sort();
		items.join("&")
	} else {
		"".to_string()
	}
}

fn uri_encode(string: &str, encode_slash: bool) -> String {
	let mut result = String::with_capacity(string.len() * 2);
	for c in string.chars() {
		match c {
			'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '~' | '.' => result.push(c),
			'/' if encode_slash => result.push_str("%2F"),
			'/' if !encode_slash => result.push('/'),
			_ => {
				result.push('%');
				result.push_str(
					&format!("{}", c)
						.bytes()
						.map(|b| format!("{:02X}", b))
						.collect::<String>(),
				);
			}
		}
	}
	result
}