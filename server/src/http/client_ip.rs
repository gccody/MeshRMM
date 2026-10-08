//! The address of the client behind a request, for the audit log and rate
//! limits.
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, request::Parts},
};
use ipnet::IpNet;

use super::AppState;

/// The client's address: the TCP peer, or behind trusted reverse proxies the
/// nearest untrusted address in `X-Forwarded-For`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or(IpAddr::from([0, 0, 0, 0]), |ConnectInfo(address)| {
                address.ip()
            });
        Ok(Self(resolve(
            peer,
            &parts.headers,
            state.config.trusted_proxies(),
        )))
    }
}

/// Walks `X-Forwarded-For` from the nearest hop outward while each hop is a
/// trusted proxy, so a client cannot choose its address by sending the header
/// itself: only addresses appended by trusted proxies are believed.
pub fn resolve(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |address: &IpAddr| trusted.iter().any(|network| network.contains(address));
    if !is_trusted(&peer) {
        return peer;
    }
    let hops = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    let mut client = peer;
    for hop in hops.into_iter().rev() {
        let Ok(address) = hop.parse::<IpAddr>() else {
            // A malformed hop ends what can be believed.
            break;
        };
        client = address;
        if !is_trusted(&address) {
            break;
        }
    }
    client
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn untrusted_peers_are_the_client_whatever_they_claim() {
        let trusted = ["127.0.0.1/32".parse().unwrap()];
        assert_eq!(
            resolve(ip("198.51.100.7"), &headers(&["203.0.113.1"]), &trusted),
            ip("198.51.100.7")
        );
        assert_eq!(
            resolve(ip("198.51.100.7"), &headers(&["203.0.113.1"]), &[]),
            ip("198.51.100.7")
        );
    }

    #[test]
    fn a_trusted_proxy_reports_the_client() {
        let trusted = ["127.0.0.1/32".parse().unwrap()];
        assert_eq!(
            resolve(ip("127.0.0.1"), &headers(&["203.0.113.1"]), &trusted),
            ip("203.0.113.1")
        );
    }

    #[test]
    fn spoofed_hops_before_the_trusted_chain_are_ignored() {
        let trusted = [
            "127.0.0.1/32".parse().unwrap(),
            "10.0.0.0/8".parse().unwrap(),
        ];
        // The client sent "1.2.3.4" itself; the edge proxy appended the real
        // address, and an internal proxy appended the edge's.
        let header = headers(&["1.2.3.4, 203.0.113.9", "10.0.0.5"]);
        assert_eq!(
            resolve(ip("127.0.0.1"), &header, &trusted),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn a_malformed_hop_stops_the_walk() {
        let trusted = ["127.0.0.1/32".parse().unwrap()];
        assert_eq!(
            resolve(
                ip("127.0.0.1"),
                &headers(&["203.0.113.1, garbage"]),
                &trusted
            ),
            ip("127.0.0.1")
        );
    }

    #[test]
    fn without_the_header_the_proxy_is_the_client() {
        let trusted = ["127.0.0.1/32".parse().unwrap()];
        assert_eq!(
            resolve(ip("127.0.0.1"), &HeaderMap::new(), &trusted),
            ip("127.0.0.1")
        );
    }
}
