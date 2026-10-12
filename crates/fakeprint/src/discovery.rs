//! WS-Discovery announcer for a fake printer (UDP 3702 / 239.255.255.250).
//!
//! Note: this is **WS-Discovery**, not classic SSDP (which is UDP 1900). Windows
//! WSD printers are found via WS-Discovery.
//!
//! On Windows, `dasHost` already owns `:3702`. Sharing the port with
//! `SO_REUSEADDR` does not reliably deliver inbound Probes to us, so we:
//!
//! 1. **Always** send Hello (and ProbeMatch when we do see a Probe) from a
//!    dedicated send socket bound to an ephemeral port — that path works.
//! 2. **Best-effort** also bind `:3702` with reuse to answer Probes when the
//!    OS delivers them (Linux / some Windows setups).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};
use uuid::Uuid;

const MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 3702;

/// Device-level print type in Hello / ProbeMatch (Brother uses this, not PrinterServiceType).
const PRINT_DEVICE_TYPE: &str = "wprt:PrintDeviceType";
const PRINT_NS: &str = "http://schemas.microsoft.com/windows/2006/08/wdp/print";

/// Counters shared with `/v1/status` so same-host tests can prove Hello is sending
/// even when another process owns UDP :3702 (Windows `dasHost`).
#[derive(Debug, Default)]
pub struct DiscoveryStats {
    pub hellos_sent: AtomicU64,
    pub probes_answered: AtomicU64,
}

impl DiscoveryStats {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.hellos_sent.load(Ordering::Relaxed),
            self.probes_answered.load(Ordering::Relaxed),
        )
    }
}

pub struct DiscoveryConfig {
    /// Device metadata URL for Hello / ProbeMatch `XAddrs` (not the Scan Service).
    pub endpoint: String,
    pub device_uuid: Uuid,
    /// Retained for callers; Brother omits Scopes, so Hello does not emit them.
    pub friendly_name: String,
    /// Interface used to join / send multicast (LAN IPv4).
    pub multicast_iface: Ipv4Addr,
    /// Bind `:3702` with reuse to answer Probes. Off by default on Windows so we
    /// do not compete with `dasHost` / local Hello listeners for datagrams.
    pub listen_probes: bool,
    pub stats: Arc<DiscoveryStats>,
    /// `AppSequence/@InstanceId` for this process. `0` → set at `run()` from
    /// boot time (seconds since UNIX epoch). Must rise after each restart so
    /// WSDAPI does not treat new Hellos as stale under a reused InstanceId.
    pub instance_id: u64,
    /// Discovery `MetadataVersion` in Hello / ProbeMatch / ResolveMatch.
    /// `0` → same as `instance_id` at `run()`. Bumped on start so clients
    /// refresh cached Transfer Get metadata after a process restart.
    pub metadata_version: u64,
}

/// Boot-time InstanceId / MetadataVersion (seconds since UNIX epoch, ≥ 1).
///
/// WS-Discovery: InstanceId MUST increase after the target service goes down
/// and comes back. WSDAPI ignores Hellos whose MessageNumber is below the last
/// seen value *for that InstanceId* — fixing InstanceId at `1` across restarts
/// makes post-restart Hellos look stale after a long prior run.
pub fn boot_sequence_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().max(1))
        .unwrap_or(1)
}

