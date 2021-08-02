use crate::{
	auth::Auth,
	error::{Error, Result, VaultErrors},
	secret::Secret,
};

use isahc::{
	config::{CaCertificate, Configurable},
	http::{Request, StatusCode},
	AsyncReadResponseExt, HttpClient,
};
use serde_json::{Map, Value};
use std::{collections::HashMap, fs::File, io::Read, time::Duration};

/// Vault client that cache its auth tokens
pub struct VaultClient {
	pub url: String,
	jwt: String,
	client: HttpClient,
	/// map a role to an authentification token
	pub auth: HashMap<String, Auth>,
}

impl VaultClient {
	/// Create a new vault client given an url, a token path and a ca certificate path
	pub async fn new(url: &str, token: &str, cacert: &str) -> Result<Self> {
		let mut jwt = String::new();
		File::open(token)
			.map_err(|e| Error::TokenError { source: e })?
			.read_to_string(&mut jwt)
			.map_err(|e| Error::TokenError { source: e })?;
		let client = HttpClient::builder()
			.ssl_ca_certificate(CaCertificate::file(cacert))
			.default_header("Content-Type", "application/json")
			.build()?;
		Ok(VaultClient {
			url: url.to_owned(),
			jwt,
			client,
			auth: HashMap::new(),
		})
	}

	pub fn is_logged(&self, role: &str) -> bool {
		self.auth
			.get(role)
			.filter(|v| v.is_valid() && !v.to_renew())
			.is_some()
	}

	/// Log in to the vault client and return Auth.
	pub async fn login(&mut self, role: &str) -> Result<&Auth> {
		// login if we are not already logged in or if it's time to renew token

		if !self.is_logged(role) {
			let url = format!("{}/auth/kubernetes/login", &self.url);
			let body = format!(r#"{{"role": "{}", "jwt": "{}"}}"#, role, &self.jwt);
			let mut res = self
				.client
				.post_async(url, body)
				.await
				.map_err(|e| Error::ClientError { source: e })?;
			let status = res.status();
			if status == StatusCode::OK {
				// parse vault response and cache important information
				let auth_value: Value = res
					.json()
					.await
					.map_err(|e| Error::ParseError { source: e })?;
				let lease_duration = auth_value["auth"]["lease_duration"]
					.as_u64()
					.unwrap_or(0u64);
				let renewable = auth_value["auth"]["renewable"].as_bool().unwrap_or(false);
				let auth = Auth::new(
					auth_value["auth"]["client_token"].as_str().unwrap_or(""),
					if renewable {
						Some(Duration::from_secs(lease_duration))
					} else {
						None
					},
				);
				// insert and forget old value if any
				let _ = self.auth.insert(role.to_owned(), auth);
			} else {
				// parse vault error
				let errors: VaultErrors = res
					.json()
					.await
					.map_err(|e| Error::ParseError { source: e })?;
				return Err(Error::VaultError(status, errors.errors.join("\n")));
			}
		}

		Ok(self.auth.get(role).unwrap())
	}

	/// Get a secret from vault server and reschedule a renew with role if necessary
	pub async fn get_secret(
		&self,
		role: &str,
		method: &str,
		path: &str,
		kwargs: Option<&Vec<(&str, &str)>>,
	) -> Result<Secret> {
		if let Some(auth) = self.auth.get(role) {
			let uri = format!("{}/{}", &self.url, path);
			// transform the kwargs into a json object
			let body = kwargs
				.map(|kwargs| {
					kwargs.iter().fold(Map::new(), |mut m, (k, v)| {
						m.insert((*k).to_owned(), Value::String((*v).to_owned()));
						m
					})
				})
				.map(|o| Value::Object(o))
				.unwrap_or(Value::Null);
			// build the request
			let request = Request::builder()
				.uri(uri)
				.method(method)
				.header("X-Vault-Token", auth.client_token.as_str())
				.body(body.to_string())
				.map_err(|e| Error::HttpError { source: e })?;
			// async send the request
			let mut res = self
				.client
				.send_async(request)
				.await
				.map_err(|e| Error::ClientError { source: e })?;
			// handle the response
			let status = res.status();
			return if status == StatusCode::OK {
				// parse vault response
				let mut secret_value: Value = res
					.json()
					.await
					.map_err(|e| Error::ParseError { source: e })?;

				let duration = secret_value
					.get("lease_duration")
					.map(|o| o.as_u64().unwrap_or(0u64))
					.filter(|o| *o != 0u64)
					.map(|o| Duration::from_secs(o * 2 / 3));
				// return the parsed secret (only the data part)
				Ok(Secret::new(secret_value["data"].take(), duration))
			} else {
				// parse vault error
				let errors: VaultErrors = res
					.json()
					.await
					.map_err(|e| Error::ParseError { source: e })?;
				Err(Error::VaultError(status, errors.errors.join("\n")))
			};
		} else {
			Err(Error::NotLogged)
		}
	}
}
