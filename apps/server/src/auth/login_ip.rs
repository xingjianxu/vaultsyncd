//! 本机 TLS 代理覆盖转发头；远端直连始终使用 TCP 来源。
use axum::http::HeaderMap;
use std::net::{IpAddr, SocketAddr};

pub(super) fn source(peer: Option<SocketAddr>, headers: &HeaderMap) -> Option<IpAddr> {
    let peer = peer?.ip();
    if peer.is_loopback() {
        let mut values = headers.get_all("x-forwarded-for").iter();
        if let Some(value) = values.next()
            && values.next().is_none()
            && let Ok(value) = value.to_str()
            && let Ok(ip) = value.trim().parse::<IpAddr>()
        {
            return Some(ip);
        }
    }
    Some(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_source_and_local_proxy_have_explicit_trust_boundary() {
        for text in ["198.51.100.7:1234", "[2001:db8::7]:1234"] {
            let peer: SocketAddr = text.parse().unwrap();
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
            headers.insert("x-real-ip", "192.0.2.2".parse().unwrap());
            assert_eq!(source(Some(peer), &headers), Some(peer.ip()));
        }
        for peer in ["127.0.0.1:1234", "[::1]:1234"] {
            let peer: SocketAddr = peer.parse().unwrap();
            for text in ["192.0.2.1", "2001:db8::1"] {
                let mut headers = HeaderMap::new();
                headers.insert("x-forwarded-for", text.parse().unwrap());
                assert_eq!(source(Some(peer), &headers), Some(text.parse().unwrap()));
                assert_eq!(source(None, &headers), None);
            }
            for text in ["", "invalid", "192.0.2.1, 192.0.2.2", "192.0.2.1:80"] {
                let mut headers = HeaderMap::new();
                headers.insert("x-forwarded-for", text.parse().unwrap());
                assert_eq!(source(Some(peer), &headers), Some(peer.ip()));
            }
            let mut headers = HeaderMap::new();
            headers.append("x-forwarded-for", "192.0.2.1".parse().unwrap());
            headers.append("x-forwarded-for", "192.0.2.2".parse().unwrap());
            assert_eq!(source(Some(peer), &headers), Some(peer.ip()));
            assert_eq!(source(Some(peer), &HeaderMap::new()), Some(peer.ip()));
        }
    }
}