/// Run WS-Discovery until `stop` is signaled, then multicast Bye and return.
pub async fn run(
    mut cfg: DiscoveryConfig,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    if cfg.instance_id == 0 {
        cfg.instance_id = boot_sequence_id();
    }
    if cfg.metadata_version == 0 {
        cfg.metadata_version = cfg.instance_id;
    }

    let send = bind_send_socket(cfg.multicast_iface)?;
    let recv = if cfg.listen_probes {
        bind_recv_socket(cfg.multicast_iface).ok()
    } else {
        None
    };

    let mcast = SocketAddr::from((MULTICAST, PORT));
    info!(
        endpoint = %cfg.endpoint,
        uuid = %cfg.device_uuid,
        iface = %cfg.multicast_iface,
        instance_id = cfg.instance_id,
        metadata_version = cfg.metadata_version,
        recv_3702 = recv.is_some(),
        "WS-Discovery announcer up (Hello on multicast; Probe receive best-effort)"
    );

    let mut msg_number: u64 = 0;
    // Burst Hellos after bind so a peer that missed the first datagram still
    // sees a fresh InstanceId soon (UDP loss / dasHost race).
    send_hello(&send, &cfg, mcast, &mut msg_number).await;
    for delay_ms in [50u64, 200, 500] {
        tokio::select! {
            _ = &mut stop => {
                send_bye(&send, &cfg, mcast, &mut msg_number).await;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {
                send_hello(&send, &cfg, mcast, &mut msg_number).await;
            }
        }
    }

    let mut buf = vec![0u8; 64 * 1024];
    let mut hello_tick = tokio::time::interval(Duration::from_secs(5));
    hello_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    hello_tick.tick().await;

    if let Some(recv) = recv {
        loop {
            tokio::select! {
                _ = &mut stop => {
                    send_bye(&send, &cfg, mcast, &mut msg_number).await;
                    return Ok(());
                }
                _ = hello_tick.tick() => {
                    send_hello(&send, &cfg, mcast, &mut msg_number).await;
                }
                recv_result = recv.recv_from(&mut buf) => {
                    match recv_result {
                        Ok((n, peer)) => {
                            handle_datagram(&send, &cfg, &buf[..n], peer, &mut msg_number).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "discovery recv error");
                        }
                    }
                }
            }
        }
    } else {
        info!("no :3702 recv socket (dasHost likely exclusive); Hello-only mode");
        loop {
            tokio::select! {
                _ = &mut stop => {
                    send_bye(&send, &cfg, mcast, &mut msg_number).await;
                    return Ok(());
                }
                _ = hello_tick.tick() => {
                    send_hello(&send, &cfg, mcast, &mut msg_number).await;
                }
            }
        }
    }
}

async fn send_hello(
    send: &UdpSocket,
    cfg: &DiscoveryConfig,
    mcast: SocketAddr,
    msg_number: &mut u64,
) {
    *msg_number = msg_number.saturating_add(1);
    let hello = build_hello(cfg, *msg_number);
    match send.send_to(hello.as_bytes(), mcast).await {
        Ok(n) => {
            cfg.stats.hellos_sent.fetch_add(1, Ordering::Relaxed);
            debug!(
                bytes = n,
                msg_number = *msg_number,
                "sent WS-Discovery Hello"
            );
        }
        Err(e) => warn!(error = %e, "Hello send failed"),
    }
}

async fn send_bye(
    send: &UdpSocket,
    cfg: &DiscoveryConfig,
    mcast: SocketAddr,
    msg_number: &mut u64,
) {
    *msg_number = msg_number.saturating_add(1);
    let bye = build_bye(cfg, *msg_number);
    match send.send_to(bye.as_bytes(), mcast).await {
        Ok(n) => {
            info!(bytes = n, msg_number = *msg_number, "sent WS-Discovery Bye");
        }
        Err(e) => warn!(error = %e, "Bye send failed"),
    }
}

