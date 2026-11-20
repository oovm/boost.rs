//! The ACME client: directory bootstrap, nonce management, and the issuance flow.

use core::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use boost_crypto::{encoding::base64url_encode, hash::sha256};

use crate::http::{Method, Request, Response, Transport, Url};

use super::{
    directory::{self, Directory},
    error::{AcmeError, AcmeResult},
    jws::{AccountKey, AccountRef, b64url, build_jws},
    types::{
        Account, Authorization, Challenge, ChallengeStatus, Order, OrderStatus, Problem, parse_account, parse_authorization, parse_challenge,
        parse_order,
    },
};

/// The replay-nonce response header.
pub const NONCE_HEADER: &str = "replay-nonce";
/// The resource location response header.
pub const LOCATION_HEADER: &str = "location";
/// The polling hint response header.
pub const RETRY_AFTER_HEADER: &str = "retry-after";
/// The only ACME problem type that makes a request worth repeating.
pub const BAD_NONCE: &str = "urn:ietf:params:acme:error:badNonce";

/// The `dns-01` TXT value for one key authorization (RFC 8555 §8.4):
/// `base64url-nopad(SHA-256(key authorization))`.
pub fn txt_digest(key_authorization: &str) -> String {
    base64url_encode(&sha256(key_authorization.as_bytes()))
}

/// The `_acme-challenge.` record name for an authorization identifier.
///
/// A leading `*.` is removed: wildcard authorizations publish the TXT record on
/// the base domain (RFC 8555 §8.4).
pub fn dns01_record_name(identifier: &str) -> String {
    let base = identifier.strip_prefix("*.").unwrap_or(identifier);
    format!("_acme-challenge.{base}")
}

/// The `http-01` path that must serve the key authorization (RFC 8555 §8.3).
pub fn http01_path(token: &str) -> String {
    format!("/.well-known/acme-challenge/{token}")
}

/// A `dns-01` challenge and the material needed to answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsChallenge {
    /// Challenge resource.
    pub challenge: Challenge,
    /// The identifier the authorization covers, with any `*.` prefix removed.
    pub identifier: String,
    /// Whether the authorization covers a wildcard identifier.
    pub wildcard: bool,
    /// `token '.' thumbprint`. This is the digest input, not the TXT value.
    pub key_authorization: String,
}

impl DnsChallenge {
    /// The `_acme-challenge.` record name for this authorization.
    pub fn record_fqdn(&self) -> String {
        dns01_record_name(&self.identifier)
    }

    /// The TXT value to publish: the digest of the key authorization.
    pub fn txt_value(&self) -> String {
        txt_digest(&self.key_authorization)
    }
}

/// How one order poll response should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderPollVerdict {
    /// The order is `valid`; the certificate can be downloaded.
    Issued,
    /// The order is still `pending`, `ready`, or `processing`.
    Pending,
    /// The order ended in the terminal `invalid` state.
    Failed,
}

/// Classify one order status for polling.
pub const fn order_poll_verdict(status: OrderStatus) -> OrderPollVerdict {
    match status {
        OrderStatus::Valid => OrderPollVerdict::Issued,
        OrderStatus::Pending | OrderStatus::Ready | OrderStatus::Processing => OrderPollVerdict::Pending,
        OrderStatus::Invalid => OrderPollVerdict::Failed,
    }
}

/// How long to wait before the next poll, honouring `Retry-After`.
///
/// `now_unix` is the current Unix second, so an HTTP date is converted into a
/// relative delay. Values that are neither delta-seconds nor an HTTP date fall
/// back to `fallback`; every result is capped by `max` so a hostile header
/// cannot stall the flow.
pub fn retry_after_delay(value: Option<&str>, now_unix: u64, fallback: Duration, max: Duration) -> Duration {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty())
    else {
        return fallback.min(max);
    };
    let seconds = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse::<u64>().ok()
    }
    else {
        parse_http_date(value).map(|epoch| epoch.saturating_sub(now_unix))
    };
    match seconds {
        Some(seconds) => Duration::from_secs(seconds).min(max),
        None => fallback.min(max),
    }
}

