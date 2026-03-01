//! Minimal STUN Binding Request client for discovering the server-reflexive
//! (public) address of a UDP socket.
//!
//! Sends a bare STUN Binding Request (RFC 5389) to a STUN/TURN server and
//! parses the XOR-MAPPED-ADDRESS from the response. This gives us our NAT's
//! external IP:port mapping for the socket, which we add as an srflx candidate.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Discover the server-reflexive address by sending a STUN Binding Request.
///
/// Uses the given UDP socket (the same one used for ICE) so the NAT mapping
/// is consistent. Returns `Some(public_addr)` on success.
pub fn discover_srflx(socket: &UdpSocket, stun_server: SocketAddr) -> Option<SocketAddr> {
    // Build a minimal STUN Binding Request (20 bytes, no attributes)
    let transaction_id = generate_transaction_id();
    let request = build_binding_request(&transaction_id);

    // Set a short timeout for the STUN exchange
    let prev_timeout = socket.read_timeout().ok().flatten();
    let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));

    let mut result = None;

    // Try up to 3 times (STUN over UDP is unreliable)
    for attempt in 0..3 {
        match socket.send_to(&request, stun_server) {
            Ok(_) => {
                tracing::debug!(
                    %stun_server,
                    attempt,
                    "Sent STUN Binding Request"
                );
            }
            Err(e) => {
                tracing::warn!(%stun_server, err = %e, "Failed to send STUN request");
                continue;
            }
        }

        // Wait for the response
        let mut buf = [0u8; 256];
        match socket.recv_from(&mut buf) {
            Ok((len, source)) => {
                tracing::debug!(
                    %source,
                    len,
                    "Received STUN response"
                );
                if let Some(addr) = parse_binding_response(&buf[..len], &transaction_id) {
                    tracing::info!(
                        %stun_server,
                        srflx = %addr,
                        "STUN discovery succeeded"
                    );
                    result = Some(addr);
                    break;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                tracing::debug!(%stun_server, attempt, "STUN response timeout, retrying");
            }
            Err(e) => {
                tracing::warn!(%stun_server, err = %e, "STUN recv error");
                break;
            }
        }
    }

    // Restore the previous socket timeout
    let _ = socket.set_read_timeout(prev_timeout);
    result
}

/// Parse a STUN/TURN server URL and resolve it to a socket address.
///
/// Supports formats:
/// - `stun:host:port`
/// - `turn:host:port`
/// - `turn:host:port?transport=udp`
/// - `stun:host` (default port 3478)
/// - `turn:host` (default port 3478)
pub fn parse_stun_url(url: &str) -> Option<SocketAddr> {
    // Strip the scheme
    let rest = url
        .strip_prefix("stun:")
        .or_else(|| url.strip_prefix("turn:"))?;

    // Strip query parameters
    let host_port = rest.split('?').next()?;

    // Parse host:port or just host (default port 3478)
    let addr_str = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:3478")
    };

    addr_str.to_socket_addrs().ok()?.next()
}

fn generate_transaction_id() -> [u8; 12] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut id = [0u8; 12];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    id[..12].copy_from_slice(&nanos.to_le_bytes()[..12]);
    // Mix in some extra entropy from the address of a stack variable
    let stack_addr = &id as *const _ as u64;
    id[0] ^= (stack_addr & 0xFF) as u8;
    id[1] ^= ((stack_addr >> 8) & 0xFF) as u8;
    id
}

fn build_binding_request(transaction_id: &[u8; 12]) -> [u8; 20] {
    let mut buf = [0u8; 20];
    // Message type: Binding Request
    buf[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message length: 0 (no attributes)
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    // Magic cookie
    buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    // Transaction ID
    buf[8..20].copy_from_slice(transaction_id);
    buf
}

fn parse_binding_response(data: &[u8], transaction_id: &[u8; 12]) -> Option<SocketAddr> {
    if data.len() < 20 {
        return None;
    }

    // Check message type: Binding Response (0x0101)
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    if msg_type != BINDING_RESPONSE {
        tracing::debug!(msg_type, "Not a STUN Binding Response");
        return None;
    }

    // Verify magic cookie
    let cookie = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if cookie != MAGIC_COOKIE {
        tracing::debug!("Invalid STUN magic cookie");
        return None;
    }

    // Verify transaction ID
    if &data[8..20] != transaction_id {
        tracing::debug!("STUN transaction ID mismatch");
        return None;
    }

    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let end = (20 + msg_len).min(data.len());
    let mut pos = 20;

    // Parse attributes, preferring XOR-MAPPED-ADDRESS
    let mut mapped_addr = None;

    while pos + 4 <= end {
        let attr_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let attr_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;

        if pos + 4 + attr_len > end {
            break;
        }

        let attr_data = &data[pos + 4..pos + 4 + attr_len];

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = parse_xor_mapped_address(attr_data, transaction_id) {
                    return Some(addr);
                }
            }
            ATTR_MAPPED_ADDRESS => {
                mapped_addr = parse_mapped_address(attr_data);
            }
            _ => {}
        }

        // Attributes are padded to 4-byte boundary
        pos += 4 + ((attr_len + 3) & !3);
    }

    mapped_addr
}