async fn handle_datagram(
    send: &UdpSocket,
    cfg: &DiscoveryConfig,
    bytes: &[u8],
    peer: SocketAddr,
    msg_number: &mut u64,
) {
    let text = String::from_utf8_lossy(bytes);
    // Ignore our own / others' match replies.
    if text.contains("ProbeMatch") || text.contains("ResolveMatch") {
        debug!(%peer, bytes = bytes.len(), "discovery Match datagram (ignored)");
        return;
    }
    let is_resolve = text.contains("<wsd:Resolve")
        || text.contains(":Resolve>")
        || text.contains("<Resolve>")
        || text.contains("discovery/Resolve");
    let is_probe = text.contains("<wsd:Probe")
        || text.contains(":Probe>")
        || text.contains("<Probe>")
        || text.contains("discovery/Probe");
    if !is_probe && !is_resolve {
        debug!(%peer, bytes = bytes.len(), "discovery datagram (ignored)");
        return;
    }

    // Resolve always targets our EPR; Probe may filter by Types.
    let types_ok = is_resolve
        || text.contains("PrinterServiceType")
        || text.contains("PrintDeviceType")
        || text.contains("wsdp:Device")
        || text.contains(":Device")
        || !text.contains("Types>");
    if !types_ok {
        debug!(%peer, "Probe Types filter miss");
        return;
    }
    // Resolve for a different device UUID — ignore.
    if is_resolve {
        let uuid_s = cfg.device_uuid.to_string();
        if !text.contains(&uuid_s) && !text.contains(&uuid_s.to_ascii_uppercase()) {
            debug!(%peer, "Resolve for other EPR (ignored)");
            return;
        }
    }

    *msg_number = msg_number.saturating_add(1);
    let relates = extract_message_id(&text).unwrap_or_else(Uuid::new_v4);
    let (kind, match_xml) = if is_resolve {
        (
            "ResolveMatch",
            build_resolve_match(cfg, relates, *msg_number),
        )
    } else {
        ("ProbeMatch", build_probe_match(cfg, relates, *msg_number))
    };
    info!(%peer, bytes = bytes.len(), kind, "WS-Discovery request");
    match send.send_to(match_xml.as_bytes(), peer).await {
        Ok(_) => {
            cfg.stats.probes_answered.fetch_add(1, Ordering::Relaxed);
            info!(%peer, kind, "answered discovery request");
        }
        Err(e) => warn!(%peer, kind, error = %e, "discovery Match send failed"),
    }
}

fn bind_send_socket(iface: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // Ephemeral port — we only send Hello / ProbeMatch from here.
    socket.bind(&socket2::SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        0,
    )))?;
    if !iface.is_unspecified() {
        let _ = socket.set_multicast_if_v4(&iface);
    }
    // Loopback so a same-host listener (test-discovery.ps1) can hear Hello.
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(4)?;
    socket.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_sock).context("tokio send socket")
}

fn bind_recv_socket(iface: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(windows)]
    {
        // Allow sharing with dasHost; do not request exclusive use.
        use std::os::windows::io::AsRawSocket;
        // socket2 sets exclusive=false when reuse_address is set on Windows.
        let _ = socket.as_raw_socket();
    }
    socket
        .bind(&socket2::SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            PORT,
        )))
        .context("bind :3702 for Probe recv")?;
    socket.set_multicast_loop_v4(true)?;
    socket
        .join_multicast_v4(&MULTICAST, &iface)
        .with_context(|| format!("join multicast on {iface}"))?;
    if !iface.is_unspecified() {
        let _ = socket.join_multicast_v4(&MULTICAST, &Ipv4Addr::UNSPECIFIED);
    }
    socket.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_sock).context("tokio recv socket")
}

fn extract_message_id(probe: &str) -> Option<Uuid> {
    for tag in ["MessageID>", "MessageId>"] {
        if let Some(i) = probe.find(tag) {
            let rest = &probe[i + tag.len()..];
            let end = rest.find('<')?;
            let raw = rest[..end].trim().trim_start_matches("urn:uuid:");
            return Uuid::parse_str(raw).ok();
        }
    }
    None
}

fn build_hello(cfg: &DiscoveryConfig, msg_number: u64) -> String {
    let msg_id = Uuid::new_v4();
    let _ = &cfg.friendly_name;
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery"
               xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
               xmlns:wprt="{PRINT_NS}">
  <soap:Header>
    <wsa:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</wsa:To>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Hello</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg_id}</wsa:MessageID>
    <wsd:AppSequence InstanceId="{instance_id}" MessageNumber="{msg_number}"/>
  </soap:Header>
  <soap:Body>
    <wsd:Hello>
      <wsa:EndpointReference>
        <wsa:Address>urn:uuid:{uuid}</wsa:Address>
      </wsa:EndpointReference>
      <wsd:Types>wsdp:Device {print_type}</wsd:Types>
      <wsd:XAddrs>{xaddrs}</wsd:XAddrs>
      <wsd:MetadataVersion>{metadata_version}</wsd:MetadataVersion>
    </wsd:Hello>
  </soap:Body>