/// Parse an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) into Unix seconds.
fn parse_http_date(value: &str) -> Option<u64> {
    let rest = value.strip_suffix(" GMT")?;
    let mut parts = rest.split(' ');
    let weekday = parts.next()?;
    if !weekday.ends_with(',') {
        return None;
    }
    let day = parts.next()?.parse::<i64>().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts.next()?.parse::<i64>().ok()?;
    let clock = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mut fields = clock.split(':');
    let hour = fields.next()?.parse::<i64>().ok()?;
    let minute = fields.next()?.parse::<i64>().ok()?;
    let second = fields.next()?.parse::<i64>().ok()?;
    if fields.next().is_some()
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

/// Days from 1970-01-01 to a civil date, by Howard Hinnant's algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The current Unix second; a host clock before the epoch reads as zero.
fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// An RFC 8555 client over an injected family HTTP transport.
#[derive(Debug)]
pub struct AcmeClient<T: Transport> {
    transport: T,
    directory: Directory,
    key: AccountKey,
    account_url: Option<String>,
    nonce: Option<String>,
}

impl<T: Transport> AcmeClient<T> {
    /// Fetch the directory and adopt the account key before any account exists.
    pub fn bootstrap(transport: T, key: AccountKey, directory_url: &str) -> AcmeResult<Self> {
        let url = parse_url(directory_url)?;
        let response = send(&transport, &Request::new(Method::Get, url))?;
        if response.status >= 400 {
            return Err(reject(&response));
        }
        let directory = directory::parse_directory(&response.body)?;
        let nonce = response.headers.get(NONCE_HEADER).map(str::to_owned);
        Ok(Self { transport, directory, key, account_url: None, nonce })
    }

    /// The parsed directory URL table.
    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    /// The account key this client signs with.
    pub fn key(&self) -> &AccountKey {
        &self.key
    }

    /// The account URL once `new_account` succeeded.
    pub fn account_url(&self) -> Option<&str> {
        self.account_url.as_deref()
    }

    /// Bind an account URL loaded from product state so a restart can reuse the
    /// same ACME account instead of creating a second one.
    pub fn bind_account(&mut self, url: impl Into<String>) {
        self.account_url = Some(url.into());
    }

    /// Create (or idempotently retrieve) the account, binding the client to its URL.
    pub fn new_account(&mut self, contact: &[String], agree_terms: bool) -> AcmeResult<Account> {
        let mut payload = serde_json::Map::new();
        payload.insert("termsOfServiceAgreed".to_owned(), serde_json::Value::Bool(agree_terms));
        if !contact.is_empty() {
            payload.insert("contact".to_owned(), serde_json::Value::Array(contact.iter().cloned().map(serde_json::Value::String).collect()));
        }
        let response = self.post(&self.directory.new_account.clone(), &serde_json::Value::Object(payload).to_string(), &AccountRef::Jwk)?;
        let url = location(&response)?;
        let account = parse_account(&url, &response.body)?;
        self.account_url = Some(url);
        Ok(account)
    }

    /// Query the bound account with a POST-as-GET.
    pub fn account(&mut self) -> AcmeResult<Account> {
        let url = self.kid()?;
        let response = self.post_as_get(&url)?;
        parse_account(&url, &response.body)
    }

    /// Create an order for the given DNS names (duplicate SANs included as-is).
    pub fn new_order(&mut self, dns_names: &[String]) -> AcmeResult<Order> {
        let identifiers: Vec<serde_json::Value> = dns_names
            .iter()
            .map(|name| {
                let mut identifier = serde_json::Map::new();
                identifier.insert("type".to_owned(), serde_json::Value::String("dns".to_owned()));
                identifier.insert("value".to_owned(), serde_json::Value::String(name.clone()));
                serde_json::Value::Object(identifier)
            })
            .collect();
        let mut payload = serde_json::Map::new();
        payload.insert("identifiers".to_owned(), serde_json::Value::Array(identifiers));
        let kid = self.kid()?;
        let response = self.post(&self.directory.new_order.clone(), &serde_json::Value::Object(payload).to_string(), &AccountRef::Kid(kid))?;
        let url = location(&response)?;
        parse_order(&url, &response.body)
    }

    /// Fetch one order resource.
    pub fn order(&mut self, url: &str) -> AcmeResult<Order> {
        let response = self.post_as_get(url)?;
        parse_order(url, &response.body)
    }

    /// Poll an order until it reaches a terminal state or the bounded deadline.
    ///
    /// Only `valid` counts as success; `invalid` fails immediately with the
    /// problem detail. Waiting honours `Retry-After` but never exceeds `delay`.
    /// Reaching the deadline is an explicit timeout, never a silent success.
    pub fn poll_order(&mut self, url: &str, attempts: usize, delay: Duration) -> AcmeResult<Order> {
        let attempts = attempts.max(1);
        let mut last_status = None;
        for attempt in 0..attempts {
            let response = self.post_as_get(url)?;
            let order = parse_order(url, &response.body)?;
            match order_poll_verdict(order.status) {
                OrderPollVerdict::Issued => return Ok(order),
                OrderPollVerdict::Failed => {
                    let detail = order
                        .error
                        .as_ref()
                        .map(|problem| problem.detail.as_str())
                        .filter(|detail| !detail.is_empty())
                        .unwrap_or("响应没有给出 problem 详情");
                    return Err(AcmeError::protocol(format!("订单 {} 进入终态 `{}`：{detail}", order.url, order.status.as_str())));
                }
                OrderPollVerdict::Pending => last_status = Some(order.status),
            }
            if attempt + 1 < attempts {
                let wait = retry_after_delay(response.headers.get(RETRY_AFTER_HEADER), unix_now(), delay, delay);
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
        }
        Err(AcmeError::network(format!("轮询订单 {url} 超时，最后状态为 `{}`", last_status.map_or("unknown", OrderStatus::as_str))))
    }

    /// Fetch one authorization resource.
    pub fn authorization(&mut self, url: &str) -> AcmeResult<Authorization> {
        let response = self.post_as_get(url)?;
        parse_authorization(url, &response.body)
    }

    /// Fetch an authorization and select its `dns-01` challenge and key authorization.
    pub fn dns_challenge(&mut self, authorization_url: &str) -> AcmeResult<DnsChallenge> {
        let authorization = self.authorization(authorization_url)?;
        let challenge = authorization.dns01().ok_or_else(|| AcmeError::protocol("该 authorization 没有提供 dns-01 挑战"))?.clone();
        let key_authorization = self.key.key_authorization(&challenge.token);
        Ok(DnsChallenge { challenge, identifier: authorization.identifier.value, wildcard: authorization.wildcard, key_authorization })
    }

    /// Tell the server that the challenge response is ready and return its new status.
    pub fn respond_challenge(&mut self, url: &str) -> AcmeResult<ChallengeStatus> {
        let response = self.post_json(url, "{}")?;
        Ok(parse_challenge(url, &response.body)?.status)
    }

    /// Submit the finalized CSR (DER) and return the updated order.
    pub fn finalize(&mut self, order: &Order, csr_der: &[u8]) -> AcmeResult<Order> {
        let mut payload = serde_json::Map::new();
        payload.insert("csr".to_owned(), serde_json::Value::String(b64url(csr_der)));
        let response = self.post_json(&order.finalize, &serde_json::Value::Object(payload).to_string())?;
        parse_order(&order.url, &response.body)
    }

    /// Download the certificate chain in PEM encoding.
    pub fn certificate(&mut self, url: &str) -> AcmeResult<String> {
        let response = self.post_as_get(url)?;
        String::from_utf8(response.body).map_err(|_| AcmeError::protocol("the certificate chain is not UTF-8 PEM"))
    }

    /// The bound account URL, required by every account-bound request.
    fn kid(&self) -> AcmeResult<String> {
        self.account_url.clone().ok_or_else(|| AcmeError::invalid_argument("no ACME account is bound to this client"))
    }

    /// Take the cached nonce or fetch a fresh one from the directory's `newNonce`.
    fn refresh_nonce(&mut self) -> AcmeResult<String> {
        if let Some(nonce) = self.nonce.take() {
            return Ok(nonce);
        }
        let url = parse_url(&self.directory.new_nonce)?;
        let response = send(&self.transport, &Request::new(Method::Head, url))?;
        response.headers.get(NONCE_HEADER).map(str::to_owned).ok_or_else(|| AcmeError::protocol("the server did not issue a replay nonce"))
    }

    /// Send one JWS; only a `badNonce` problem is retried once with a fresh nonce.
    fn post(&mut self, url: &str, payload_json: &str, account: &AccountRef) -> AcmeResult<Response> {
        let mut retried = false;
        loop {
            let nonce = self.refresh_nonce()?;
            let document = build_jws(&self.key, account, url, &nonce, payload_json)?;
            let response = send(
                &self.transport,
                &Request::new(Method::Post, parse_url(url)?)
                    .with_header("Content-Type", "application/jose+json")
                    .with_body(document.into_bytes()),
            )?;
            if let Some(nonce) = response.headers.get(NONCE_HEADER) {
                self.nonce = Some(nonce.to_owned());
            }
            if response.status >= 400 {
                let problem = Problem::parse(response.status, &response.body);
                if problem.problem_type.as_deref() == Some(BAD_NONCE) && !retried {
                    retried = true;
                    continue;
                }
                return Err(reject(&response));
            }
            return Ok(response);
        }
    }

    /// A POST-as-GET: an empty payload authenticated with the account URL.
    fn post_as_get(&mut self, url: &str) -> AcmeResult<Response> {
        let kid = self.kid()?;
        self.post(url, "", &AccountRef::Kid(kid))
    }

    /// A POST with a JSON payload authenticated with the account URL.
    fn post_json(&mut self, url: &str, payload_json: &str) -> AcmeResult<Response> {
        let kid = self.kid()?;
        self.post(url, payload_json, &AccountRef::Kid(kid))
    }
}

fn send<T: Transport>(transport: &T, request: &Request) -> AcmeResult<Response> {
    transport.send(request).map_err(|error| AcmeError::network(error.to_string()))
}

fn parse_url(url: &str) -> AcmeResult<Url> {
    Url::parse(url).map_err(|error| AcmeError::protocol(format!("invalid URL `{url}`: {error}")))
}

fn location(response: &Response) -> AcmeResult<String> {
    response.headers.get(LOCATION_HEADER).map(str::to_owned).ok_or_else(|| AcmeError::protocol("the server omitted the Location header"))
}

fn reject(response: &Response) -> AcmeError {
    let problem = Problem::parse(response.status, &response.body);
    let detail = if problem.detail.is_empty() { format!("status {}", response.status) } else { problem.detail };
    let message = format!("the ACME server rejected the request to {}: {detail}", response.url);
    if response.status == 403 { AcmeError::denied(message) } else { AcmeError::protocol(message) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_prefers_delta_seconds_and_caps_it() {
        let fallback = Duration::from_secs(5);
        let max = Duration::from_secs(30);
        assert_eq!(retry_after_delay(Some("7"), 0, fallback, max), Duration::from_secs(7));
        assert_eq!(retry_after_delay(Some("300"), 0, fallback, max), max);
        assert_eq!(retry_after_delay(None, 0, fallback, max), fallback);
        assert_eq!(retry_after_delay(Some("nonsense"), 0, fallback, max), fallback);
    }

    #[test]
    fn retry_after_converts_http_dates_to_a_relative_delay() {
        let now = 784_111_777;
        assert_eq!(
            retry_after_delay(Some("Sun, 06 Nov 1994 08:49:47 GMT"), now, Duration::ZERO, Duration::from_secs(60)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn order_status_maps_to_a_poll_verdict() {
        assert_eq!(order_poll_verdict(OrderStatus::Valid), OrderPollVerdict::Issued);
        assert_eq!(order_poll_verdict(OrderStatus::Ready), OrderPollVerdict::Pending);
        assert_eq!(order_poll_verdict(OrderStatus::Invalid), OrderPollVerdict::Failed);
    }

    #[test]
    fn dns_and_http_challenge_material_follows_rfc_8555() {
        assert_eq!(dns01_record_name("*.example.test"), "_acme-challenge.example.test");
        assert_eq!(dns01_record_name("example.test"), "_acme-challenge.example.test");
        assert_eq!(http01_path("token"), "/.well-known/acme-challenge/token");
        assert_eq!(txt_digest("token.thumbprint").len(), 43);
    }
}
