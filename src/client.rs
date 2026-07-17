//! Kalshi REST client: signs requests via [`crate::auth::Signer`] and sends
//! them. The send step is abstracted behind [`RequestSender`] so the order path
//! can be unit-tested without a network (see `orders.rs`).

use crate::auth::Signer;
use anyhow::{anyhow, Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// An outgoing, already-signed HTTP request.
#[derive(Debug, Clone)]
pub struct SignedRequest {
    pub method: String,
    pub url: String,
    /// `(header_name, header_value)` pairs, including the KALSHI-ACCESS-* auth headers.
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// A raw HTTP response.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub body: String,
}

impl RawResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The transport. Real impl hits the network; tests inject a fake.
pub trait RequestSender: Send + Sync {
    fn send(&self, req: SignedRequest) -> Result<RawResponse>;
}

/// Production sender backed by a blocking-over-async reqwest call.
pub struct ReqwestSender {
    inner: reqwest::Client,
    rt: tokio::runtime::Handle,
}

impl ReqwestSender {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self { inner: reqwest::Client::new(), rt }
    }
}

impl RequestSender for ReqwestSender {
    fn send(&self, req: SignedRequest) -> Result<RawResponse> {
        let client = self.inner.clone();
        let resp = self.rt.block_on(async move {
            let mut rb = client.request(req.method.parse()?, &req.url);
            for (k, v) in &req.headers {
                rb = rb.header(k, v);
            }
            if let Some(body) = req.body {
                rb = rb.header("Content-Type", "application/json").body(body);
            }
            let r = rb.send().await.context("sending request")?;
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            anyhow::Ok(RawResponse { status, body })
        })?;
        Ok(resp)
    }
}

/// Signs requests and dispatches them through a [`RequestSender`].
pub struct KalshiClient<S: RequestSender> {
    base_url: String,
    signer: Signer,
    sender: S,
}

impl<S: RequestSender> KalshiClient<S> {
    pub fn new(base_url: impl Into<String>, signer: Signer, sender: S) -> Self {
        Self { base_url: into_base(base_url.into()), signer, sender }
    }

