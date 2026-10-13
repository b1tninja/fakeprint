//! WS-Discovery (2005/04) target service for the fake printer (UDP 3702, 239.255.255.250).
//!
//! This is WS-Discovery, not SSDP (UDP 1900). Layers:
//!
//! * [`DiscoveryCore`] is the protocol state machine and does no I/O: it parses datagrams
//!   with the real XML parser (no substring matching), matches Probe `Types`/`Scopes` and
//!   Resolve addresses, suppresses duplicate MessageIDs, rate-limits responses and builds
//!   Hello / Bye / ProbeMatches / ResolveMatches with one shared `AppSequence`
//!   (InstanceId + increasing MessageNumber). It returns [`Transmit`]s that say what to send,
//!   to whom and after how long (spec jitter and retransmission schedule).
//! * [`DiscoveryService`] wraps the core in a mutex for sharing with the HTTP endpoint that
//!   answers directed (unicast) Probe/Resolve.
//! * [`DiscoverySocket`] abstracts the multicast socket; [`UdpDiscoverySocket`] is the real
//!   one, tests use an in-memory fake. [`run_service`] is the event loop.
//!
//! On Windows `dasHost` owns `:3702`. We always send from a dedicated ephemeral socket and
//! best-effort also bind `:3702` with address reuse to hear Probes (`listen_probes`). The
//! periodic Hello (default 5 s) keeps Windows treating the device as present when it cannot
//! hear Probes; it is a documented lab behaviour, not part of the spec.
//!
//! Replies to multicast Probe/Resolve always go to the datagram's source address, never to
//! a `ReplyTo` in the message (no redirection of our traffic to third parties).

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::wsd::envelope::{EndpointRef, SoapEnvelope};
use crate::wsd::fault::{FaultCode, PrintFault};
use crate::wsd::soap::{SoapReply, SoapRequest};
use crate::wsd::xml::{is_ncname, ns, Element, PrefixMap, QName, XmlLimits};

pub const MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const PORT: u16 = 3702;

pub const ACTION_HELLO: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/Hello";
pub const ACTION_BYE: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/Bye";
pub const ACTION_PROBE: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe";
pub const ACTION_PROBE_MATCHES: &str =
    "http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches";
pub const ACTION_RESOLVE: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/Resolve";
pub const ACTION_RESOLVE_MATCHES: &str =
    "http://schemas.xmlsoap.org/ws/2005/04/discovery/ResolveMatches";
/// `wsa:To` of multicast discovery messages.
pub const DISCOVERY_TO: &str = "urn:schemas-xmlsoap-org:ws:2005:04:discovery";
/// HTTP path of the directed (unicast) discovery endpoint.
pub const DIRECTED_PATH: &str =
    "/StableWSDiscoveryEndpoint/schemas-xmlsoap-org_ws_2005_04_discovery";

pub const MATCH_BY_RFC3986: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/rfc3986";
pub const MATCH_BY_UUID: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/uuid";
pub const MATCH_BY_LDAP: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/ldap";
pub const MATCH_BY_STRCMP0: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/strcmp0";
pub const MATCH_BY_NONE: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/none";

const MAX_TOKENS: usize = 32;
const MAX_TOKEN_LEN: usize = 1024;
const MAX_DATAGRAM: usize = 65_507;
const MAX_SEEN: usize = 512;
const MAX_RESPONSES_PER_SECOND: usize = 64;

/// Limits for parsing one datagram.
const DATAGRAM_LIMITS: XmlLimits = XmlLimits {
    max_input: MAX_DATAGRAM,
    max_depth: 16,
    max_nodes: 400,
    max_attrs: 16,
    max_ns_decls: 16,
    max_name: 128,
    max_attr_value: 2048,
    max_text: 16 * 1024,
};

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// Counters shared with `/v1/status` so same-host tests can prove Hello is sending even
/// when another process owns UDP :3702 (Windows `dasHost`).
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

/// Delays and repeats (WS-Discovery + SOAP-over-UDP defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryTimings {
    /// Random delay before Hello and before answering Probe/Resolve (0..=this).
    pub app_max_delay: Duration,
    pub udp_min_delay: Duration,
    pub udp_max_delay: Duration,
    pub udp_upper_delay: Duration,
    /// Extra transmissions of a multicast message (Hello, Bye).
    pub multicast_repeat: u32,
    /// Extra transmissions of a unicast message (matches).
    pub unicast_repeat: u32,
    /// Interval of the lab re-Hello; `None` disables it.
    pub hello_interval: Option<Duration>,
    /// How long a seen MessageID suppresses a duplicate request.
    pub dedupe_window: Duration,
}

impl Default for DiscoveryTimings {
    fn default() -> Self {
        Self {
            app_max_delay: Duration::from_millis(500),
            udp_min_delay: Duration::from_millis(50),
            udp_max_delay: Duration::from_millis(250),
            udp_upper_delay: Duration::from_millis(450),
            multicast_repeat: 2,
            unicast_repeat: 1,
            hello_interval: Some(Duration::from_secs(5)),
            dedupe_window: Duration::from_secs(30),
        }
    }
}

impl DiscoveryTimings {
    /// No delays, no repeats, no periodic Hello (tests).
    pub fn immediate() -> Self {
        Self {
            app_max_delay: Duration::ZERO,
            udp_min_delay: Duration::ZERO,
            udp_max_delay: Duration::ZERO,
            udp_upper_delay: Duration::ZERO,
            multicast_repeat: 0,
            unicast_repeat: 0,
            hello_interval: None,
            dedupe_window: Duration::from_secs(30),
        }
    }
}

/// Opt-in deviations from the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiscoveryQuirks {
    /// Also answer Probes for the hosted service type `wprt:PrinterServiceType` (the old
    /// loose matcher did; the specification only matches device types).
    pub match_hosted_service_type: bool,
}

pub struct DiscoveryConfig {
    /// Device metadata URL for `XAddrs`.
    pub endpoint: String,
    pub device_uuid: Uuid,
    /// Retained for callers; Hello does not carry it.
    pub friendly_name: String,
    /// Interface used to join / send multicast (LAN IPv4).
    pub multicast_iface: Ipv4Addr,
    /// Bind `:3702` with reuse to answer Probes. Off by default on Windows.
    pub listen_probes: bool,
    pub stats: Arc<DiscoveryStats>,
    /// `AppSequence/@InstanceId`; `0` selects the boot time. It must rise after every
    /// restart so WSDAPI does not treat new Hellos as stale.
    pub instance_id: u64,
    /// `MetadataVersion`; `0` means the same as the instance id.
    pub metadata_version: u64,
    /// Target scopes (usually none: Windows printers omit them).
    pub scopes: Vec<String>,
    /// Target types; empty selects `wsdp:Device wprt:PrintDeviceType`.
    pub types: Vec<QName>,
    pub timings: DiscoveryTimings,
    pub quirks: DiscoveryQuirks,
}

impl DiscoveryConfig {
    pub fn new(endpoint: impl Into<String>, device_uuid: Uuid) -> Self {
        Self {
            endpoint: endpoint.into(),
            device_uuid,
            friendly_name: "fakeprint".into(),
            multicast_iface: Ipv4Addr::UNSPECIFIED,
            listen_probes: false,
            stats: Arc::new(DiscoveryStats::default()),
            instance_id: 0,
            metadata_version: 0,
            scopes: Vec::new(),
            types: Vec::new(),
            timings: DiscoveryTimings::default(),
            quirks: DiscoveryQuirks::default(),
        }
    }
}

/// Boot-time InstanceId / MetadataVersion (seconds since the epoch, at least 1).
///
/// InstanceId must increase after the service goes down and comes back; WSDAPI ignores
/// Hellos whose MessageNumber is below the last one seen for that InstanceId.
pub fn boot_sequence_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().max(1))
        .unwrap_or(1)
}

/// The default device types.
pub fn default_types() -> Vec<QName> {
    vec![
        QName::new(ns::WSDP, "Device"),
        QName::new(ns::PRINT, "PrintDeviceType"),
    ]
}

// ---------------------------------------------------------------------------------------------
// Jitter
// ---------------------------------------------------------------------------------------------