</soap:Envelope>"#,
        msg_id = msg_id,
        instance_id = cfg.instance_id,
        msg_number = msg_number,
        uuid = cfg.device_uuid,
        PRINT_NS = PRINT_NS,
        print_type = PRINT_DEVICE_TYPE,
        xaddrs = cfg.endpoint,
        metadata_version = cfg.metadata_version,
    )
}

fn build_bye(cfg: &DiscoveryConfig, msg_number: u64) -> String {
    let msg_id = Uuid::new_v4();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery"
               xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
               xmlns:wprt="{PRINT_NS}">
  <soap:Header>
    <wsa:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</wsa:To>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Bye</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg_id}</wsa:MessageID>
    <wsd:AppSequence InstanceId="{instance_id}" MessageNumber="{msg_number}"/>
  </soap:Header>
  <soap:Body>
    <wsd:Bye>
      <wsa:EndpointReference>
        <wsa:Address>urn:uuid:{uuid}</wsa:Address>
      </wsa:EndpointReference>
      <wsd:Types>wsdp:Device {print_type}</wsd:Types>
      <wsd:XAddrs>{xaddrs}</wsd:XAddrs>
    </wsd:Bye>
  </soap:Body>
</soap:Envelope>"#,
        msg_id = msg_id,
        instance_id = cfg.instance_id,
        msg_number = msg_number,
        uuid = cfg.device_uuid,
        PRINT_NS = PRINT_NS,
        print_type = PRINT_DEVICE_TYPE,
        xaddrs = cfg.endpoint,
    )
}

fn build_probe_match(cfg: &DiscoveryConfig, relates: Uuid, msg_number: u64) -> String {
    let msg_id = Uuid::new_v4();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery"
               xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
               xmlns:wprt="{PRINT_NS}">
  <soap:Header>
    <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg_id}</wsa:MessageID>
    <wsa:RelatesTo>urn:uuid:{relates}</wsa:RelatesTo>
    <wsd:AppSequence InstanceId="{instance_id}" MessageNumber="{msg_number}"/>
  </soap:Header>
  <soap:Body>
    <wsd:ProbeMatches>
      <wsd:ProbeMatch>
        <wsa:EndpointReference>
          <wsa:Address>urn:uuid:{uuid}</wsa:Address>
        </wsa:EndpointReference>
        <wsd:Types>wsdp:Device {print_type}</wsd:Types>
        <wsd:XAddrs>{xaddrs}</wsd:XAddrs>
        <wsd:MetadataVersion>{metadata_version}</wsd:MetadataVersion>
      </wsd:ProbeMatch>
    </wsd:ProbeMatches>
  </soap:Body>
</soap:Envelope>"#,
        msg_id = msg_id,
        relates = relates,
        instance_id = cfg.instance_id,
        msg_number = msg_number,
        uuid = cfg.device_uuid,
        PRINT_NS = PRINT_NS,
        print_type = PRINT_DEVICE_TYPE,
        xaddrs = cfg.endpoint,
        metadata_version = cfg.metadata_version,
    )
}

