//! Best-effort observation of this application's public egress at import time.
//! This is separate from an official account login IP and carries no credentials.
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportIp {
    pub ip: String,
    pub observed_at: i64,
}

pub struct Observer {
    client: reqwest::Client,
    endpoint: String,
}

impl Observer {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(4))
                .build()
                .expect("import IP HTTP client"),
            endpoint: "https://api64.ipify.org?format=json".into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(endpoint: String) -> Self {
        Self {
            endpoint,
            ..Self::new()
        }
    }

    pub async fn observe(&self) -> Option<ImportIp> {
        // A dedicated client/request prevents any login headers or query state
        // from being forwarded to the IP service.
        let response = self.client.get(&self.endpoint).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if bytes.len().saturating_add(chunk.len()) > 1024 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let ip = value.get("ip")?.as_str()?.trim().parse::<IpAddr>().ok()?;
        if !public_address(ip) {
            return None;
        }
        Some(ImportIp {
            ip: ip.to_string(),
            observed_at: crate::reqlog::now_ms(),
        })
    }
}

fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && a != 0
                && a < 240
                && !(a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_address(IpAddr::V4(ip));
            }
            // Global unicast. Excludes local, link-local, multicast and unspecified.
            ip.segments()[0] & 0xe000 == 0x2000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::Request, http::Response, routing::any, Router};

    #[tokio::test]
    async fn a_slow_ip_service_is_optional_and_times_out() {
        let app = Router::new().fallback(any(|| async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            "{\"ip\":\"203.0.113.42\"}"
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let observer = Observer::at(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = tokio::time::timeout(Duration::from_secs(5), observer.observe()).await;
        assert!(result.expect("lookup exceeded its time budget").is_none());
        server.abort();
    }

    #[tokio::test]
    async fn observations_are_bounded_validated_and_anonymous() {
        for (status, body, expected) in [
            (
                200,
                r#"{"ip":"203.0.113.42"}"#.to_string(),
                Some("203.0.113.42"),
            ),
            (
                200,
                r#"{"ip":"2001:db8::42"}"#.to_string(),
                Some("2001:db8::42"),
            ),
            (200, r#"{"ip":"127.0.0.1"}"#.to_string(), None),
            (200, r#"{"ip":"10.1.2.3"}"#.to_string(), None),
            (200, r#"{"ip":"::ffff:127.0.0.1"}"#.to_string(), None),
            (200, r#"{"ip":"not-an-ip"}"#.to_string(), None),
            (200, "x".repeat(1025), None),
            (500, r#"{"ip":"203.0.113.42"}"#.to_string(), None),
            (302, String::new(), None),
        ] {
            let app = Router::new().fallback(any(move |request: Request| {
                let body = body.clone();
                async move {
                    assert_eq!(request.uri().query(), None);
                    assert!(!request.headers().contains_key("authorization"));
                    assert!(!request.headers().contains_key("cookie"));
                    Response::builder()
                        .status(status)
                        .header("location", "/unexpected")
                        .body(Body::from(body))
                        .unwrap()
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let observer = Observer::at(format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let observed = observer.observe().await;
            assert_eq!(observed.as_ref().map(|item| item.ip.as_str()), expected);
            if let Some(observed) = observed {
                assert!(observed.observed_at > 0);
            }
            server.abort();
        }
    }
}