/// Source of randomness for the protocol delays.
pub trait Jitter: Send {
    /// A value in `0..=max_ms`.
    fn up_to(&mut self, max_ms: u64) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RandomJitter;

impl Jitter for RandomJitter {
    fn up_to(&mut self, max_ms: u64) -> u64 {
        if max_ms == 0 {
            return 0;
        }
        (Uuid::new_v4().as_u128() % (max_ms as u128 + 1)) as u64
    }
}

/// Always returns `min(value, max)`.
#[derive(Debug, Clone, Copy)]
pub struct FixedJitter(pub u64);

impl Jitter for FixedJitter {
    fn up_to(&mut self, max_ms: u64) -> u64 {
        self.0.min(max_ms)
    }
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/// Scope matching rule of a Probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchBy {
    Rfc3986,
    Uuid,
    Ldap,
    Strcmp0,
    None,
    Unknown(String),
}

impl MatchBy {
    fn parse(uri: Option<&str>) -> MatchBy {
        match uri.map(str::trim) {
            None | Some(MATCH_BY_RFC3986) => MatchBy::Rfc3986,
            Some(MATCH_BY_UUID) => MatchBy::Uuid,
            Some(MATCH_BY_LDAP) => MatchBy::Ldap,
            Some(MATCH_BY_STRCMP0) => MatchBy::Strcmp0,
            Some(MATCH_BY_NONE) => MatchBy::None,
            Some(other) => MatchBy::Unknown(other.to_string()),
        }
    }
}

/// A parsed Probe or Resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Probe {
        types: Vec<QName>,
        scopes: Vec<String>,
        match_by: MatchBy,
    },
    Resolve {
        address: String,
    },
}

fn resolve_token(el: &Element, token: &str) -> Option<QName> {
    let (prefix, local) = match token.split_once(':') {
        Some((p, l)) => (p, l),
        None => ("", token),
    };
    if !is_ncname(local) || (!prefix.is_empty() && !is_ncname(prefix)) {
        return None;
    }
    let uri = if prefix.is_empty() {
        el.resolve_prefix("").unwrap_or_default()
    } else {
        el.resolve_prefix(prefix)?
    };
    Some(QName::new(&uri, local))
}

fn tokens(el: &Element) -> Result<Vec<String>, String> {
    let t: Vec<String> = el.text.split_whitespace().map(str::to_string).collect();
    if t.len() > MAX_TOKENS || t.iter().any(|s| s.len() > MAX_TOKEN_LEN) {
        return Err("too many or too long list entries".into());
    }
    Ok(t)
}

/// Read the query from the body element of a Probe/Resolve (`action` selects which).
pub fn parse_query(action: &str, body: &Element) -> Result<Option<Query>, String> {
    match action {
        ACTION_PROBE => {
            if !body.is(ns::WSD, "Probe") {
                return Ok(None);
            }
            let mut types = Vec::new();
            let mut scopes = Vec::new();
            let mut match_by = MatchBy::Rfc3986;
            let (mut seen_types, mut seen_scopes) = (false, false);
            for c in body.children.iter().filter(|c| c.name.ns == ns::WSD) {
                match c.name.local.as_str() {
                    "Types" => {
                        if std::mem::replace(&mut seen_types, true) {
                            return Err("duplicate Types".into());
                        }
                        for tok in tokens(c)? {
                            types.push(
                                resolve_token(c, &tok)
                                    .ok_or_else(|| format!("unresolvable type {tok:?}"))?,
                            );
                        }
                    }
                    "Scopes" => {
                        if std::mem::replace(&mut seen_scopes, true) {
                            return Err("duplicate Scopes".into());
                        }
                        scopes = tokens(c)?;
                        match_by = MatchBy::parse(c.attr_local("MatchBy"));
                    }
                    _ => {}
                }
            }
            Ok(Some(Query::Probe {
                types,
                scopes,
                match_by,
            }))
        }
        ACTION_RESOLVE => {
            if !body.is(ns::WSD, "Resolve") {
                return Ok(None);
            }
            let epr = body
                .child(ns::WSA, "EndpointReference")
                .ok_or("Resolve without EndpointReference")?;
            let address = epr
                .child(ns::WSA, "Address")
                .map(|a| a.text_trimmed().to_string())
                .filter(|a| !a.is_empty())
                .ok_or("Resolve without Address")?;
            Ok(Some(Query::Resolve { address }))
        }
        _ => Ok(None),
    }
}

fn norm_scope_rfc3986(s: &str) -> String {
    let s = s.trim();
    let (head, path) = match s.find("://") {
        Some(i) => {
            let rest = &s[i + 3..];
            let end = rest.find(['/', '?']).unwrap_or(rest.len());
            (
                format!(
                    "{}://{}",
                    s[..i].to_ascii_lowercase(),
                    rest[..end].to_ascii_lowercase()
                ),
                rest[end..].to_string(),
            )
        }
        None => (s.to_ascii_lowercase(), String::new()),
    };
    format!("{head}{}", path.trim_end_matches('/'))
}