/// Windows WSDAPI clients often Resolve after Hello (they ignore Hello XAddrs).
fn build_resolve_match(cfg: &DiscoveryConfig, relates: Uuid, msg_number: u64) -> String {
    let msg_id = Uuid::new_v4();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery"
               xmlns:wsdp="http://schemas.xmlsoap.org/ws/2006/02/devprof"
               xmlns:wprt="{PRINT_NS}">
  <soap:Header>
    <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ResolveMatches</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg_id}</wsa:MessageID>
    <wsa:RelatesTo>urn:uuid:{relates}</wsa:RelatesTo>
    <wsd:AppSequence InstanceId="{instance_id}" MessageNumber="{msg_number}"/>
  </soap:Header>
  <soap:Body>
    <wsd:ResolveMatches>
      <wsd:ResolveMatch>
        <wsa:EndpointReference>
          <wsa:Address>urn:uuid:{uuid}</wsa:Address>
        </wsa:EndpointReference>
        <wsd:Types>wsdp:Device {print_type}</wsd:Types>
        <wsd:XAddrs>{xaddrs}</wsd:XAddrs>
        <wsd:MetadataVersion>{metadata_version}</wsd:MetadataVersion>
      </wsd:ResolveMatch>
    </wsd:ResolveMatches>
  </soap:Body>
</soap:Envelope>"#,
        msg_id = msg_id,
        relates = relates,
        instance_id = cfg.instance_id,
        msg_number = msg_number,
        uuid = cfg.device_uuid,
        PRINT_NS = PRINT_NS,
        print_type = PRINT_DEVICE_TYPE,
        xaddrs = cfg.endpoint,
        metadata_version = cfg.metadata_version,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(instance_id: u64, metadata_version: u64) -> DiscoveryConfig {
        DiscoveryConfig {
            endpoint: "http://192.0.2.10:3921/WebServices/Device".into(),
            device_uuid: Uuid::parse_str("00000000-0000-4000-8000-0000000000f3").unwrap(),
            friendly_name: "fakeprint".into(),
            multicast_iface: Ipv4Addr::UNSPECIFIED,
            listen_probes: false,
            stats: Arc::new(DiscoveryStats::default()),
            instance_id,
            metadata_version,
        }
    }

    #[test]
    fn hello_carries_boot_instance_and_metadata_version() {
        let xml = build_hello(&cfg(1_700_000_000, 1_700_000_001), 3);
        assert!(
            xml.contains(r#"InstanceId="1700000000""#),
            "InstanceId must not stay hardcoded at 1: {xml}"
        );
        assert!(
            xml.contains(r#"MessageNumber="3""#),
            "MessageNumber missing: {xml}"
        );
        assert!(
            xml.contains("<wsd:MetadataVersion>1700000001</wsd:MetadataVersion>"),
            "MetadataVersion must track boot bump: {xml}"
        );
        assert!(xml.contains("wprt:PrintDeviceType"));
        assert!(xml.contains("http://192.0.2.10:3921/WebServices/Device"));
        assert!(!xml.contains(r#"InstanceId="1""#));
    }

    #[test]
    fn probe_and_resolve_match_share_sequence_fields() {
        let relates = Uuid::nil();
        let probe = build_probe_match(&cfg(42, 99), relates, 7);
        let resolve = build_resolve_match(&cfg(42, 99), relates, 8);
        for (label, xml) in [
            ("ProbeMatch", probe.as_str()),
            ("ResolveMatch", resolve.as_str()),
        ] {
            assert!(
                xml.contains(r#"InstanceId="42""#),
                "{label} InstanceId: {xml}"
            );
            assert!(
                xml.contains("<wsd:MetadataVersion>99</wsd:MetadataVersion>"),
                "{label} MetadataVersion: {xml}"
            );
        }
        assert!(probe.contains(r#"MessageNumber="7""#));
        assert!(resolve.contains(r#"MessageNumber="8""#));
    }

    #[test]
    fn bye_reuses_instance_id_without_metadata_version() {
        let xml = build_bye(&cfg(55, 66), 9);
        assert!(xml.contains(r#"InstanceId="55""#));
        assert!(xml.contains(r#"MessageNumber="9""#));
        assert!(
            !xml.contains("MetadataVersion"),
            "Bye should not carry MetadataVersion: {xml}"
        );
    }

    #[test]
    fn boot_sequence_id_is_nonzero() {
        assert!(boot_sequence_id() >= 1);
    }
}
