//! LAN discovery: the client broadcasts a probe, hosts answer with a small JSON blob.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{DISCOVERY_PORT, DISCOVERY_PROBE, DiscoveryReply};

/// Answers discovery probes until `stop` is set.
pub fn spawn_responder(
    reply: Arc<Mutex<DiscoveryReply>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    sock.set_reuse_address(true)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).into())?;
    let sock: UdpSocket = sock.into();
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    std::thread::Builder::new().name("discovery".into()).spawn(move || {
        let mut buf = [0u8; 512];
        while !stop.load(Ordering::Relaxed) {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if &buf[..n] == DISCOVERY_PROBE {
                        let json = {
                            let r = reply.lock().unwrap();
                            serde_json::to_vec(&*r).unwrap_or_default()
                        };
                        let _ = sock.send_to(&json, from);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    log::warn!("discovery socket error: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    })
}

#[derive(Debug, Clone)]
pub struct FoundHost {
    pub reply: DiscoveryReply,
    pub addr: SocketAddr,
    pub last_seen: Instant,
}

/// Continuously probes the LAN in a background thread while alive.
pub struct Browser {
    hosts: Arc<Mutex<HashMap<SocketAddr, FoundHost>>>,
    stop: Arc<AtomicBool>,
}

impl Browser {
    pub fn start() -> Browser {
        let hosts: Arc<Mutex<HashMap<SocketAddr, FoundHost>>> = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hosts.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("discovery-browse".into()).spawn(move || {
            if let Err(e) = browse_loop(h, s) {
                log::warn!("LAN discovery unavailable: {e}");
            }
        });
        Browser { hosts, stop }
    }

    pub fn hosts(&self) -> Vec<FoundHost> {
        let mut v: Vec<FoundHost> = self
            .hosts
            .lock()
            .unwrap()
            .values()
            .filter(|h| h.last_seen.elapsed() < Duration::from_secs(8))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.reply.name.cmp(&b.reply.name));
        v
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn browse_loop(hosts: Arc<Mutex<HashMap<SocketAddr, FoundHost>>>, stop: Arc<AtomicBool>) -> std::io::Result<()> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut last_probe = Instant::now() - Duration::from_secs(10);
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if last_probe.elapsed() > Duration::from_secs(2) {
            last_probe = Instant::now();
            for b in crate::net::broadcast_addrs() {
                let _ = sock.send_to(DISCOVERY_PROBE, SocketAddrV4::new(b, DISCOVERY_PORT));
            }
            // Also probe localhost so a host on the same machine shows up (handy for testing).
            let _ = sock.send_to(DISCOVERY_PROBE, SocketAddrV4::new(Ipv4Addr::LOCALHOST, DISCOVERY_PORT));
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Ok(reply) = serde_json::from_slice::<DiscoveryReply>(&buf[..n]) {
                    if reply.app != "pc-connector" {
                        continue;
                    }
                    let addr = SocketAddr::new(from.ip(), reply.control_port);
                    let mut map = hosts.lock().unwrap();
                    // Prefer a LAN address over loopback for the same host name.
                    if from.ip().is_loopback()
                        && map.values().any(|h| h.reply.name == reply.name && !h.addr.ip().is_loopback())
                    {
                        continue;
                    }
                    if !from.ip().is_loopback() {
                        map.retain(|_, h| !(h.reply.name == reply.name && h.addr.ip().is_loopback()));
                    }
                    map.insert(addr, FoundHost { reply, addr, last_seen: Instant::now() });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                // Windows reports ICMP "port unreachable" from earlier sends as recv errors.
                log::debug!("discovery recv: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Ok(())
}