/// Does `target` satisfy the probe scope `probe` under `rule`?
pub fn scope_matches(rule: &MatchBy, probe: &str, target: &str) -> bool {
    match rule {
        MatchBy::Rfc3986 => {
            let (p, t) = (norm_scope_rfc3986(probe), norm_scope_rfc3986(target));
            if p.split('/').any(|seg| seg == "..") {
                return false;
            }
            t == p || t.strip_prefix(&p).is_some_and(|rest| rest.starts_with('/'))
        }
        MatchBy::Strcmp0 | MatchBy::None => probe == target,
        MatchBy::Uuid | MatchBy::Ldap => probe.eq_ignore_ascii_case(target),
        MatchBy::Unknown(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Core
// ---------------------------------------------------------------------------------------------

/// Where a [`Transmit`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Multicast,
    Unicast(SocketAddr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Hello,
    Bye,
    ProbeMatches,
    ResolveMatches,
}

/// One datagram to send `after` the instruction was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub after: Duration,
    pub dest: Dest,
    pub kind: MsgKind,
    /// First transmission of its message (retransmissions are false).
    pub first: bool,
    pub data: Vec<u8>,
}

/// Why a datagram was not answered (debugging aid and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    TooLarge,
    NotSoap,
    NotForUs,
    NotAQuery,
    BadQuery,
    Duplicate,
    NoMatch,
    RateLimited,
    BadPeer,
}

pub struct DiscoveryCore {
    uuid: Uuid,
    types: Vec<QName>,
    scopes: Vec<String>,
    xaddrs: Vec<String>,
    instance_id: u64,
    metadata_version: u64,
    message_number: u64,
    timings: DiscoveryTimings,
    quirks: DiscoveryQuirks,
    seen: VecDeque<(String, SystemTime)>,
    responses: VecDeque<SystemTime>,
    jitter: Box<dyn Jitter>,
    /// Why the last `on_datagram` produced nothing.
    pub last_ignored: Option<Ignored>,
}

fn first_delay(j: &mut dyn Jitter, max: Duration) -> Duration {
    Duration::from_millis(j.up_to(max.as_millis().min(u64::MAX as u128) as u64))
}

impl DiscoveryCore {
    pub fn new(cfg: &DiscoveryConfig, jitter: Box<dyn Jitter>) -> Self {
        let instance_id = if cfg.instance_id == 0 {
            boot_sequence_id()
        } else {
            cfg.instance_id
        };
        Self {
            uuid: cfg.device_uuid,
            types: if cfg.types.is_empty() {
                default_types()
            } else {
                cfg.types.clone()
            },
            scopes: cfg.scopes.clone(),
            xaddrs: vec![cfg.endpoint.clone()],
            instance_id,
            metadata_version: if cfg.metadata_version == 0 {
                instance_id
            } else {
                cfg.metadata_version
            },
            message_number: 0,
            timings: cfg.timings.clone(),
            quirks: cfg.quirks,
            seen: VecDeque::new(),
            responses: VecDeque::new(),
            jitter,
            last_ignored: None,
        }
    }

    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }
    pub fn message_number(&self) -> u64 {
        self.message_number
    }
    pub fn metadata_version(&self) -> u64 {
        self.metadata_version
    }
    pub fn address(&self) -> String {
        format!("urn:uuid:{}", self.uuid)
    }

    /// The device is announced again after having left: new InstanceId, numbering restarts.
    pub fn restart(&mut self) {
        self.instance_id = boot_sequence_id().max(self.instance_id.saturating_add(1));
        self.message_number = 0;
    }

    /// The metadata changed: clients should refetch.
    pub fn bump_metadata_version(&mut self) {
        self.metadata_version = self.metadata_version.saturating_add(1);
    }

    fn next_number(&mut self) -> u64 {
        self.message_number = self.message_number.saturating_add(1);
        self.message_number
    }

    // ----- builders -----

    fn types_text(&self) -> (String, Vec<(String, String)>) {
        let mut text = Vec::new();
        let mut decls: Vec<(String, String)> = Vec::new();
        for (i, q) in self.types.iter().enumerate() {
            let prefix = if q.ns == ns::WSDP {
                "wsdp".to_string()
            } else if q.ns == ns::PRINT {
                "wprt".to_string()
            } else if q.ns.is_empty() {
                text.push(q.local.clone());
                continue;
            } else {
                format!("t{i}")
            };
            if !decls.iter().any(|(p, _)| *p == prefix) {
                decls.push((prefix.clone(), q.ns.clone()));
            }
            text.push(format!("{prefix}:{}", q.local));
        }
        (text.join(" "), decls)
    }

    fn app_sequence(&self, number: u64) -> Element {
        Element::new(ns::WSD, "AppSequence")
            .with_attr("InstanceId", self.instance_id.to_string())
            .with_attr("MessageNumber", number.to_string())
    }

    fn epr(&self) -> Element {
        EndpointRef::new(self.address()).to_element(QName::new(ns::WSA, "EndpointReference"))
    }

    /// Common children of Hello and the match entries.
    fn target_children(&self, el: &mut Element, with_version: bool) {
        el.children.push(self.epr());
        let (types, _) = self.types_text();
        el.children.push(Element::text_el(ns::WSD, "Types", types));
        if !self.scopes.is_empty() {
            el.children
                .push(Element::text_el(ns::WSD, "Scopes", self.scopes.join(" ")));
        }
        el.children
            .push(Element::text_el(ns::WSD, "XAddrs", self.xaddrs.join(" ")));
        if with_version {
            el.children.push(Element::text_el(
                ns::WSD,
                "MetadataVersion",
                self.metadata_version.to_string(),
            ));
        }
    }

    fn body_for(&self, kind: MsgKind) -> Element {
        match kind {
            MsgKind::Hello => {
                let mut e = Element::new(ns::WSD, "Hello");
                self.target_children(&mut e, true);
                e
            }
            MsgKind::Bye => {
                let mut e = Element::new(ns::WSD, "Bye");
                self.target_children(&mut e, false);
                e
            }
            MsgKind::ProbeMatches => {
                let mut m = Element::new(ns::WSD, "ProbeMatch");
                self.target_children(&mut m, true);
                Element::new(ns::WSD, "ProbeMatches").with_child(m)
            }
            MsgKind::ResolveMatches => {
                let mut m = Element::new(ns::WSD, "ResolveMatch");
                self.target_children(&mut m, true);
                Element::new(ns::WSD, "ResolveMatches").with_child(m)
            }
        }
    }

    fn action_for(kind: MsgKind) -> &'static str {
        match kind {
            MsgKind::Hello => ACTION_HELLO,
            MsgKind::Bye => ACTION_BYE,
            MsgKind::ProbeMatches => ACTION_PROBE_MATCHES,
            MsgKind::ResolveMatches => ACTION_RESOLVE_MATCHES,
        }
    }

    /// The message as a SOAP reply tree.
    fn message(&mut self, kind: MsgKind, relates_to: Option<&str>) -> SoapReply {
        let number = self.next_number();
        let mut reply = SoapReply::new(Self::action_for(kind), self.body_for(kind))
            .relates_to(relates_to)
            .with_header(self.app_sequence(number));
        if matches!(kind, MsgKind::Hello | MsgKind::Bye) {
            reply.to = DISCOVERY_TO.to_string();
        }
        reply
    }

    fn serialize(&self, reply: &SoapReply) -> Vec<u8> {
        let mut root = reply.to_element();
        root.declared = self.types_text().1;
        root.to_document(&PrefixMap::wsd()).into_bytes()
    }

    fn schedule(
        &mut self,
        first: Duration,
        repeat: u32,
        dest: Dest,
        kind: MsgKind,
        data: Vec<u8>,
    ) -> Vec<Transmit> {
        let t = &self.timings;
        let (min, max, upper) = (t.udp_min_delay, t.udp_max_delay, t.udp_upper_delay);
        let mut out = vec![Transmit {
            after: first,
            dest,
            kind,
            first: true,
            data: data.clone(),
        }];
        let mut at = first;
        let mut gap = {
            let lo = min.as_millis() as u64;
            let hi = max.as_millis() as u64;
            Duration::from_millis(lo + self.jitter.up_to(hi.saturating_sub(lo)))
        };
        for _ in 0..repeat {
            at += gap;
            out.push(Transmit {
                after: at,
                dest,
                kind,
                first: false,
                data: data.clone(),
            });
            gap = (gap * 2).min(upper);
        }
        out
    }

    /// Multicast Hello with the spec delay and repeats. Each call is a new message.
    pub fn hello(&mut self) -> Vec<Transmit> {
        let reply = self.message(MsgKind::Hello, None);
        let data = self.serialize(&reply);
        let first = first_delay(self.jitter.as_mut(), self.timings.app_max_delay);
        let repeat = self.timings.multicast_repeat;
        self.schedule(first, repeat, Dest::Multicast, MsgKind::Hello, data)
    }

    /// A single immediate Hello (the periodic lab re-announce).
    pub fn periodic_hello(&mut self) -> Vec<Transmit> {
        let reply = self.message(MsgKind::Hello, None);
        let data = self.serialize(&reply);
        vec![Transmit {
            after: Duration::ZERO,
            dest: Dest::Multicast,
            kind: MsgKind::Hello,
            first: true,
            data,
        }]
    }

    /// Multicast Bye, no initial delay.
    pub fn bye(&mut self) -> Vec<Transmit> {
        let reply = self.message(MsgKind::Bye, None);
        let data = self.serialize(&reply);
        let repeat = self.timings.multicast_repeat;
        self.schedule(Duration::ZERO, repeat, Dest::Multicast, MsgKind::Bye, data)
    }

    // ----- matching -----

    fn target_has_type(&self, q: &QName) -> bool {
        self.types.contains(q)
            || (self.quirks.match_hosted_service_type
                && *q == QName::new(ns::PRINT, "PrinterServiceType"))
    }

    /// Does the Probe select this target?
    pub fn probe_matches(&self, types: &[QName], scopes: &[String], match_by: &MatchBy) -> bool {
        if !types.iter().all(|q| self.target_has_type(q)) {
            return false;
        }
        scopes
            .iter()
            .all(|ps| self.scopes.iter().any(|ts| scope_matches(match_by, ps, ts)))
    }

    /// Does the Resolve name this target?
    pub fn resolve_matches(&self, address: &str) -> bool {
        address.trim().eq_ignore_ascii_case(&self.address())
    }

    fn to_ok(&self, to: Option<&str>) -> bool {
        match to {
            None => true,
            Some(t) => t == DISCOVERY_TO || t.eq_ignore_ascii_case(&self.address()),
        }
    }

    fn prune(&mut self, now: SystemTime) {
        let window = self.timings.dedupe_window;
        while self
            .seen
            .front()
            .is_some_and(|(_, t)| now.duration_since(*t).map(|d| d > window).unwrap_or(false))
        {
            self.seen.pop_front();
        }
        while self.seen.len() > MAX_SEEN {
            self.seen.pop_front();
        }
        while self.responses.front().is_some_and(|t| {
            now.duration_since(*t)
                .map(|d| d >= Duration::from_secs(1))
                .unwrap_or(false)
        }) {
            self.responses.pop_front();
        }
    }

    /// Remember `id`; true when it was already seen inside the window.
    fn is_duplicate(&mut self, id: &str, now: SystemTime) -> bool {
        self.prune(now);
        if self.seen.iter().any(|(s, _)| s == id) {
            return true;
        }
        self.seen.push_back((id.to_string(), now));
        false
    }

    /// Handle one received datagram. Anything that is not a matching Probe/Resolve yields no
    /// transmissions; the reason is left in `last_ignored`.
    pub fn on_datagram(
        &mut self,
        now: SystemTime,
        bytes: &[u8],
        peer: SocketAddr,
    ) -> Vec<Transmit> {
        self.last_ignored = None;
        match self.answer(now, bytes, peer) {
            Ok(t) => t,
            Err(why) => {
                self.last_ignored = Some(why);
                Vec::new()
            }
        }
    }

    fn answer(
        &mut self,
        now: SystemTime,
        bytes: &[u8],
        peer: SocketAddr,
    ) -> Result<Vec<Transmit>, Ignored> {
        if bytes.len() > MAX_DATAGRAM {
            return Err(Ignored::TooLarge);
        }
        let env = SoapEnvelope::parse(bytes, &DATAGRAM_LIMITS).map_err(|_| Ignored::NotSoap)?;
        let action = env.addressing.action.clone().ok_or(Ignored::NotAQuery)?;
        if action != ACTION_PROBE && action != ACTION_RESOLVE {
            return Err(Ignored::NotAQuery);
        }
        let body = env.body_first().ok_or(Ignored::NotAQuery)?;
        let query = parse_query(&action, body)
            .map_err(|_| Ignored::BadQuery)?
            .ok_or(Ignored::NotAQuery)?;
        if !self.to_ok(env.addressing.to.as_deref()) {
            return Err(Ignored::NotForUs);
        }
        let message_id = env.addressing.message_id.clone().ok_or(Ignored::BadQuery)?;
        if peer.port() == 0 || peer.ip().is_multicast() || peer.ip().is_unspecified() {
            return Err(Ignored::BadPeer);
        }
        let (kind, matched) = match &query {
            Query::Probe {
                types,
                scopes,
                match_by,
            } => {
                if matches!(match_by, MatchBy::Unknown(_)) {
                    return Err(Ignored::BadQuery);
                }
                (
                    MsgKind::ProbeMatches,
                    self.probe_matches(types, scopes, match_by),
                )
            }
            Query::Resolve { address } => (MsgKind::ResolveMatches, self.resolve_matches(address)),
        };
        if !matched {
            return Err(Ignored::NoMatch);
        }
        if self.is_duplicate(&message_id, now) {
            return Err(Ignored::Duplicate);
        }
        self.prune(now);
        if self.responses.len() >= MAX_RESPONSES_PER_SECOND {
            return Err(Ignored::RateLimited);
        }
        self.responses.push_back(now);
        let reply = self.message(kind, Some(&message_id));
        let data = self.serialize(&reply);
        let delay = first_delay(self.jitter.as_mut(), self.timings.app_max_delay);
        let repeat = self.timings.unicast_repeat;
        Ok(self.schedule(delay, repeat, Dest::Unicast(peer), kind, data))
    }

    // ----- directed -----

    /// Answer a directed (unicast, HTTP) Probe or Resolve. A non-match is an empty
    /// `ProbeMatches` / `ResolveMatches`.
    pub fn directed(&mut self, req: &SoapRequest) -> Result<SoapReply, PrintFault> {
        let body = req
            .body()
            .ok_or_else(|| PrintFault::malformed("empty body"))?;
        let query = parse_query(&req.action, body)
            .map_err(PrintFault::malformed)?
            .ok_or_else(|| PrintFault::action_not_supported(&req.action))?;
        if !self.to_ok(req.addressing().to.as_deref()) {
            return Err(PrintFault::header_invalid(
                "To",
                "not addressed to this device",
            ));
        }
        let (kind, matched) = match &query {
            Query::Probe {
                types,
                scopes,
                match_by,
            } => {
                if let MatchBy::Unknown(uri) = match_by {
                    return Err(PrintFault::custom(
                        FaultCode::Sender,
                        Some(QName::new(ns::WSD, "MatchingRuleNotSupported")),
                        format!("matching rule {uri} is not supported"),
                    ));
                }
                (
                    MsgKind::ProbeMatches,
                    self.probe_matches(types, scopes, match_by),
                )
            }
            Query::Resolve { address } => (MsgKind::ResolveMatches, self.resolve_matches(address)),
        };
        let number = self.next_number();
        let body = if matched {
            self.body_for(kind)
        } else {
            Element::new(
                ns::WSD,
                if kind == MsgKind::ProbeMatches {
                    "ProbeMatches"
                } else {
                    "ResolveMatches"
                },
            )
        };
        Ok(req
            .reply(Self::action_for(kind), body)
            .with_header(self.app_sequence(number)))
    }
}