    /// Path portion used for signing — everything after the host, including the
    /// `/trade-api/v2` prefix. `route` is the part after `/trade-api/v2`,
    /// e.g. `/portfolio/balance`.
    fn sign_path(&self, route: &str) -> String {
        // base_url is like https://host/trade-api/v2 ; extract its path prefix.
        let prefix = self
            .base_url
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/').map(|(_, p)| format!("/{}", p)))
            .unwrap_or_else(|| "/trade-api/v2".into());
        // Kalshi signs the path WITHOUT the query string — strip anything from
        // '?' onward. Signing the query yields a 401 on authenticated routes.
        let route_path = route.split('?').next().unwrap_or(route);
        format!("{}{}", prefix.trim_end_matches('/'), route_path)
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Build a signed request for `method route`, with an optional JSON body.
    pub fn build(&self, method: &str, route: &str, body: Option<String>) -> Result<SignedRequest> {
        let path = self.sign_path(route);
        let signed = self.signer.sign(method, &path, Self::now_ms())?;
        let headers = vec![
            ("KALSHI-ACCESS-KEY".into(), signed.key_id),
            ("KALSHI-ACCESS-TIMESTAMP".into(), signed.timestamp_ms),
            ("KALSHI-ACCESS-SIGNATURE".into(), signed.signature_b64),
        ];
        Ok(SignedRequest {
            method: method.to_uppercase(),
            url: format!("{}{}", self.base_url.trim_end_matches('/'), route),
            headers,
            body,
        })
    }

    /// Sign, send, and return the parsed JSON body on 2xx (else an error with
    /// status + body for diagnosis — e.g. a 401 signals a signing-string issue).
    pub fn request_json(
        &self,
        method: &str,
        route: &str,
        body: Option<String>,
    ) -> Result<serde_json::Value> {
        let req = self.build(method, route, body)?;
        let resp = self.sender.send(req)?;
        if !resp.is_success() {
            return Err(anyhow!(
                "Kalshi API {} {} -> HTTP {}: {}",
                method,
                route,
                resp.status,
                resp.body
            ));
        }
        serde_json::from_str(&resp.body)
            .with_context(|| format!("parsing JSON from {} {}", method, route))
    }

    pub fn sender(&self) -> &S {
        &self.sender
    }
}

fn into_base(s: String) -> String {
    s.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
    use rsa::RsaPrivateKey;
    use std::sync::Mutex;

    fn test_signer() -> Signer {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = key.to_pkcs1_pem(LineEnding::LF).unwrap().to_string();
        Signer::new("kid", &pem).unwrap()
    }

    /// Records the last request and returns a canned response.
    struct FakeSender {
        last: Mutex<Option<SignedRequest>>,
        resp: RawResponse,
    }
    impl RequestSender for FakeSender {
        fn send(&self, req: SignedRequest) -> Result<RawResponse> {
            *self.last.lock().unwrap() = Some(req);
            Ok(self.resp.clone())
        }
    }

    #[test]
    fn build_signs_with_trade_api_path_and_sets_headers() {
        let fake = FakeSender {
            last: Mutex::new(None),
            resp: RawResponse { status: 200, body: "{}".into() },
        };
        let client = KalshiClient::new(
            "https://api.elections.kalshi.com/trade-api/v2",
            test_signer(),
            fake,
        );
        let req = client.build("GET", "/portfolio/balance", None).unwrap();

        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "https://api.elections.kalshi.com/trade-api/v2/portfolio/balance");
        let names: Vec<&str> = req.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"KALSHI-ACCESS-KEY"));
        assert!(names.contains(&"KALSHI-ACCESS-TIMESTAMP"));
        assert!(names.contains(&"KALSHI-ACCESS-SIGNATURE"));
    }

    #[test]
    fn sign_path_includes_trade_api_prefix() {
        let client = KalshiClient::new(
            "https://api.elections.kalshi.com/trade-api/v2",
            test_signer(),
            FakeSender { last: Mutex::new(None), resp: RawResponse { status: 200, body: "{}".into() } },
        );
        assert_eq!(
            client.sign_path("/portfolio/balance"),
            "/trade-api/v2/portfolio/balance"
        );
    }

    #[test]
    fn sign_path_excludes_query_string() {
        // Kalshi signs the path WITHOUT the query string. Signing it with the
        // query produces a 401 on authenticated endpoints.
        let client = KalshiClient::new(
            "https://api.elections.kalshi.com/trade-api/v2",
            test_signer(),
            FakeSender { last: Mutex::new(None), resp: RawResponse { status: 200, body: "{}".into() } },
        );
        assert_eq!(
            client.sign_path("/portfolio/settlements?limit=3"),
            "/trade-api/v2/portfolio/settlements"
        );
    }

    #[test]
    fn url_keeps_query_string_even_though_signature_drops_it() {
        let client = KalshiClient::new(
            "https://host/trade-api/v2",
            test_signer(),
            FakeSender { last: Mutex::new(None), resp: RawResponse { status: 200, body: "{}".into() } },
        );
        let req = client.build("GET", "/markets?limit=10&status=settled", None).unwrap();
        // The request URL must still carry the query so the server filters correctly.
        assert_eq!(req.url, "https://host/trade-api/v2/markets?limit=10&status=settled");
    }

    #[test]
    fn non_2xx_is_an_error_with_status_and_body() {
        let client = KalshiClient::new(
            "https://host/trade-api/v2",
            test_signer(),
            FakeSender {
                last: Mutex::new(None),
                resp: RawResponse { status: 401, body: "unauthorized".into() },
            },
        );
        let err = client.request_json("GET", "/portfolio/balance", None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("401"), "got: {msg}");
        assert!(msg.contains("unauthorized"), "got: {msg}");
    }
}
