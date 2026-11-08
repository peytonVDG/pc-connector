//! Small networking helpers shared by host and client.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::protocol::ControlMsg;

pub fn hostname() -> String {
    if let Ok(name) = std::env::var("COMPUTERNAME")
        && !name.is_empty()
    {
        return name;
    }
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(name) = std::fs::read_to_string(path) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "pc-connector".into())
}

pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unknown"
    }
}

/// Non-loopback IPv4 addresses of this machine (for display to the user).
pub fn local_ipv4_addrs() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = &iface.addr
                && !v4.ip.is_loopback()
                && !v4.ip.is_link_local()
                && !out.contains(&v4.ip)
            {
                out.push(v4.ip);
            }
        }
    }
    out
}

/// Broadcast addresses of all IPv4 interfaces, plus the limited broadcast address.
pub fn broadcast_addrs() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                if v4.ip.is_loopback() {
                    continue;
                }
                let bcast = v4.broadcast.unwrap_or_else(|| {
                    let ip = u32::from(v4.ip);
                    let mask = u32::from(v4.netmask);
                    Ipv4Addr::from(ip | !mask)
                });
                if !out.contains(&bcast) {
                    out.push(bcast);
                }
            }
        }
    }
    out
}

/// Parse "host", "host:port", "1.2.3.4" or "1.2.3.4:port" into a socket address.
pub fn resolve(addr: &str, default_port: u16) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    let addr = addr.trim();
    let with_port = if addr.parse::<SocketAddr>().is_ok() {
        addr.to_string()
    } else if addr.parse::<IpAddr>().is_ok() {
        format!("{addr}:{default_port}")
    } else if addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        addr.to_string()
    } else {
        format!("{addr}:{default_port}")
    };
    with_port
        .to_socket_addrs()
        .with_context(|| format!("could not resolve '{addr}'"))?
        .find(|a| a.is_ipv4())
        .or_else(|| with_port.to_socket_addrs().ok()?.next())
        .with_context(|| format!("no address found for '{addr}'"))
}

/// Mark a UDP socket's packets for the Wi-Fi "voice"/"video" access categories (WMM).
/// Best effort: some OSes ignore it without admin rights, which is fine.
pub fn set_dscp(sock: &UdpSocket, dscp: u8) {
    let s = socket2::SockRef::from(sock);
    let _ = s.set_tos_v4((dscp as u32) << 2);
}

pub fn set_buffers(sock: &UdpSocket, recv: usize, send: usize) {
    let s = socket2::SockRef::from(sock);
    if recv > 0 {
        let _ = s.set_recv_buffer_size(recv);
    }
    if send > 0 {
        let _ = s.set_send_buffer_size(send);
    }
}

/// Newline-delimited JSON framing for the control channel.
pub struct ControlReader {
    reader: BufReader<TcpStream>,
    line: String,
}

impl ControlReader {
    pub fn new(stream: TcpStream) -> Self {
        Self { reader: BufReader::new(stream), line: String::new() }
    }

    /// Blocks until a message arrives. `Ok(None)` means the peer closed the connection.
    /// Read timeouts (if set on the socket) surface as `Err` of kind WouldBlock/TimedOut.
    pub fn read(&mut self) -> std::io::Result<Option<ControlMsg>> {
        loop {
            // `read_line` appends, so a timeout mid-line keeps the partial data for the next call.
            let n = self.reader.read_line(&mut self.line)?;
            if n == 0 {
                return Ok(None);
            }
            if !self.line.ends_with('\n') {
                continue;
            }
            let text = std::mem::take(&mut self.line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<ControlMsg>(text) {
                Ok(msg) => return Ok(Some(msg)),
                Err(e) => log::warn!("Ignoring malformed control message ({e}): {text}"),
            }
        }
    }
}

#[derive(Clone)]
pub struct ControlWriter {
    stream: std::sync::Arc<std::sync::Mutex<TcpStream>>,
}

impl ControlWriter {
    pub fn new(stream: TcpStream) -> Self {
        Self { stream: std::sync::Arc::new(std::sync::Mutex::new(stream)) }
    }

    pub fn send(&self, msg: &ControlMsg) -> std::io::Result<()> {
        let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        s.write_all(line.as_bytes())?;
        s.flush()
    }

    pub fn shutdown(&self) {
        let s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

pub fn tune_control_stream(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let s = socket2::SockRef::from(stream);
    let keepalive =
        socket2::TcpKeepalive::new().with_time(Duration::from_secs(2)).with_interval(Duration::from_secs(1));
    let _ = s.set_tcp_keepalive(&keepalive);
}

pub fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_forms() {
        assert_eq!(resolve("127.0.0.1", 10).unwrap(), "127.0.0.1:10".parse().unwrap());
        assert_eq!(resolve("127.0.0.1:99", 10).unwrap(), "127.0.0.1:99".parse().unwrap());
        assert_eq!(resolve(" localhost ", 10).unwrap().port(), 10);
        assert_eq!(resolve("localhost:77", 10).unwrap().port(), 77);
    }
}