/// Shared handle: the event loop and the directed HTTP endpoint use the same sequence.
pub struct DiscoveryService {
    core: Mutex<DiscoveryCore>,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    pub timings: DiscoveryTimings,
}

impl DiscoveryService {
    pub fn new(cfg: &DiscoveryConfig) -> Self {
        Self::with_parts(cfg, Box::new(RandomJitter), Arc::new(SystemTime::now))
    }

    /// `clock` supplies "now" (injectable for tests).
    pub fn with_parts(
        cfg: &DiscoveryConfig,
        jitter: Box<dyn Jitter>,
        clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    ) -> Self {
        Self {
            core: Mutex::new(DiscoveryCore::new(cfg, jitter)),
            clock,
            timings: cfg.timings.clone(),
        }
    }

    fn core(&self) -> MutexGuard<'_, DiscoveryCore> {
        match self.core.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn hello(&self) -> Vec<Transmit> {
        self.core().hello()
    }
    pub fn periodic_hello(&self) -> Vec<Transmit> {
        self.core().periodic_hello()
    }
    pub fn bye(&self) -> Vec<Transmit> {
        self.core().bye()
    }
    pub fn restart(&self) {
        self.core().restart()
    }
    pub fn bump_metadata_version(&self) {
        self.core().bump_metadata_version()
    }
    pub fn instance_id(&self) -> u64 {
        self.core().instance_id()
    }
    pub fn message_number(&self) -> u64 {
        self.core().message_number()
    }
    pub fn on_datagram(&self, bytes: &[u8], peer: SocketAddr) -> Vec<Transmit> {
        let now = (self.clock)();
        self.core().on_datagram(now, bytes, peer)
    }

    /// Reply to a directed Probe/Resolve that arrived over HTTP.
    pub fn handle_directed(&self, req: &SoapRequest) -> SoapReply {
        match self.core().directed(req) {
            Ok(r) => r,
            Err(f) => req.fault_reply(&f),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------------------------

/// The multicast endpoint the event loop talks to.
pub trait DiscoverySocket: Send {
    /// Receive one datagram.
    fn recv(&mut self) -> impl Future<Output = io::Result<(Vec<u8>, SocketAddr)>> + Send;
    fn send_to(
        &mut self,
        data: &[u8],
        dest: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send;
    /// Join the WS-Discovery group (idempotent, best effort).
    fn join_group(&mut self) -> io::Result<()>;
    fn leave_group(&mut self) -> io::Result<()>;
    /// False when only sending is possible (Hello-only mode).
    fn can_receive(&self) -> bool;
}

/// The real UDP implementation.
pub struct UdpDiscoverySocket {
    send: UdpSocket,
    recv: Option<UdpSocket>,
    iface: Ipv4Addr,
    joined: bool,
}

impl UdpDiscoverySocket {
    /// Bind the ephemeral send socket and, when `listen`, the `:3702` receive socket. The
    /// group is not joined until [`DiscoverySocket::join_group`].
    pub fn bind(iface: Ipv4Addr, listen: bool) -> Result<Self> {
        let send = bind_send_socket(iface)?;
        let recv = if listen {
            bind_recv_socket().ok()
        } else {
            None
        };
        Ok(Self {
            send,
            recv,
            iface,
            joined: false,
        })
    }
}

impl DiscoverySocket for UdpDiscoverySocket {
    async fn recv(&mut self) -> io::Result<(Vec<u8>, SocketAddr)> {
        match &self.recv {
            Some(r) => {
                let mut buf = vec![0u8; 64 * 1024];
                let (n, peer) = r.recv_from(&mut buf).await?;
                buf.truncate(n);
                Ok((buf, peer))
            }
            None => std::future::pending().await,
        }
    }

    async fn send_to(&mut self, data: &[u8], dest: SocketAddr) -> io::Result<usize> {
        self.send.send_to(data, dest).await
    }

    fn join_group(&mut self) -> io::Result<()> {
        if self.joined {
            return Ok(());
        }
        let Some(r) = &self.recv else { return Ok(()) };
        r.join_multicast_v4(MULTICAST, self.iface)?;
        if !self.iface.is_unspecified() {
            let _ = r.join_multicast_v4(MULTICAST, Ipv4Addr::UNSPECIFIED);
        }
        self.joined = true;
        Ok(())
    }

    fn leave_group(&mut self) -> io::Result<()> {
        if !self.joined {
            return Ok(());
        }
        self.joined = false;
        let Some(r) = &self.recv else { return Ok(()) };
        let _ = r.leave_multicast_v4(MULTICAST, Ipv4Addr::UNSPECIFIED);
        r.leave_multicast_v4(MULTICAST, self.iface)
    }

    fn can_receive(&self) -> bool {
        self.recv.is_some()
    }
}

fn bind_send_socket(iface: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&socket2::SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        0,
    )))?;
    if !iface.is_unspecified() {
        let _ = socket.set_multicast_if_v4(&iface);
    }
    // Loopback so a same-host listener can hear our multicast.
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(4)?;
    socket.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_sock).context("tokio send socket")
}

