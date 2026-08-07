use std::net::{IpAddr, SocketAddr, UdpSocket};

use anyhow::{Context, Result};

pub fn local_ip_for(host: IpAddr, port: u16) -> Result<IpAddr> {
    let bind = if host.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).context("could not create route probe socket")?;
    socket
        .connect(SocketAddr::new(host, port))
        .with_context(|| format!("could not find a network route to {host}"))?;
    Ok(socket.local_addr()?.ip())
}

pub fn private_route() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("could not generate a private media URL")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn http_url(address: SocketAddr, path: &str) -> String {
    debug_assert!(path.starts_with('/'));
    match address.ip() {
        IpAddr::V4(ip) => format!("http://{ip}:{}{path}", address.port()),
        IpAddr::V6(ip) => format!("http://[{ip}]:{}{path}", address.port()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ipv4_http_url() {
        let address = SocketAddr::from(([192, 168, 1, 20], 8080));
        assert_eq!(
            http_url(address, "/private/media"),
            "http://192.168.1.20:8080/private/media"
        );
    }

    #[test]
    fn formats_bracketed_ipv6_http_url() {
        let address = SocketAddr::new("fe80::1234".parse().unwrap(), 8080);
        assert_eq!(
            http_url(address, "/private/media"),
            "http://[fe80::1234]:8080/private/media"
        );
    }

    #[test]
    fn private_routes_are_128_bit_hex_tokens() {
        let first = private_route().unwrap();
        let second = private_route().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