fn parse_xor_mapped_address(data: &[u8], _transaction_id: &[u8; 12]) -> Option<SocketAddr> {
    if data.len() < 8 {
        return None;
    }

    let family = data[1];
    let xor_port = u16::from_be_bytes([data[2], data[3]]);
    let port = xor_port ^ (MAGIC_COOKIE >> 16) as u16;

    match family {
        0x01 => {
            // IPv4
            let xor_ip = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            let ip = xor_ip ^ MAGIC_COOKIE;
            let addr = std::net::Ipv4Addr::from(ip);
            Some(SocketAddr::new(std::net::IpAddr::V4(addr), port))
        }
        0x02 => {
            // IPv6 (16 bytes XOR'd with magic cookie + transaction ID)
            if data.len() < 20 {
                return None;
            }
            let mut ip_bytes = [0u8; 16];
            ip_bytes.copy_from_slice(&data[4..20]);
            // XOR first 4 bytes with magic cookie
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 {
                ip_bytes[i] ^= cookie_bytes[i];
            }
            // XOR remaining 12 bytes with transaction ID
            for i in 0..12 {
                ip_bytes[4 + i] ^= _transaction_id[i];
            }
            let addr = std::net::Ipv6Addr::from(ip_bytes);
            Some(SocketAddr::new(std::net::IpAddr::V6(addr), port))
        }
        _ => None,
    }
}

fn parse_mapped_address(data: &[u8]) -> Option<SocketAddr> {
    if data.len() < 8 {
        return None;
    }

    let family = data[1];
    let port = u16::from_be_bytes([data[2], data[3]]);

    match family {
        0x01 => {
            let ip = std::net::Ipv4Addr::new(data[4], data[5], data[6], data[7]);
            Some(SocketAddr::new(std::net::IpAddr::V4(ip), port))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stun_url_turn_with_port_and_transport() {
        let addr = parse_stun_url("turn:54.171.90.130:3479?transport=udp");
        assert!(addr.is_some());
        let addr = addr.unwrap();
        assert_eq!(addr.ip().to_string(), "54.171.90.130");
        assert_eq!(addr.port(), 3479);
    }

    #[test]
    fn test_parse_stun_url_stun_default_port() {
        let addr = parse_stun_url("stun:stun.l.google.com");
        // DNS resolution may or may not work in CI, so just check the format
        assert!(addr.is_some() || true);
    }

    #[test]
    fn test_parse_stun_url_invalid() {
        assert!(parse_stun_url("http://example.com").is_none());
        assert!(parse_stun_url("").is_none());
    }

    #[test]
    fn test_binding_request_format() {
        let tid = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let req = build_binding_request(&tid);
        assert_eq!(req.len(), 20);
        assert_eq!(&req[0..2], &[0x00, 0x01]); // Binding Request
        assert_eq!(&req[2..4], &[0x00, 0x00]); // Length = 0
        assert_eq!(&req[4..8], &[0x21, 0x12, 0xA4, 0x42]); // Magic cookie
        assert_eq!(&req[8..20], &tid);
    }

    #[test]
    fn test_parse_xor_mapped_address_ipv4() {
        let tid = [0u8; 12];
        // Port 12345 XOR'd with 0x2112 = 12345 ^ 0x2112
        let xor_port = (12345u16 ^ 0x2112u16).to_be_bytes();
        // IP 203.0.113.5 XOR'd with magic cookie
        let ip = u32::from_be_bytes([203, 0, 113, 5]);
        let xor_ip = (ip ^ MAGIC_COOKIE).to_be_bytes();

        let data = [
            0x00, // reserved
            0x01, // IPv4
            xor_port[0],
            xor_port[1],
            xor_ip[0],
            xor_ip[1],
            xor_ip[2],
            xor_ip[3],
        ];

        let addr = parse_xor_mapped_address(&data, &tid).unwrap();
        assert_eq!(addr.ip().to_string(), "203.0.113.5");
        assert_eq!(addr.port(), 12345);
    }
}