fn bind_recv_socket() -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket
        .bind(&socket2::SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            PORT,
        )))
        .context("bind :3702 for Probe recv")?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_sock).context("tokio recv socket")
}

// ---------------------------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------------------------

fn dest_addr(d: Dest) -> SocketAddr {
    match d {
        Dest::Multicast => SocketAddr::from((MULTICAST, PORT)),
        Dest::Unicast(a) => a,
    }
}

async fn send_one<S: DiscoverySocket>(socket: &mut S, stats: &DiscoveryStats, t: &Transmit) {
    match socket.send_to(&t.data, dest_addr(t.dest)).await {
        Ok(n) => {
            debug!(bytes = n, kind = ?t.kind, first = t.first, "sent WS-Discovery message");
            match t.kind {
                MsgKind::Hello => {
                    stats.hellos_sent.fetch_add(1, Ordering::Relaxed);
                }
                MsgKind::ProbeMatches | MsgKind::ResolveMatches if t.first => {
                    stats.probes_answered.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        Err(e) => warn!(error = %e, kind = ?t.kind, "discovery send failed"),
    }
}

/// Run the target service on `socket` until `stop` fires; then multicast Bye (with its
/// repeats) and leave the group.
pub async fn run_service<S: DiscoverySocket>(
    service: Arc<DiscoveryService>,
    mut socket: S,
    stats: Arc<DiscoveryStats>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    if let Err(e) = socket.join_group() {
        warn!(error = %e, "multicast join failed; continuing Hello-only");
    }
    let interval = service.timings.hello_interval;
    let mut queue: Vec<(Instant, Transmit)> = Vec::new();
    let start = Instant::now();
    queue.extend(service.hello().into_iter().map(|t| (start + t.after, t)));
    let mut next_hello = interval.map(|i| start + i);

    loop {
        let now = Instant::now();
        let (due, rest): (Vec<_>, Vec<_>) = queue.into_iter().partition(|(at, _)| *at <= now);
        queue = rest;
        for (_, t) in &due {
            send_one(&mut socket, &stats, t).await;
        }
        if let (Some(at), Some(i)) = (next_hello, interval) {
            if at <= now {
                queue.extend(
                    service
                        .periodic_hello()
                        .into_iter()
                        .map(|t| (now + t.after, t)),
                );
                next_hello = Some(now + i);
            }
        }
        let far = now + Duration::from_secs(3600);
        let wake = queue
            .iter()
            .map(|(at, _)| *at)
            .chain(next_hello)
            .min()
            .unwrap_or(far);
        let can_recv = socket.can_receive();
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep_until(wake) => {}
            r = async {
                if can_recv {
                    socket.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match r {
                    Ok((bytes, peer)) => {
                        let out = service.on_datagram(&bytes, peer);
                        if !out.is_empty() {
                            info!(%peer, bytes = bytes.len(), "answering WS-Discovery request");
                        }
                        let at = Instant::now();
                        queue.extend(out.into_iter().map(|t| (at + t.after, t)));
                    }
                    Err(e) => {
                        warn!(error = %e, "discovery recv error");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    let mut elapsed = Duration::ZERO;
    for t in service.bye() {
        if t.after > elapsed {
            tokio::time::sleep(t.after - elapsed).await;
            elapsed = t.after;
        }
        send_one(&mut socket, &stats, &t).await;
    }
    info!("sent WS-Discovery Bye");
    if let Err(e) = socket.leave_group() {
        debug!(error = %e, "multicast leave failed");
    }
    Ok(())
}

/// Run WS-Discovery on the real multicast sockets until `stop` is signaled, then Bye.
pub async fn run(cfg: DiscoveryConfig, stop: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
    let service = Arc::new(DiscoveryService::new(&cfg));
    run_with_service(cfg, service, stop).await
}

/// Like [`run`] with a service the caller also hands to the directed HTTP endpoint.
pub async fn run_with_service(
    cfg: DiscoveryConfig,
    service: Arc<DiscoveryService>,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let socket = UdpDiscoverySocket::bind(cfg.multicast_iface, cfg.listen_probes)?;
    info!(
        endpoint = %cfg.endpoint,
        uuid = %cfg.device_uuid,
        iface = %cfg.multicast_iface,
        instance_id = service.instance_id(),
        recv_3702 = socket.can_receive(),
        "WS-Discovery announcer up (Hello on multicast; Probe receive best-effort)"
    );
    if !socket.can_receive() {
        info!("no :3702 recv socket (dasHost likely exclusive); Hello-only mode");
    }
    run_service(service, socket, Arc::clone(&cfg.stats), stop).await
}

/// Is `ip` something a discovery reply should be sent to?
pub fn is_unicast_peer(ip: IpAddr) -> bool {
    !(ip.is_multicast() || ip.is_unspecified())
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::xml::parse_str;

    const UUID: &str = "00000000-0000-4000-8000-0000000000f3";

    fn cfg() -> DiscoveryConfig {
        let mut c = DiscoveryConfig::new(
            "http://192.0.2.10:3921/WebServices/Device",
            Uuid::parse_str(UUID).expect("uuid"),
        );
        c.instance_id = 1_700_000_000;
        c.metadata_version = 1_700_000_001;
        c.timings = DiscoveryTimings::immediate();
        c
    }

    fn core_with(c: &DiscoveryConfig) -> DiscoveryCore {
        DiscoveryCore::new(c, Box::new(FixedJitter(0)))
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:40000".parse().expect("addr")
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn probe_xml(id: &str, to: &str, body_inner: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="{}" xmlns:a="{}" xmlns:d="{}" xmlns:dp="{}" xmlns:pr="{}"><s:Header><a:To>{to}</a:To><a:Action>{ACTION_PROBE}</a:Action><a:MessageID>urn:uuid:{id}</a:MessageID></s:Header><s:Body><d:Probe>{body_inner}</d:Probe></s:Body></s:Envelope>"#,
            ns::SOAP12,
            ns::WSA,
            ns::WSD,
            ns::WSDP,
            ns::PRINT
        )
    }

    fn resolve_xml(id: &str, addr: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="{}" xmlns:a="{}" xmlns:d="{}"><s:Header><a:To>{DISCOVERY_TO}</a:To><a:Action>{ACTION_RESOLVE}</a:Action><a:MessageID>urn:uuid:{id}</a:MessageID></s:Header><s:Body><d:Resolve><a:EndpointReference><a:Address>{addr}</a:Address></a:EndpointReference></d:Resolve></s:Body></s:Envelope>"#,
            ns::SOAP12,
            ns::WSA,
            ns::WSD
        )
    }

    fn probe(core: &mut DiscoveryCore, id: &str, inner: &str) -> Vec<Transmit> {
        core.on_datagram(now(), probe_xml(id, DISCOVERY_TO, inner).as_bytes(), peer())
    }

    fn parse(t: &Transmit) -> Element {
        parse_str(
            std::str::from_utf8(&t.data).expect("utf8"),
            &XmlLimits::SMALL,
        )
        .expect("our output parses")
    }

    fn header_of(root: &Element, local: &str) -> Element {
        root.child(ns::SOAP12, "Header")
            .expect("header")
            .children
            .iter()
            .find(|c| c.name.local == local)
            .expect("block")
            .clone()
    }

    fn body_of(root: &Element) -> Element {
        root.child(ns::SOAP12, "Body").expect("body").children[0].clone()
    }

    fn types_of(el: &Element) -> Vec<QName> {
        el.text
            .split_whitespace()
            .map(|t| resolve_token(el, t).expect("type resolves"))
            .collect()
    }

    fn number_of(root: &Element) -> u64 {
        header_of(root, "AppSequence")
            .attr_local("MessageNumber")
            .expect("n")
            .parse()
            .expect("num")
    }

    #[test]
    fn hello_content_and_sequence() {
        let mut core = core_with(&cfg());
        let t = core.hello();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].dest, Dest::Multicast);
        let root = parse(&t[0]);
        assert_eq!(header_of(&root, "To").text, DISCOVERY_TO);
        assert_eq!(header_of(&root, "Action").text, ACTION_HELLO);
        let seq = header_of(&root, "AppSequence");
        assert_eq!(seq.attr_local("InstanceId"), Some("1700000000"));
        assert_eq!(seq.attr_local("MessageNumber"), Some("1"));
        let hello = body_of(&root);
        assert_eq!(hello.name, QName::new(ns::WSD, "Hello"));
        let kids: Vec<&str> = hello
            .children
            .iter()
            .map(|c| c.name.local.as_str())
            .collect();
        assert_eq!(
            kids,
            ["EndpointReference", "Types", "XAddrs", "MetadataVersion"]
        );
        assert_eq!(
            hello.child(ns::WSD, "MetadataVersion").expect("m").text,
            "1700000001"
        );
        assert_eq!(
            hello.child(ns::WSD, "XAddrs").expect("x").text,
            "http://192.0.2.10:3921/WebServices/Device"
        );
        assert_eq!(
            types_of(hello.child(ns::WSD, "Types").expect("t")),
            default_types()
        );
        assert_eq!(
            hello
                .child(ns::WSA, "EndpointReference")
                .expect("e")
                .child(ns::WSA, "Address")
                .expect("a")
                .text,
            format!("urn:uuid:{UUID}")
        );
    }

    #[test]
    fn bye_omits_metadata_version_and_numbers_continue() {
        let mut core = core_with(&cfg());
        core.hello();
        core.periodic_hello();
        let bye = core.bye();
        let root = parse(&bye[0]);
        assert_eq!(header_of(&root, "Action").text, ACTION_BYE);
        assert_eq!(number_of(&root), 3);
        let b = body_of(&root);
        assert!(b.child(ns::WSD, "MetadataVersion").is_none());
        assert!(b.child(ns::WSD, "XAddrs").is_some());
    }

    #[test]
    fn numbers_are_shared_across_message_kinds_and_restart_bumps_instance() {
        let mut core = core_with(&cfg());
        core.hello(); // 1
        let m = probe(&mut core, "a1", ""); // 2
        assert_eq!(number_of(&parse(&m[0])), 2);
        let r = core.on_datagram(
            now(),
            resolve_xml("a2", &format!("urn:uuid:{UUID}")).as_bytes(),
            peer(),
        ); // 3
        assert_eq!(number_of(&parse(&r[0])), 3);
        assert_eq!(core.message_number(), 3);
        let before = core.instance_id();
        core.restart();
        assert!(core.instance_id() > before);
        assert_eq!(core.message_number(), 0);
        let h = core.hello();
        let root = parse(&h[0]);
        assert_eq!(number_of(&root), 1);
        assert_eq!(
            header_of(&root, "AppSequence").attr_local("InstanceId"),
            Some(core.instance_id().to_string().as_str())
        );
    }

    #[test]
    fn retransmissions_repeat_the_same_message_with_spec_schedule() {
        let mut c = cfg();
        c.timings = DiscoveryTimings::default();
        let mut core = DiscoveryCore::new(&c, Box::new(FixedJitter(10_000)));
        let t = core.hello();
        assert_eq!(t.len(), 3); // 1 + MULTICAST_UDP_REPEAT
        assert_eq!(t[0].after, Duration::from_millis(500)); // jitter capped by APP_MAX_DELAY
        assert!(
            t.iter().all(|x| x.data == t[0].data),
            "same MessageID and sequence number"
        );
        assert_eq!(t[1].after - t[0].after, Duration::from_millis(250));
        assert_eq!(t[2].after - t[1].after, Duration::from_millis(450)); // doubled, capped at upper
        assert_eq!(core.message_number(), 1);
        // Bye is not delayed
        assert_eq!(core.bye()[0].after, Duration::ZERO);
        // unicast answers repeat once
        let m = probe(&mut core, "b1", "");
        assert_eq!(m.len(), 2);
        assert!(m[0].first && !m[1].first);
        // zero jitter: the first hello is immediate and the first gap is the minimum
        let mut z = DiscoveryCore::new(&c, Box::new(FixedJitter(0)));
        let t = z.hello();
        assert_eq!(t[0].after, Duration::ZERO);
        assert_eq!(t[1].after, Duration::from_millis(50));
    }

    #[test]
    fn probe_matrix() {
        let mut core = core_with(&cfg());
        let with = |t: &str| format!("<d:Types>{t}</d:Types>");
        let cases: Vec<(String, bool)> = vec![
            ("".into(), true),
            ("<d:Types></d:Types>".into(), true),
            (with("dp:Device"), true),
            (with("pr:PrintDeviceType"), true),
            (with("dp:Device pr:PrintDeviceType"), true),
            (with("pr:PrintDeviceType dp:Device"), true),
            (with("pr:PrinterServiceType"), false),
            (with("dp:Device pr:PrintDeviceType pr:Other"), false),
            (with("dp:Printer"), false),
            (with("x:Device"), false), // unbound prefix: malformed, ignored
            (r#"<d:Scopes>http://example.com/x</d:Scopes>"#.into(), false), // we have no scopes
        ];
        for (i, (inner, expect)) in cases.iter().enumerate() {
            let out = probe(&mut core, &format!("p{i}"), inner);
            assert_eq!(!out.is_empty(), *expect, "case {i}: {inner}");
        }
    }

    #[test]
    fn different_namespace_with_same_local_name_does_not_match() {
        let mut core = core_with(&cfg());
        let out = probe(
            &mut core,
            "q1",
            r#"<d:Types xmlns:z="urn:other">z:Device z:PrintDeviceType</d:Types>"#,
        );
        assert!(out.is_empty());
        assert_eq!(core.last_ignored, Some(Ignored::NoMatch));
    }

    #[test]
    fn hosted_service_type_probe_is_an_opt_in_quirk() {
        let mut c = cfg();
        c.quirks.match_hosted_service_type = true;
        let mut core = core_with(&c);
        let out = probe(&mut core, "q2", "<d:Types>pr:PrinterServiceType</d:Types>");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn scoped_target_and_match_rules() {
        let mut c = cfg();
        c.scopes = vec![
            "http://example.com/Printers/Floor2".into(),
            "urn:other".into(),
        ];
        let mut core = core_with(&c);
        let scoped = |by: &str, s: &str| format!(r#"<d:Scopes MatchBy="{by}">{s}</d:Scopes>"#);
        let mut n = 0;
        let mut ask = |core: &mut DiscoveryCore, inner: String| {
            n += 1;
            !probe(core, &format!("s{n}"), &inner).is_empty()
        };
        assert!(ask(
            &mut core,
            scoped(MATCH_BY_RFC3986, "http://example.com/Printers")
        ));
        assert!(ask(
            &mut core,
            scoped(MATCH_BY_RFC3986, "HTTP://EXAMPLE.com/Printers/Floor2/")
        ));
        assert!(
            ask(
                &mut core,
                "<d:Scopes>http://example.com/Printers urn:other</d:Scopes>".into()
            ),
            "default rule is rfc3986"
        );
        assert!(!ask(
            &mut core,
            scoped(MATCH_BY_RFC3986, "http://example.com/Print")
        )); // not a segment prefix
        assert!(!ask(
            &mut core,
            scoped(MATCH_BY_RFC3986, "http://example.com/Printers/../x")
        ));
        assert!(!ask(
            &mut core,
            scoped(MATCH_BY_RFC3986, "http://example.com/Printers urn:missing")
        ));
        assert!(ask(&mut core, scoped(MATCH_BY_STRCMP0, "urn:other")));
        assert!(!ask(&mut core, scoped(MATCH_BY_STRCMP0, "URN:OTHER")));
        assert!(ask(&mut core, scoped(MATCH_BY_UUID, "URN:OTHER")));
        assert!(
            !ask(&mut core, scoped("urn:made-up-rule", "urn:other")),
            "unknown rule is ignored over UDP"
        );
        assert_eq!(core.last_ignored, Some(Ignored::BadQuery));
        // probe with types and scopes combined
        assert!(ask(
            &mut core,
            format!(
                "<d:Types>dp:Device</d:Types>{}",
                scoped(MATCH_BY_RFC3986, "urn:other")
            )
        ));
        assert!(!ask(
            &mut core,
            format!(
                "<d:Types>pr:Nope</d:Types>{}",
                scoped(MATCH_BY_RFC3986, "urn:other")
            )
        ));
    }

    #[test]
    fn resolve_matrix() {
        let mut core = core_with(&cfg());
        let mut ask = |id: &str, addr: &str| {
            !core
                .on_datagram(now(), resolve_xml(id, addr).as_bytes(), peer())
                .is_empty()
        };
        assert!(ask("r1", &format!("urn:uuid:{UUID}")));
        assert!(ask(
            "r2",
            &format!("urn:uuid:{}", UUID.to_ascii_uppercase())
        ));
        assert!(!ask("r3", "urn:uuid:11111111-1111-4111-8111-111111111111"));
        assert!(!ask("r4", &format!("urn:uuid:{UUID}0")));
        assert!(!ask("r5", &format!("xurn:uuid:{UUID}")));
        assert!(!ask("r6", ""));
        // the old matcher accepted any text that merely contained the uuid
        assert!(!ask("r7", &format!("http://evil/{UUID}")));
    }

    #[test]
    fn resolve_response_has_matches_and_relates_to() {
        let mut core = core_with(&cfg());
        let out = core.on_datagram(
            now(),
            resolve_xml("r9", &format!("urn:uuid:{UUID}")).as_bytes(),
            peer(),
        );
        let root = parse(&out[0]);
        assert_eq!(out[0].dest, Dest::Unicast(peer()));
        assert_eq!(header_of(&root, "Action").text, ACTION_RESOLVE_MATCHES);
        assert_eq!(header_of(&root, "RelatesTo").text, "urn:uuid:r9");
        let body = body_of(&root);
        assert_eq!(body.name.local, "ResolveMatches");
        let m = &body.children[0];
        assert_eq!(m.name.local, "ResolveMatch");
        assert!(m.child(ns::WSD, "MetadataVersion").is_some());
    }

    #[test]
    fn probe_response_shape() {
        let mut core = core_with(&cfg());
        let out = probe(&mut core, "pr1", "");
        let root = parse(&out[0]);
        assert_eq!(header_of(&root, "Action").text, ACTION_PROBE_MATCHES);
        let m = &body_of(&root).children[0];
        let kids: Vec<&str> = m.children.iter().map(|c| c.name.local.as_str()).collect();
        assert_eq!(
            kids,
            ["EndpointReference", "Types", "XAddrs", "MetadataVersion"]
        );
        assert_eq!(
            types_of(m.child(ns::WSD, "Types").expect("t")),
            default_types()
        );
    }

    #[test]
    fn scopes_are_advertised_when_configured() {
        let mut c = cfg();
        c.scopes = vec!["urn:s1".into(), "urn:s2".into()];
        let mut core = core_with(&c);
        let h = core.hello();
        let hello = body_of(&parse(&h[0]));
        assert_eq!(
            hello.child(ns::WSD, "Scopes").expect("s").text,
            "urn:s1 urn:s2"
        );
        let kids: Vec<&str> = hello
            .children
            .iter()
            .map(|c| c.name.local.as_str())
            .collect();
        assert_eq!(
            kids,
            [
                "EndpointReference",
                "Types",
                "Scopes",
                "XAddrs",
                "MetadataVersion"
            ]
        );
    }

    #[test]
    fn duplicates_are_suppressed_until_the_window_passes() {
        let mut core = core_with(&cfg());
        let xml = probe_xml("d1", DISCOVERY_TO, "");
        assert_eq!(core.on_datagram(now(), xml.as_bytes(), peer()).len(), 1);
        assert!(core.on_datagram(now(), xml.as_bytes(), peer()).is_empty());
        assert_eq!(core.last_ignored, Some(Ignored::Duplicate));
        // a non-matching probe does not poison the cache
        let other = probe_xml("d2", DISCOVERY_TO, "<d:Types>pr:Nope</d:Types>");
        assert!(core.on_datagram(now(), other.as_bytes(), peer()).is_empty());
        assert_eq!(core.last_ignored, Some(Ignored::NoMatch));
        let later = now() + Duration::from_secs(31);
        assert_eq!(core.on_datagram(later, xml.as_bytes(), peer()).len(), 1);
    }

    #[test]
    fn hostile_input_is_ignored() {
        let mut core = core_with(&cfg());
        let deep = format!("{}{}", "<a>".repeat(200), "</a>".repeat(200));
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("garbage", b"\x00\xff\xfe not xml".to_vec()),
            ("text", b"Probe discovery/Probe wsdp:Device".to_vec()),
            (
                "doctype",
                format!(
                    r#"<!DOCTYPE x [<!ENTITY a "b">]>{}"#,
                    probe_xml("h1", DISCOVERY_TO, "")
                )
                .into_bytes(),
            ),
            (
                "soap11",
                probe_xml("h2", DISCOVERY_TO, "")
                    .replace(ns::SOAP12, ns::SOAP11)
                    .into_bytes(),
            ),
            ("deep", deep.into_bytes()),
            ("huge", vec![b'<'; 70_000]),
            (
                "truncated",
                probe_xml("h3", DISCOVERY_TO, "").as_bytes()[..120].to_vec(),
            ),
            (
                "match message",
                probe_xml("h4", DISCOVERY_TO, "")
                    .replace(ACTION_PROBE, ACTION_PROBE_MATCHES)
                    .into_bytes(),
            ),
            (
                "hello message",
                probe_xml("h5", DISCOVERY_TO, "")
                    .replace(ACTION_PROBE, ACTION_HELLO)
                    .into_bytes(),
            ),
            (
                "other ns probe",
                probe_xml("h6", DISCOVERY_TO, "")
                    .replace(
                        ns::WSD,
                        "http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01",
                    )
                    .into_bytes(),
            ),
            (
                "wrong to",
                probe_xml("h7", "urn:uuid:11111111-1111-4111-8111-111111111111", "").into_bytes(),
            ),
            (
                "too many types",
                probe_xml(
                    "h8",
                    DISCOVERY_TO,
                    &format!("<d:Types>{}</d:Types>", "dp:Device ".repeat(40)),
                )
                .into_bytes(),
            ),
            (
                "duplicate types",
                probe_xml(
                    "h9",
                    DISCOVERY_TO,
                    "<d:Types>dp:Device</d:Types><d:Types>dp:Device</d:Types>",
                )
                .into_bytes(),
            ),
            (
                "no message id",
                probe_xml("h10", DISCOVERY_TO, "")
                    .replace("MessageID", "Other")
                    .into_bytes(),
            ),
        ];
        for (name, bytes) in cases {
            assert!(
                core.on_datagram(now(), &bytes, peer()).is_empty(),
                "{name} must be ignored"
            );
        }
        // bad peers
        let ok = probe_xml("g1", DISCOVERY_TO, "");
        for bad in ["0.0.0.0:3702", "239.255.255.250:3702", "127.0.0.1:0"] {
            let p: SocketAddr = bad.parse().expect("addr");
            assert!(
                core.on_datagram(now(), ok.as_bytes(), p).is_empty(),
                "{bad}"
            );
            assert_eq!(core.last_ignored, Some(Ignored::BadPeer));
        }
        // and the core still works afterwards
        assert_eq!(probe(&mut core, "after", "").len(), 1);
    }

    #[test]
    fn reply_to_in_the_probe_does_not_redirect_the_answer() {
        let mut core = core_with(&cfg());
        let xml = probe_xml("rt1", DISCOVERY_TO, "").replace(
            "</s:Header>",
            "<a:ReplyTo><a:Address>http://8.8.8.8/x</a:Address></a:ReplyTo></s:Header>",
        );
        let out = core.on_datagram(now(), xml.as_bytes(), peer());
        assert_eq!(out[0].dest, Dest::Unicast(peer()));
    }

    #[test]
    fn response_rate_is_bounded() {
        let mut core = core_with(&cfg());
        let mut answered = 0;
        for i in 0..200 {
            answered += (!probe(&mut core, &format!("rate{i}"), "").is_empty()) as usize;
        }
        assert_eq!(answered, MAX_RESPONSES_PER_SECOND);
        // a second later it answers again
        let later = now() + Duration::from_secs(2);
        let xml = probe_xml("rate-next", DISCOVERY_TO, "");
        assert_eq!(core.on_datagram(later, xml.as_bytes(), peer()).len(), 1);
    }

    fn directed_request(action: &str, body: &str, to: &str) -> SoapRequest {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{}" xmlns:a="{}" xmlns:d="{}" xmlns:dp="{}"><s:Header><a:To>{to}</a:To><a:Action>{action}</a:Action><a:MessageID>urn:uuid:dd</a:MessageID></s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
            ns::SOAP12,
            ns::WSA,
            ns::WSD,
            ns::WSDP
        );
        SoapRequest::parse(Some("application/soap+xml"), None, xml.as_bytes()).expect("parse")
    }

    #[test]
    fn directed_probe_and_resolve() {
        let svc = DiscoveryService::with_parts(&cfg(), Box::new(FixedJitter(0)), Arc::new(now));
        let to = format!("urn:uuid:{UUID}");
        let r = svc.handle_directed(&directed_request(
            ACTION_PROBE,
            "<d:Probe><d:Types>dp:Device</d:Types></d:Probe>",
            &to,
        ));
        assert_eq!(r.action, ACTION_PROBE_MATCHES);
        assert_eq!(r.relates_to.as_deref(), Some("urn:uuid:dd"));
        let body = r.body.clone().expect("b");
        assert_eq!(body.children.len(), 1);
        assert!(r.headers.iter().any(|h| h.name.local == "AppSequence"));
        // no match -> empty ProbeMatches
        let r = svc.handle_directed(&directed_request(
            ACTION_PROBE,
            "<d:Probe><d:Types>dp:Nope</d:Types></d:Probe>",
            &to,
        ));
        assert!(r.body.expect("b").children.is_empty());
        // resolve
        let rb = format!("<d:Resolve><a:EndpointReference><a:Address>{to}</a:Address></a:EndpointReference></d:Resolve>");
        let r = svc.handle_directed(&directed_request(ACTION_RESOLVE, &rb, &to));
        assert_eq!(r.action, ACTION_RESOLVE_MATCHES);
        assert_eq!(r.body.expect("b").children.len(), 1);
        // unknown rule -> fault
        let r = svc.handle_directed(&directed_request(
            ACTION_PROBE,
            r#"<d:Probe><d:Scopes MatchBy="urn:zzz">urn:x</d:Scopes></d:Probe>"#,
            &to,
        ));
        assert_eq!(r.status, 400);
        assert_eq!(r.body.expect("b").name.local, "Fault");
        // not addressed to us
        let r = svc.handle_directed(&directed_request(
            ACTION_PROBE,
            "<d:Probe/>",
            "urn:uuid:11111111-1111-4111-8111-111111111111",
        ));
        assert_eq!(r.body.expect("b").name.local, "Fault");
        // sequence numbers are shared with the multicast side
        assert!(svc.message_number() >= 3);
    }

    // ----- fake socket -----

    /// One datagram sent: destination and bytes.
    type Datagram = (SocketAddr, Vec<u8>);

    struct FakeSocket {
        rx: tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
        sent: Arc<Mutex<Vec<Datagram>>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        receive: bool,
    }

    impl DiscoverySocket for FakeSocket {
        async fn recv(&mut self) -> io::Result<(Vec<u8>, SocketAddr)> {
            match self.rx.recv().await {
                Some(x) => Ok(x),
                None => std::future::pending().await,
            }
        }
        async fn send_to(&mut self, data: &[u8], dest: SocketAddr) -> io::Result<usize> {
            self.sent.lock().expect("lock").push((dest, data.to_vec()));
            Ok(data.len())
        }
        fn join_group(&mut self) -> io::Result<()> {
            self.events.lock().expect("lock").push("join");
            Ok(())
        }
        fn leave_group(&mut self) -> io::Result<()> {
            self.events.lock().expect("lock").push("leave");
            Ok(())
        }
        fn can_receive(&self) -> bool {
            self.receive
        }
    }

    type Sent = Arc<Mutex<Vec<(SocketAddr, Vec<u8>)>>>;

    async fn wait_for(sent: &Sent, n: usize) {
        for _ in 0..200 {
            if sent.lock().expect("lock").len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} datagrams, got {}",
            sent.lock().expect("lock").len()
        );
    }

    fn action_of(data: &[u8]) -> String {
        let root =
            parse_str(std::str::from_utf8(data).expect("utf8"), &XmlLimits::SMALL).expect("xml");
        header_of(&root, "Action").text
    }

    #[tokio::test]
    async fn event_loop_on_a_fake_socket() {
        let c = cfg();
        let stats = Arc::new(DiscoveryStats::default());
        let service = Arc::new(DiscoveryService::new(&c));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sent: Sent = Arc::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let socket = FakeSocket {
            rx,
            sent: sent.clone(),
            events: events.clone(),
            receive: true,
        };
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_service(service, socket, stats.clone(), stop_rx));

        wait_for(&sent, 1).await;
        assert_eq!(
            sent.lock().expect("lock")[0].0,
            SocketAddr::from((MULTICAST, PORT))
        );
        assert_eq!(action_of(&sent.lock().expect("lock")[0].1), ACTION_HELLO);

        tx.send((
            probe_xml("loop1", DISCOVERY_TO, "<d:Types>dp:Device</d:Types>").into_bytes(),
            peer(),
        ))
        .expect("send");
        tx.send((
            probe_xml("loop2", DISCOVERY_TO, "<d:Types>dp:Nope</d:Types>").into_bytes(),
            peer(),
        ))
        .expect("send");
        tx.send((b"junk".to_vec(), peer())).expect("send");
        wait_for(&sent, 2).await;

        stop_tx.send(()).expect("stop");
        task.await.expect("join").expect("run ok");
        let sent = sent.lock().expect("lock").clone();
        let actions: Vec<String> = sent.iter().map(|(_, d)| action_of(d)).collect();
        assert_eq!(actions, [ACTION_HELLO, ACTION_PROBE_MATCHES, ACTION_BYE]);
        assert_eq!(sent[1].0, peer());
        assert_eq!(*events.lock().expect("lock"), ["join", "leave"]);
        assert_eq!(stats.snapshot(), (1, 1));
    }

    #[tokio::test]
    async fn event_loop_hello_only_mode_and_periodic_hello() {
        let mut c = cfg();
        c.timings.hello_interval = Some(Duration::from_millis(30));
        let service = Arc::new(DiscoveryService::new(&c));
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sent: Sent = Arc::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let socket = FakeSocket {
            rx,
            sent: sent.clone(),
            events,
            receive: false,
        };
        let stats = Arc::new(DiscoveryStats::default());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_service(service, socket, stats.clone(), stop_rx));
        wait_for(&sent, 3).await;
        stop_tx.send(()).expect("stop");
        task.await.expect("join").expect("ok");
        let sent = sent.lock().expect("lock").clone();
        let numbers: Vec<u64> = sent
            .iter()
            .map(|(_, d)| {
                number_of(
                    &parse_str(std::str::from_utf8(d).expect("u"), &XmlLimits::SMALL).expect("x"),
                )
            })
            .collect();
        // a queued periodic Hello that never left may leave a gap before the Bye
        assert!(numbers.windows(2).all(|w| w[1] > w[0]), "{numbers:?}");
        assert_eq!(action_of(&sent.last().expect("last").1), ACTION_BYE);
        assert!(stats.snapshot().0 >= 3);
    }

    #[test]
    fn boot_sequence_id_is_nonzero() {
        assert!(boot_sequence_id() >= 1);
    }

    #[test]
    fn scope_matching_helpers() {
        assert!(scope_matches(
            &MatchBy::Rfc3986,
            "http://a/b",
            "http://A/b/c"
        ));
        assert!(!scope_matches(
            &MatchBy::Rfc3986,
            "http://a/b/c",
            "http://a/b"
        ));
        assert!(scope_matches(&MatchBy::Rfc3986, "urn:x", "urn:x"));
        assert!(!scope_matches(&MatchBy::Unknown("q".into()), "a", "a"));
        assert!(is_unicast_peer("10.0.0.1".parse().expect("ip")));
        assert!(!is_unicast_peer("239.255.255.250".parse().expect("ip")));
    }
}
