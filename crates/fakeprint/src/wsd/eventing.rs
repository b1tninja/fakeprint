//! WS-Eventing (2004/08) event source for the WS-Print service.
//!
//! [`EventManager`] is the subscription table and the [`EventBus`] the print stack publishes
//! into. It never touches the network itself:
//!
//! * [`EventManager::handle`] answers the four management operations (Subscribe, Renew,
//!   GetStatus, Unsubscribe) as pure request -> reply, with `wse:` / `wsa:` faults.
//! * [`EventManager::publish`] queues the event for every matching subscription.
//! * [`EventManager::flush`] hands due notifications (and SubscriptionEnd messages) to a
//!   [`Deliver`] implementation. [`HttpDeliver`] is the std-only HTTP/1.1 client; tests use a
//!   recording one. [`run_pump`] calls `flush` on a timer from async code.
//!
//! Semantics:
//!
//! * Filter dialect: the DPWS action dialect (a whitespace separated list of event action
//!   URIs). No `Filter` means every event. Other dialects fault with
//!   `wse:FilteringRequestedUnavailable`.
//! * Buffering: `CompleteState` events (status summary, job status) keep only the newest per
//!   key (summary / job id) and are sent no more often than the event rate; `Cumulative`
//!   events are never merged or dropped (a queue overflow ends the subscription with
//!   `DeliveryFailure` rather than losing an event). A terminal `JobEndState` supersedes any
//!   pending `JobStatus` of the same job.
//! * Safety: `NotifyTo`/`EndTo` must be `http://` with a numeric loopback/private address
//!   unless the policy is relaxed; subscribe rate, subscription count and per-target count are
//!   bounded.
//! * Time comes from the print context's [`Clock`] (`wsd::ctx`), injectable for tests.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use super::ctx::{format_datetime, Clock, SystemClock};
use super::envelope::EndpointRef;
use super::events;
use super::fault::{FaultCode, PrintFault};
use super::soap::{SoapReply, SoapRequest};
use super::types::{EventBus, EventCategory, PrintEvent, PrintOp};
use super::xml::{ns, Element, QName};

// ---------------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------------

/// Delivery mode URI (push is the only mode).
pub const DELIVERY_MODE_PUSH: &str =
    "http://schemas.xmlsoap.org/ws/2004/08/eventing/DeliveryModes/Push";
/// DPWS filter dialect: whitespace separated action URIs.
pub const DIALECT_ACTION: &str = "http://schemas.xmlsoap.org/ws/2006/02/devprof/Action";
pub const ACTION_SUBSCRIPTION_END: &str =
    "http://schemas.xmlsoap.org/ws/2004/08/eventing/SubscriptionEnd";
pub const STATUS_DELIVERY_FAILURE: &str =
    "http://schemas.xmlsoap.org/ws/2004/08/eventing/DeliveryFailure";
pub const STATUS_SOURCE_SHUTTING_DOWN: &str =
    "http://schemas.xmlsoap.org/ws/2004/08/eventing/SourceShuttingDown";
pub const STATUS_SOURCE_CANCELLING: &str =
    "http://schemas.xmlsoap.org/ws/2004/08/eventing/SourceCancelling";

const MAX_URL: usize = 2048;
const MAX_FILTER_ACTIONS: usize = 32;
const MAX_ACTION_LEN: usize = 512;

// ---------------------------------------------------------------------------------------------
// Time formats
// ---------------------------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `xs:dateTime` (`Z` or a numeric offset; fractional seconds are dropped).
pub fn parse_datetime(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    if !s.is_ascii() || s.len() < 20 || s.len() > 40 {
        return None;
    }
    let b = s.as_bytes();
    let num = |a: usize, z: usize| -> Option<i64> {
        let part = s.get(a..z)?;
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let offset = match s.get(i..)? {
        "Z" => 0,
        tz if tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-')) && &tz[3..4] == ":" => {
            let hh: i64 = tz[1..3].parse().ok()?;
            let mm: i64 = tz[4..6].parse().ok()?;
            if hh > 14 || mm > 59 {
                return None;
            }
            let v = hh * 3600 + mm * 60;
            if tz.starts_with('-') {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    let total = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset;
    u64::try_from(total)
        .ok()
        .map(|t| UNIX_EPOCH + Duration::from_secs(t))
}

/// The text is not an `xs:duration` this service understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadDuration;

/// Parse `xs:duration`. `Ok(None)` means a negative duration. Years count as 365 days and
/// months as 30 days.
pub fn parse_duration(s: &str) -> Result<Option<Duration>, BadDuration> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let s = s.strip_prefix('P').ok_or(BadDuration)?;
    if s.is_empty() || s.len() > 64 || !s.is_ascii() {
        return Err(BadDuration);
    }
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => {
            if t.is_empty() {
                return Err(BadDuration);
            }
            (d, Some(t))
        }
        None => (s, None),
    };
    let mut total: f64 = 0.0;
    let mut any = false;
    let mut scan =
        |part: &str, units: &[(char, f64)], allow_frac_last: bool| -> Result<(), BadDuration> {
            let mut rest = part;
            let mut last_idx: Option<usize> = None;
            while !rest.is_empty() {
                let pos = rest
                    .find(|c: char| c.is_ascii_alphabetic())
                    .ok_or(BadDuration)?;
                let (n, tail) = rest.split_at(pos);
                let unit = tail.chars().next().ok_or(BadDuration)?;
                let idx = units
                    .iter()
                    .position(|(u, _)| *u == unit)
                    .ok_or(BadDuration)?;
                if last_idx.is_some_and(|l| idx <= l) || n.is_empty() {
                    return Err(BadDuration);
                }
                let frac = n.contains('.');
                if frac && !(allow_frac_last && unit == 'S') {
                    return Err(BadDuration);
                }
                if !n.bytes().all(|c| c.is_ascii_digit() || c == b'.') || n.matches('.').count() > 1
                {
                    return Err(BadDuration);
                }
                let v: f64 = n.parse().map_err(|_| BadDuration)?;
                total += v * units[idx].1;
                last_idx = Some(idx);
                any = true;
                rest = &tail[1..];
            }
            Ok(())
        };
    scan(
        date,
        &[
            ('Y', 365.0 * 86_400.0),
            ('M', 30.0 * 86_400.0),
            ('D', 86_400.0),
        ],
        false,
    )?;
    if let Some(t) = time {
        scan(t, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)], true)?;
    }
    if !any || !total.is_finite() || total > 4.0e9 {
        return Err(BadDuration);
    }
    if neg {
        return Ok(None);
    }
    Ok(Some(Duration::from_secs_f64(total)))
}

/// `xs:duration` text for a span (`PT1H30M`).
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return "PT0S".to_string();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut out = String::from("PT");
    if h > 0 {
        out.push_str(&format!("{h}H"));
    }
    if m > 0 {
        out.push_str(&format!("{m}M"));
    }
    if s > 0 {
        out.push_str(&format!("{s}S"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Target validation
// ---------------------------------------------------------------------------------------------

/// Which `NotifyTo`/`EndTo` addresses are acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressPolicy {
    /// Numeric loopback, private (RFC 1918 / ULA) and link-local addresses only. Default.
    PrivateOnly,
    /// Any unicast address, host names included (resolved at delivery time).
    AllowAny,
}

/// A validated `http://` delivery target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub ip: Option<IpAddr>,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl Target {
    /// `host:port` for the `Host` header (IPv6 literals bracketed).
    pub fn authority(&self) -> String {
        match self.ip {
            Some(IpAddr::V6(_)) => format!("[{}]:{}", self.host, self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }
}

fn ip_allowed(ip: IpAddr, policy: AddressPolicy) -> Result<(), String> {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    if ip.is_unspecified() || ip.is_multicast() {
        return Err("unspecified or multicast address".into());
    }
    if let IpAddr::V4(v4) = ip {
        if v4.is_broadcast() {
            return Err("broadcast address".into());
        }
    }
    if policy == AddressPolicy::AllowAny {
        return Ok(());
    }
    let ok = match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || (v4.is_link_local() && v4 != Ipv4Addr::new(169, 254, 169, 254))
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    };
    if ok {
        Ok(())
    } else {
        Err("address is not loopback/private".into())
    }
}

/// Validate an event delivery URL. The error is a human-readable reason.
pub fn parse_target(url: &str, policy: AddressPolicy) -> Result<Target, String> {
    let url = url.trim();
    if url.is_empty() || url.len() > MAX_URL {
        return Err("empty or oversized address".into());
    }
    if !url.bytes().all(|c| (0x21..=0x7e).contains(&c)) {
        return Err("address contains whitespace or control characters".into());
    }
    if url.contains('#') {
        return Err("address contains a fragment".into());
    }
    let rest = match url.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("http://") => &url[7..],
        _ => return Err("only http:// delivery is supported".into()),
    };
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(split);
    if authority.is_empty() || authority.contains('@') {
        return Err("missing host or user information in address".into());
    }
    let path = if tail.is_empty() {
        "/".to_string()
    } else if tail.starts_with('?') {
        format!("/{tail}")
    } else {
        tail.to_string()
    };
    let (host, port_text): (&str, Option<&str>) = if let Some(r) = authority.strip_prefix('[') {
        let end = r.find(']').ok_or("unterminated IPv6 literal")?;
        let after = &r[end + 1..];
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or("bad text after IPv6 literal")?),
        };
        (&r[..end], port)
    } else {
        match authority.matches(':').count() {
            0 => (authority, None),
            1 => {
                let (h, p) = authority.split_once(':').ok_or("bad authority")?;
                (h, Some(p))
            }
            _ => return Err("unbracketed IPv6 literal".into()),
        }
    };
    let port: u16 = match port_text {
        None => 80,
        Some(p) => p.parse().map_err(|_| "bad port".to_string())?,
    };
    if port == 0 {
        return Err("port 0".into());
    }
    let ip = host.parse::<IpAddr>().ok();
    match ip {
        Some(ip) => ip_allowed(ip, policy)?,
        None => {
            if policy == AddressPolicy::PrivateOnly {
                return Err("a numeric address is required".into());
            }
            let ok = !host.is_empty()
                && host.len() <= 253
                && host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.');
            if !ok {
                return Err("invalid host name".into());
            }
        }
    }
    Ok(Target {
        host: host.to_string(),
        ip,
        port,
        path,
    })
}

// ---------------------------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverError(pub String);

impl std::fmt::Display for DeliverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DeliverError {}

/// Sends one SOAP message to a subscriber endpoint. Blocking; call from a worker thread.
pub trait Deliver: Send + Sync {
    fn deliver(&self, target: &EndpointRef, msg: &SoapReply) -> Result<(), DeliverError>;
}

/// HTTP/1.1 POST over `std::net::TcpStream` with timeouts. Re-validates the target against
/// its address policy at send time.
#[derive(Debug, Clone)]
pub struct HttpDeliver {
    pub policy: AddressPolicy,
    pub timeout: Duration,
    /// Where each delivery is recorded as an outbound request (none: not recorded).
    pub log: Option<Arc<crate::reqlog::RequestLog>>,
}

impl Default for HttpDeliver {
    fn default() -> Self {
        Self {
            policy: AddressPolicy::PrivateOnly,
            timeout: Duration::from_secs(3),
            log: None,
        }
    }
}

impl Deliver for HttpDeliver {
    fn deliver(&self, target: &EndpointRef, msg: &SoapReply) -> Result<(), DeliverError> {
        let started = std::time::Instant::now();
        let t = parse_target(&target.address, self.policy).map_err(DeliverError)?;
        let addr: SocketAddr = match t.ip {
            Some(ip) => SocketAddr::new(ip, t.port),
            None => (t.host.as_str(), t.port)
                .to_socket_addrs()
                .map_err(|e| DeliverError(format!("resolve {}: {e}", t.host)))?
                .find(|a| ip_allowed(a.ip(), self.policy).is_ok())
                .ok_or_else(|| DeliverError("no acceptable address".into()))?,
        };
        let http = msg.clone().into_http();
        let mut content_type = http.content_type.clone();
        if !content_type.to_ascii_lowercase().starts_with("multipart/") {
            content_type.push_str(&format!("; action=\"{}\"", msg.action));
        }
        let record = |status: Option<u16>, error: Option<String>| {
            if let Some(log) = &self.log {
                log.record_outbound(
                    &addr.to_string(),
                    &t.path,
                    vec![
                        ("Host".into(), t.authority()),
                        ("Content-Type".into(), content_type.clone()),
                        ("Content-Length".into(), http.body.len().to_string()),
                        ("Connection".into(), "close".into()),
                    ],
                    &http.body,
                    status,
                    error,
                    started,
                    &msg.action,
                );
            }
        };
        let mut stream = match TcpStream::connect_timeout(&addr, self.timeout) {
            Ok(s) => s,
            Err(e) => {
                record(None, Some(format!("connect: {e}")));
                return Err(DeliverError(format!("connect {addr}: {e}")));
            }
        };
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));
        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            t.path,
            t.authority(),
            content_type,
            http.body.len()
        );
        if let Err(e) = stream
            .write_all(head.as_bytes())
            .and_then(|_| stream.write_all(&http.body))
        {
            record(None, Some(format!("write: {e}")));
            return Err(DeliverError(format!("write: {e}")));
        }
        let mut buf = [0u8; 1024];
        let mut got = 0usize;
        while got < buf.len() {
            match stream.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => {
                    got += n;
                    if buf[..got].windows(2).any(|w| w == b"\r\n") {
                        break;
                    }
                }
                Err(e) => {
                    record(None, Some(format!("read: {e}")));
                    return Err(DeliverError(format!("read: {e}")));
                }
            }
        }
        let line = String::from_utf8_lossy(&buf[..got]);
        let status = line
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok());
        let Some(status) = status else {
            record(None, Some("no HTTP status line".into()));
            return Err(DeliverError("no HTTP status line".into()));
        };
        record(Some(status), None);
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(DeliverError(format!("subscriber answered HTTP {status}")))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration and filters
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EventingConfig {
    /// Address of the event source / subscription manager (the print service endpoint).
    pub event_source: String,
    pub default_expires: Duration,
    pub min_expires: Duration,
    pub max_expires: Duration,
    pub max_subscriptions: usize,
    /// Subscriptions allowed per delivery target host:port.
    pub max_per_target: usize,
    /// Subscribe requests accepted per `subscribe_window`.
    pub max_subscribes_per_window: usize,
    pub subscribe_window: Duration,
    /// Queued notifications per subscription.
    pub max_queue: usize,
    /// Notifications handed out per subscription per flush.
    pub max_per_flush: usize,
    /// Minimum spacing between CompleteState notifications to one subscriber.
    pub event_rate: Duration,
    /// Consecutive delivery failures before the subscription is ended.
    pub max_failures: u32,
    pub address_policy: AddressPolicy,
}

impl EventingConfig {
    pub fn new(event_source: impl Into<String>) -> Self {
        Self {
            event_source: event_source.into(),
            default_expires: Duration::from_secs(3600),
            min_expires: Duration::from_secs(1),
            max_expires: Duration::from_secs(24 * 3600),
            max_subscriptions: 64,
            max_per_target: 8,
            max_subscribes_per_window: 30,
            subscribe_window: Duration::from_secs(10),
            max_queue: 256,
            max_per_flush: 32,
            event_rate: Duration::from_secs(1),
            max_failures: 3,
            address_policy: AddressPolicy::PrivateOnly,
        }
    }
}

/// Which events a subscription receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    All,
    /// Exact event action URIs.
    Actions(Vec<String>),
    /// Programmatic only: every event of a category.
    Category(EventCategory),
}

impl Filter {
    pub fn matches(&self, action: &str, category: EventCategory) -> bool {
        match self {
            Filter::All => true,
            Filter::Actions(list) => list.iter().any(|a| a == action),
            Filter::Category(c) => *c == category,
        }
    }
}

/// How the subscriber expressed `Expires`; replies use the same form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiresStyle {
    Duration,
    DateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionInfo {
    pub id: String,
    pub notify_to: String,
    pub end_to: Option<String>,
    pub filter: Filter,
    pub expires: SystemTime,
    pub queued: usize,
    pub failures: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlushReport {
    pub delivered: usize,
    pub failed: usize,
    pub ends_sent: usize,
    pub subscriptions_ended: usize,
}

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Queued {
    key: Option<String>,
    event: PrintEvent,
}

#[derive(Debug)]
struct Sub {
    id: String,
    notify_to: EndpointRef,
    end_to: Option<EndpointRef>,
    filter: Filter,
    expires: SystemTime,
    style: ExpiresStyle,
    queue: VecDeque<Queued>,
    last_complete_sent: Option<SystemTime>,
    failures: u32,
    retry_at: Option<SystemTime>,
}

#[derive(Debug, Default)]
struct State {
    subs: Vec<Sub>,
    outbox: Vec<(EndpointRef, SoapReply)>,
    subscribe_times: VecDeque<SystemTime>,
    event_rate: Duration,
    ended_total: usize,
}

pub struct EventManager {
    config: EventingConfig,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

fn wse(local: &str) -> QName {
    QName::new(ns::WSE, local)
}

fn wse_fault(local: &str, code: FaultCode, reason: &str) -> PrintFault {
    PrintFault::custom(code, Some(wse(local)), reason)
}

fn invalid_message(reason: &str) -> PrintFault {
    wse_fault("InvalidMessage", FaultCode::Sender, reason)
}

impl EventManager {
    pub fn new(config: EventingConfig, clock: Arc<dyn Clock>) -> Self {
        let state = State {
            event_rate: config.event_rate,
            ..State::default()
        };
        Self {
            config,
            clock,
            state: Mutex::new(state),
        }
    }

    pub fn with_system_clock(config: EventingConfig) -> Self {
        Self::new(config, Arc::new(SystemClock))
    }

    pub fn config(&self) -> &EventingConfig {
        &self.config
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Set the CompleteState spacing (`SetEventRate`, 1..=600 seconds).
    pub fn set_event_rate(&self, seconds: u32) -> Result<(), PrintFault> {
        if !(1..=600).contains(&seconds) {
            return Err(PrintFault::invalid_args(
                "EventRate must be between 1 and 600",
            ));
        }
        self.lock().event_rate = Duration::from_secs(seconds as u64);
        Ok(())
    }

    pub fn event_rate(&self) -> Duration {
        self.lock().event_rate
    }

    pub fn subscription_count(&self) -> usize {
        self.lock().subs.len()
    }

    /// Snapshot of the table (expired entries are removed first).
    pub fn subscriptions(&self) -> Vec<SubscriptionInfo> {
        let now = self.clock.now();
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        st.subs
            .iter()
            .map(|s| SubscriptionInfo {
                id: s.id.clone(),
                notify_to: s.notify_to.address.clone(),
                end_to: s.end_to.as_ref().map(|e| e.address.clone()),
                filter: s.filter.clone(),
                expires: s.expires,
                queued: s.queue.len(),
                failures: s.failures,
            })
            .collect()
    }

    /// Subscribe without a SOAP request (lab scripts, tests). Returns the identifier.
    pub fn subscribe_direct(
        &self,
        notify_to: EndpointRef,
        end_to: Option<EndpointRef>,
        filter: Filter,
        expires: Duration,
    ) -> Result<String, PrintFault> {
        let now = self.clock.now();
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let (id, _) = self.add_locked(
            &mut st,
            now,
            notify_to,
            end_to,
            filter,
            expires,
            ExpiresStyle::Duration,
        )?;
        Ok(id)
    }

    /// Queue `event` for one subscription only (if its filter matches). Used to seed a new
    /// subscriber with the current state. False when the subscription is unknown or the
    /// event does not match its filter.
    pub fn queue_for(&self, id: &str, event: PrintEvent) -> bool {
        let now = self.clock.now();
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let max_queue = self.config.max_queue;
        let Some(sub) = st.subs.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if !sub.filter.matches(event.action(), event.category()) {
            return false;
        }
        let key = match &event {
            PrintEvent::PrinterStatusSummary { .. } => Some("summary".to_string()),
            PrintEvent::JobStatus { job } => Some(format!("job:{}", job.id)),
            _ => None,
        };
        if event.category() == EventCategory::CompleteState {
            if let Some(k) = &key {
                sub.queue.retain(|q| q.key.as_ref() != Some(k));
            }
        }
        if sub.queue.len() >= max_queue {
            return false;
        }
        sub.queue.push_back(Queued { key, event });
        true
    }

    // ----- time-driven maintenance -----

    fn expire_locked(&self, st: &mut State, now: SystemTime) {
        st.subs.retain(|s| s.expires > now);
    }

    /// Drop expired subscriptions; returns how many went.
    pub fn expire(&self) -> usize {
        let now = self.clock.now();
        let mut st = self.lock();
        let before = st.subs.len();
        self.expire_locked(&mut st, now);
        before - st.subs.len()
    }

    fn manager_epr(&self, id: &str) -> EndpointRef {
        let mut epr = EndpointRef::new(self.config.event_source.as_str());
        epr.reference_parameters
            .push(Element::text_el(ns::WSE, "Identifier", id));
        epr
    }

    fn end_locked(&self, st: &mut State, id: &str, status: &str, reason: &str) {
        let Some(pos) = st.subs.iter().position(|s| s.id == id) else {
            return;
        };
        let sub = st.subs.remove(pos);
        st.ended_total += 1;
        if let Some(end_to) = sub.end_to {
            let body = Element::new(ns::WSE, "SubscriptionEnd")
                .with_child(
                    self.manager_epr(&sub.id)
                        .to_element(wse("SubscriptionManager")),
                )
                .with_child(Element::text_el(ns::WSE, "Status", status))
                .with_child(
                    Element::text_el(ns::WSE, "Reason", reason)
                        .with_attr_ns(QName::new(ns::XML, "lang"), "en"),
                );
            let msg = SoapReply::notification(&end_to, ACTION_SUBSCRIPTION_END, body);
            st.outbox.push((end_to, msg));
        }
    }

    /// The event source cancels one subscription (`SourceCancelling`).
    pub fn cancel(&self, id: &str, reason: &str) -> bool {
        let mut st = self.lock();
        let had = st.subs.iter().any(|s| s.id == id);
        self.end_locked(&mut st, id, STATUS_SOURCE_CANCELLING, reason);
        had
    }

    /// The service is going away: end every subscription with `SourceShuttingDown`. The
    /// SubscriptionEnd messages go out on the next [`EventManager::flush`].
    pub fn shutdown(&self) {
        let mut st = self.lock();
        let ids: Vec<String> = st.subs.iter().map(|s| s.id.clone()).collect();
        for id in ids {
            self.end_locked(
                &mut st,
                &id,
                STATUS_SOURCE_SHUTTING_DOWN,
                "the event source is shutting down",
            );
        }
    }

    // ----- table operations -----

    #[allow(clippy::too_many_arguments)]
    fn add_locked(
        &self,
        st: &mut State,
        now: SystemTime,
        notify_to: EndpointRef,
        end_to: Option<EndpointRef>,
        filter: Filter,
        requested: Duration,
        style: ExpiresStyle,
    ) -> Result<(String, Duration), PrintFault> {
        let cfg = &self.config;
        // subscribe rate
        while st
            .subscribe_times
            .front()
            .is_some_and(|t| now.duration_since(*t).unwrap_or_default() > cfg.subscribe_window)
        {
            st.subscribe_times.pop_front();
        }
        if st.subscribe_times.len() >= cfg.max_subscribes_per_window {
            return Err(wse_fault(
                "EventSourceUnableToProcess",
                FaultCode::Receiver,
                "too many Subscribe requests",
            ));
        }
        // a repeat of an identical subscription replaces the old one
        if let Some(pos) = st
            .subs
            .iter()
            .position(|s| s.notify_to == notify_to && s.filter == filter && s.end_to == end_to)
        {
            st.subs.remove(pos);
        }
        if st.subs.len() >= cfg.max_subscriptions {
            return Err(wse_fault(
                "EventSourceUnableToProcess",
                FaultCode::Receiver,
                "subscription table is full",
            ));
        }
        let target_key = authority_key(&notify_to.address);
        if st
            .subs
            .iter()
            .filter(|s| authority_key(&s.notify_to.address) == target_key)
            .count()
            >= cfg.max_per_target
        {
            return Err(wse_fault(
                "EventSourceUnableToProcess",
                FaultCode::Receiver,
                "too many subscriptions for this target",
            ));
        }
        st.subscribe_times.push_back(now);
        let granted = requested.clamp(cfg.min_expires, cfg.max_expires);
        let id = format!("uuid:{}", Uuid::new_v4());
        st.subs.push(Sub {
            id: id.clone(),
            notify_to,
            end_to,
            filter,
            expires: now + granted,
            style,
            queue: VecDeque::new(),
            last_complete_sent: None,
            failures: 0,
            retry_at: None,
        });
        Ok((id, granted))
    }

    // ----- management operations -----

    /// Answer a Subscribe, Renew, GetStatus or Unsubscribe request.
    pub fn handle(&self, req: &SoapRequest) -> SoapReply {
        match self.handle_inner(req) {
            Ok(reply) => reply,
            Err(fault) => req.fault_reply(&fault),
        }
    }

    fn handle_inner(&self, req: &SoapRequest) -> Result<SoapReply, PrintFault> {
        let op = PrintOp::from_action(&req.action)
            .filter(|op| {
                matches!(
                    op,
                    PrintOp::Subscribe | PrintOp::Renew | PrintOp::GetStatus | PrintOp::Unsubscribe
                )
            })
            .ok_or_else(|| PrintFault::action_not_supported(&req.action))?;
        req.check_must_understand(&[])?;
        let body = req.body().ok_or_else(|| invalid_message("empty body"))?;
        if !body.is(ns::WSE, op.name()) {
            return Err(invalid_message(&format!(
                "the body is not wse:{}",
                op.name()
            )));
        }
        let now = self.clock.now();
        match op {
            PrintOp::Subscribe => self.subscribe(req, body, now),
            PrintOp::Renew => self.renew(req, body, now),
            PrintOp::GetStatus => self.get_status(req, now),
            _ => self.unsubscribe(req, now),
        }
    }

    fn identifier(req: &SoapRequest) -> Result<String, PrintFault> {
        let id = req
            .header(&wse("Identifier"))
            .map(|e| e.text_trimmed().to_string())
            .filter(|s| !s.is_empty());
        id.ok_or_else(|| invalid_message("the wse:Identifier header is required"))
    }

    fn parse_epr(
        el: &Element,
        what: &str,
        policy: AddressPolicy,
    ) -> Result<EndpointRef, PrintFault> {
        let epr = EndpointRef::from_element(el)
            .map_err(|_| invalid_message(&format!("{what} is not an endpoint reference")))?;
        parse_target(&epr.address, policy)
            .map_err(|why| invalid_message(&format!("{what} rejected: {why}")))?;
        Ok(epr)
    }

    fn parse_expires(
        &self,
        text: &str,
        now: SystemTime,
    ) -> Result<(Duration, ExpiresStyle), PrintFault> {
        let t = text.trim();
        let unsupported = || {
            wse_fault(
                "UnsupportedExpirationType",
                FaultCode::Sender,
                "Expires must be an xs:duration or xs:dateTime",
            )
        };
        let invalid = || {
            wse_fault(
                "InvalidExpirationTime",
                FaultCode::Sender,
                "the expiration time is not in the future",
            )
        };
        if t.starts_with('P') || t.starts_with("-P") {
            match parse_duration(t).map_err(|_| unsupported())? {
                Some(d) if !d.is_zero() => Ok((d, ExpiresStyle::Duration)),
                _ => Err(invalid()),
            }
        } else if let Some(at) = parse_datetime(t) {
            match at.duration_since(now) {
                Ok(d) if !d.is_zero() => Ok((d, ExpiresStyle::DateTime)),
                _ => Err(invalid()),
            }
        } else {
            Err(unsupported())
        }
    }

    fn expires_element(&self, style: ExpiresStyle, granted: Duration, at: SystemTime) -> Element {
        let text = match style {
            ExpiresStyle::Duration => format_duration(granted),
            ExpiresStyle::DateTime => format_datetime(at),
        };
        Element::text_el(ns::WSE, "Expires", text)
    }

    fn subscribe(
        &self,
        req: &SoapRequest,
        body: &Element,
        now: SystemTime,
    ) -> Result<SoapReply, PrintFault> {
        let policy = self.config.address_policy;
        let (mut end_to_el, mut delivery, mut expires_el, mut filter_el) = (None, None, None, None);
        for c in body.children.iter().filter(|c| c.name.ns == ns::WSE) {
            let slot = match c.name.local.as_str() {
                "EndTo" => &mut end_to_el,
                "Delivery" => &mut delivery,
                "Expires" => &mut expires_el,
                "Filter" => &mut filter_el,
                _ => continue,
            };
            if slot.replace(c).is_some() {
                return Err(invalid_message(&format!(
                    "wse:{} appears more than once",
                    c.name.local
                )));
            }
        }
        let delivery = delivery.ok_or_else(|| invalid_message("wse:Delivery is required"))?;
        if let Some(mode) = delivery.attr_local("Mode") {
            if mode.trim() != DELIVERY_MODE_PUSH {
                return Err(wse_fault(
                    "DeliveryModeRequestedUnavailable",
                    FaultCode::Sender,
                    "only the push delivery mode is supported",
                ));
            }
        }
        let notify_name = wse("NotifyTo");
        let mut notify_els = delivery.find_all(&notify_name);
        let notify_el = notify_els
            .next()
            .ok_or_else(|| invalid_message("wse:NotifyTo is required"))?;
        if notify_els.next().is_some() {
            return Err(invalid_message("wse:NotifyTo appears more than once"));
        }
        let notify_to = Self::parse_epr(notify_el, "NotifyTo", policy)?;
        let end_to = match end_to_el {
            Some(e) => Some(Self::parse_epr(e, "EndTo", policy)?),
            None => None,
        };
        let (requested, style) = match expires_el {
            Some(e) => self.parse_expires(&e.text, now)?,
            None => (self.config.default_expires, ExpiresStyle::Duration),
        };
        let filter = match filter_el {
            Some(f) => parse_filter(f)?,
            None => Filter::All,
        };
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let (id, granted) =
            self.add_locked(&mut st, now, notify_to, end_to, filter, requested, style)?;
        drop(st);
        let resp = Element::new(ns::WSE, "SubscribeResponse")
            .with_child(self.manager_epr(&id).to_element(wse("SubscriptionManager")))
            .with_child(self.expires_element(style, granted, now + granted));
        Ok(req.reply(
            PrintOp::Subscribe.response_action().unwrap_or_default(),
            resp,
        ))
    }

    fn renew(
        &self,
        req: &SoapRequest,
        body: &Element,
        now: SystemTime,
    ) -> Result<SoapReply, PrintFault> {
        let id = Self::identifier(req)?;
        let requested = match body.child(ns::WSE, "Expires") {
            Some(e) => Some(self.parse_expires(&e.text, now)?),
            None => None,
        };
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let cfg = &self.config;
        let sub = st.subs.iter_mut().find(|s| s.id == id).ok_or_else(|| {
            wse_fault(
                "UnableToRenew",
                FaultCode::Receiver,
                "the subscription does not exist or has expired",
            )
        })?;
        let (want, style) = requested.unwrap_or((cfg.default_expires, sub.style));
        let granted = want.clamp(cfg.min_expires, cfg.max_expires);
        sub.expires = now + granted;
        sub.style = style;
        let el = self.expires_element(style, granted, sub.expires);
        drop(st);
        let resp = Element::new(ns::WSE, "RenewResponse").with_child(el);
        Ok(req.reply(PrintOp::Renew.response_action().unwrap_or_default(), resp))
    }

    fn get_status(&self, req: &SoapRequest, now: SystemTime) -> Result<SoapReply, PrintFault> {
        let id = Self::identifier(req)?;
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let sub = st
            .subs
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| invalid_message("unknown or expired subscription"))?;
        let remaining = sub.expires.duration_since(now).unwrap_or_default();
        let el = self.expires_element(sub.style, remaining, sub.expires);
        drop(st);
        let resp = Element::new(ns::WSE, "GetStatusResponse").with_child(el);
        Ok(req.reply(
            PrintOp::GetStatus.response_action().unwrap_or_default(),
            resp,
        ))
    }

    fn unsubscribe(&self, req: &SoapRequest, now: SystemTime) -> Result<SoapReply, PrintFault> {
        let id = Self::identifier(req)?;
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let pos = st
            .subs
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| invalid_message("unknown or expired subscription"))?;
        st.subs.remove(pos);
        drop(st);
        Ok(req.reply_empty(PrintOp::Unsubscribe.response_action().unwrap_or_default()))
    }

    // ----- delivery -----

    /// Hand every due notification and pending SubscriptionEnd to `deliver`.
    pub fn flush(&self, deliver: &dyn Deliver) -> FlushReport {
        let now = self.clock.now();
        let mut report = FlushReport::default();
        let (work, outbox, rate) = {
            let mut st = self.lock();
            self.expire_locked(&mut st, now);
            let rate = st.event_rate;
            let max = self.config.max_per_flush;
            let mut work: Vec<(String, EndpointRef, Vec<Queued>)> = Vec::new();
            for sub in st.subs.iter_mut() {
                if sub.retry_at.is_some_and(|t| t > now) || sub.queue.is_empty() {
                    continue;
                }
                let mut complete_ok = sub
                    .last_complete_sent
                    .map(|t| now.duration_since(t).map(|d| d >= rate).unwrap_or(false))
                    .unwrap_or(true);
                let mut take = Vec::new();
                let mut keep = VecDeque::new();
                for q in sub.queue.drain(..) {
                    let ready = take.len() < max
                        && match q.event.category() {
                            EventCategory::Cumulative => true,
                            EventCategory::CompleteState => {
                                let ok = complete_ok;
                                if ok {
                                    complete_ok = false;
                                }
                                ok
                            }
                        };
                    if ready {
                        take.push(q);
                    } else {
                        keep.push_back(q);
                    }
                }
                sub.queue = keep;
                if !take.is_empty() {
                    work.push((sub.id.clone(), sub.notify_to.clone(), take));
                }
            }
            (work, std::mem::take(&mut st.outbox), rate)
        };
        let _ = rate;

        for (id, target, items) in work {
            let mut sent_complete = false;
            let mut leftover: Option<Vec<Queued>> = None;
            for (i, q) in items.iter().enumerate() {
                let msg = events::notification(&target, &q.event);
                match deliver.deliver(&target, &msg) {
                    Ok(()) => {
                        report.delivered += 1;
                        if q.event.category() == EventCategory::CompleteState {
                            sent_complete = true;
                        }
                    }
                    Err(_) => {
                        report.failed += 1;
                        leftover = Some(items[i..].to_vec());
                        break;
                    }
                }
            }
            let mut st = self.lock();
            let max_failures = self.config.max_failures;
            let Some(pos) = st.subs.iter().position(|s| s.id == id) else {
                continue;
            };
            let mut give_up = false;
            {
                let sub = &mut st.subs[pos];
                if sent_complete {
                    sub.last_complete_sent = Some(now);
                }
                match leftover {
                    None => {
                        sub.failures = 0;
                        sub.retry_at = None;
                    }
                    Some(rest) => {
                        for q in rest.into_iter().rev() {
                            sub.queue.push_front(q);
                        }
                        sub.failures += 1;
                        let backoff =
                            Duration::from_secs((1u64 << (sub.failures - 1).min(5)).min(30));
                        sub.retry_at = Some(now + backoff);
                        give_up = sub.failures >= max_failures;
                    }
                }
            }
            if give_up {
                self.end_locked(
                    &mut st,
                    &id,
                    STATUS_DELIVERY_FAILURE,
                    "event delivery failed repeatedly",
                );
                report.subscriptions_ended += 1;
            }
        }

        // SubscriptionEnd messages: best effort, once
        let mut more = {
            let mut st = self.lock();
            std::mem::take(&mut st.outbox)
        };
        let mut all = outbox;
        all.append(&mut more);
        for (target, msg) in all {
            if deliver.deliver(&target, &msg).is_ok() {
                report.ends_sent += 1;
            }
        }
        report
    }
}

fn authority_key(url: &str) -> String {
    match parse_target(url, AddressPolicy::AllowAny) {
        Ok(t) => t.authority().to_ascii_lowercase(),
        Err(_) => url.to_string(),
    }
}

fn parse_filter(f: &Element) -> Result<Filter, PrintFault> {
    let dialect = f.attr_local("Dialect").map(str::trim);
    match dialect {
        Some(DIALECT_ACTION) => {}
        _ => {
            return Err(wse_fault(
                "FilteringRequestedUnavailable",
                FaultCode::Sender,
                "only the devprof Action filter dialect is supported",
            ))
        }
    }
    if !f.children.is_empty() {
        return Err(invalid_message("an Action filter contains text only"));
    }
    let actions: Vec<String> = f.text.split_whitespace().map(str::to_string).collect();
    if actions.is_empty() || actions.len() > MAX_FILTER_ACTIONS {
        return Err(invalid_message(
            "the filter must list between 1 and 32 actions",
        ));
    }
    if actions
        .iter()
        .any(|a| a.len() > MAX_ACTION_LEN || !a.contains(':'))
    {
        return Err(invalid_message("a filter action is not an absolute URI"));
    }
    Ok(Filter::Actions(actions))
}

impl EventBus for EventManager {
    fn publish(&self, event: PrintEvent) {
        let now = self.clock.now();
        let mut st = self.lock();
        self.expire_locked(&mut st, now);
        let action = event.action();
        let category = event.category();
        let key = match &event {
            PrintEvent::PrinterStatusSummary { .. } => Some("summary".to_string()),
            PrintEvent::JobStatus { job } => Some(format!("job:{}", job.id)),
            _ => None,
        };
        let superseded = match &event {
            PrintEvent::JobEndState { job } => Some(format!("job:{}", job.id)),
            _ => None,
        };
        let max_queue = self.config.max_queue;
        let mut overflow = Vec::new();
        for sub in st.subs.iter_mut() {
            if let Some(k) = &superseded {
                sub.queue.retain(|q| q.key.as_ref() != Some(k));
            }
            if !sub.filter.matches(action, category) {
                continue;
            }
            if category == EventCategory::CompleteState {
                if let Some(k) = &key {
                    sub.queue.retain(|q| q.key.as_ref() != Some(k));
                }
            }
            sub.queue.push_back(Queued {
                key: key.clone(),
                event: event.clone(),
            });
            if sub.queue.len() > max_queue {
                overflow.push(sub.id.clone());
            }
        }
        for id in overflow {
            self.end_locked(
                &mut st,
                &id,
                STATUS_DELIVERY_FAILURE,
                "the event queue overflowed",
            );
        }
    }
}

/// Flush on a timer until `stop` fires, then send `SourceShuttingDown` messages. Blocking
/// delivery runs on the blocking pool.
pub async fn run_pump(
    manager: Arc<EventManager>,
    deliver: Arc<dyn Deliver>,
    tick: Duration,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(tick) => {
                let (m, d) = (Arc::clone(&manager), Arc::clone(&deliver));
                let _ = tokio::task::spawn_blocking(move || m.flush(d.as_ref())).await;
            }
        }
    }
    manager.shutdown();
    let (m, d) = (Arc::clone(&manager), Arc::clone(&deliver));
    let _ = tokio::task::spawn_blocking(move || m.flush(d.as_ref())).await;
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::ctx::ManualClock;
    use crate::wsd::types::{DeviceCondition, JobState, PrintJob, PrinterState, Severity};

    const SVC: &str = "http://192.168.1.50:3921/WebServices/PrinterService";

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(String, String)>>, // (to, action)
        fail: Mutex<bool>,
    }
    impl Recorder {
        fn actions(&self) -> Vec<String> {
            self.sent
                .lock()
                .expect("lock")
                .iter()
                .map(|(_, a)| a.clone())
                .collect()
        }
        fn set_fail(&self, f: bool) {
            *self.fail.lock().expect("lock") = f;
        }
    }
    impl Deliver for Recorder {
        fn deliver(&self, target: &EndpointRef, msg: &SoapReply) -> Result<(), DeliverError> {
            if *self.fail.lock().expect("lock") && !target.address.ends_with("/end") {
                return Err(DeliverError("down".into()));
            }
            self.sent
                .lock()
                .expect("lock")
                .push((target.address.clone(), msg.action.clone()));
            Ok(())
        }
    }

    fn mgr() -> (EventManager, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let mut cfg = EventingConfig::new(SVC);
        cfg.event_rate = Duration::from_secs(5);
        (EventManager::new(cfg, clock.clone()), clock)
    }

    fn request(action: &str, headers: &str, body: &str) -> SoapRequest {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{}" xmlns:a="{}" xmlns:e="{}"><s:Header><a:To>{SVC}</a:To><a:Action>{action}</a:Action><a:MessageID>urn:uuid:11111111-1111-4111-8111-111111111111</a:MessageID>{headers}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
            ns::SOAP12,
            ns::WSA,
            ns::WSE
        );
        SoapRequest::parse(Some("application/soap+xml"), None, xml.as_bytes())
            .expect("request parses")
    }

    fn subscribe_xml(notify: &str, extra: &str, delivery_attrs: &str) -> String {
        format!(
            r#"<e:Subscribe>{extra}<e:Delivery {delivery_attrs}><e:NotifyTo><a:Address>{notify}</a:Address><a:ReferenceParameters><x:Ctx xmlns:x="urn:x">c1</x:Ctx></a:ReferenceParameters></e:NotifyTo></e:Delivery></e:Subscribe>"#
        )
    }

    fn do_subscribe(m: &EventManager, notify: &str, extra: &str) -> SoapReply {
        m.handle(&request(
            PrintOp::Subscribe.action().expect("a"),
            "",
            &subscribe_xml(notify, extra, ""),
        ))
    }

    fn sub_id(reply: &SoapReply) -> String {
        let body = reply.body.as_ref().expect("body");
        assert_eq!(body.name.local, "SubscribeResponse", "{body:?}");
        let mgr = body.child(ns::WSE, "SubscriptionManager").expect("mgr");
        let params = mgr.child(ns::WSA, "ReferenceParameters").expect("params");
        params
            .child(ns::WSE, "Identifier")
            .expect("id")
            .text
            .clone()
    }

    fn id_header(id: &str) -> String {
        format!("<e:Identifier>{id}</e:Identifier>")
    }

    fn fault_sub(reply: &SoapReply) -> String {
        let body = reply.body.as_ref().expect("body");
        assert_eq!(body.name.local, "Fault", "not a fault: {body:?}");
        let code = body.child(ns::SOAP12, "Code").expect("code");
        let sub = code.child(ns::SOAP12, "Subcode").expect("subcode");
        sub.child(ns::SOAP12, "Value")
            .expect("v")
            .text_qname()
            .expect("q")
            .local
    }

    fn job(id: u32, state: JobState) -> PrintJob {
        let mut j = PrintJob::new(id, "j", "u");
        j.state = state;
        j
    }

    fn summary() -> PrintEvent {
        PrintEvent::PrinterStatusSummary {
            state: PrinterState::Idle,
            primary_reason: None,
            reasons: vec![],
        }
    }

    const OK: &str = "http://127.0.0.1:5357/sink";

    #[test]
    fn lifecycle_subscribe_renew_status_unsubscribe() {
        let (m, clock) = mgr();
        let r = do_subscribe(&m, OK, "<e:Expires>PT10M</e:Expires>");
        let id = sub_id(&r);
        let body = r.body.as_ref().expect("b");
        assert_eq!(body.child(ns::WSE, "Expires").expect("e").text, "PT10M");
        assert_eq!(r.action, PrintOp::Subscribe.response_action().expect("a"));
        assert_eq!(m.subscription_count(), 1);

        clock.advance(Duration::from_secs(60));
        let st = m.handle(&request(
            PrintOp::GetStatus.action().expect("a"),
            &id_header(&id),
            "<e:GetStatus/>",
        ));
        assert_eq!(
            st.body
                .as_ref()
                .expect("b")
                .child(ns::WSE, "Expires")
                .expect("e")
                .text,
            "PT9M"
        );

        let rn = m.handle(&request(
            PrintOp::Renew.action().expect("a"),
            &id_header(&id),
            "<e:Renew><e:Expires>PT1H</e:Expires></e:Renew>",
        ));
        assert_eq!(
            rn.body
                .as_ref()
                .expect("b")
                .child(ns::WSE, "Expires")
                .expect("e")
                .text,
            "PT1H"
        );

        let un = m.handle(&request(
            PrintOp::Unsubscribe.action().expect("a"),
            &id_header(&id),
            "<e:Unsubscribe/>",
        ));
        assert!(un.body.is_none());
        assert_eq!(
            un.action,
            PrintOp::Unsubscribe.response_action().expect("a")
        );
        assert_eq!(m.subscription_count(), 0);
        let again = m.handle(&request(
            PrintOp::Unsubscribe.action().expect("a"),
            &id_header(&id),
            "<e:Unsubscribe/>",
        ));
        assert_eq!(fault_sub(&again), "InvalidMessage");
    }

    #[test]
    fn datetime_expires_and_response_style() {
        let (m, clock) = mgr();
        let at = format_datetime(clock.now() + Duration::from_secs(1800));
        let r = do_subscribe(&m, OK, &format!("<e:Expires>{at}</e:Expires>"));
        let got = r
            .body
            .as_ref()
            .expect("b")
            .child(ns::WSE, "Expires")
            .expect("e")
            .text
            .clone();
        assert_eq!(got, at);
    }

    #[test]
    fn expiry_removes_subscription_and_blocks_renew() {
        let (m, clock) = mgr();
        let id = sub_id(&do_subscribe(&m, OK, "<e:Expires>PT30S</e:Expires>"));
        clock.advance(Duration::from_secs(31));
        let st = m.handle(&request(
            PrintOp::GetStatus.action().expect("a"),
            &id_header(&id),
            "<e:GetStatus/>",
        ));
        assert_eq!(fault_sub(&st), "InvalidMessage");
        let rn = m.handle(&request(
            PrintOp::Renew.action().expect("a"),
            &id_header(&id),
            "<e:Renew/>",
        ));
        assert_eq!(fault_sub(&rn), "UnableToRenew");
        assert_eq!(m.subscription_count(), 0);
        // expired subscriptions get no events
        m.publish(summary());
        let rec = Recorder::default();
        assert_eq!(m.flush(&rec).delivered, 0);
    }

    #[test]
    fn expire_drops_entries() {
        let (m, clock) = mgr();
        do_subscribe(&m, OK, "<e:Expires>PT5S</e:Expires>");
        assert_eq!(m.expire(), 0);
        clock.advance(Duration::from_secs(6));
        assert_eq!(m.expire(), 1);
    }

    #[test]
    fn faults_for_bad_requests() {
        let (m, _) = mgr();
        let sub = PrintOp::Subscribe.action().expect("a");
        let f = |body: String| fault_sub(&m.handle(&request(sub, "", &body)));
        assert_eq!(
            f(subscribe_xml(OK, "", r#"Mode="urn:pull""#)),
            "DeliveryModeRequestedUnavailable"
        );
        assert_eq!(
            f(subscribe_xml(OK, "<e:Expires>PT0S</e:Expires>", "")),
            "InvalidExpirationTime"
        );
        assert_eq!(
            f(subscribe_xml(OK, "<e:Expires>-PT5S</e:Expires>", "")),
            "InvalidExpirationTime"
        );
        assert_eq!(
            f(subscribe_xml(
                OK,
                "<e:Expires>2001-01-01T00:00:00Z</e:Expires>",
                ""
            )),
            "InvalidExpirationTime"
        );
        assert_eq!(
            f(subscribe_xml(OK, "<e:Expires>soon</e:Expires>", "")),
            "UnsupportedExpirationType"
        );
        assert_eq!(
            f(subscribe_xml(
                OK,
                r#"<e:Filter Dialect="http://www.w3.org/TR/1999/REC-xpath-19991116">//x</e:Filter>"#,
                ""
            )),
            "FilteringRequestedUnavailable"
        );
        assert_eq!(
            f(subscribe_xml(
                OK,
                &format!(r#"<e:Filter Dialect="{DIALECT_ACTION}"> </e:Filter>"#),
                ""
            )),
            "InvalidMessage"
        );
        assert_eq!(f("<e:Subscribe/>".into()), "InvalidMessage");
        assert_eq!(f("<e:Renew/>".into()), "InvalidMessage");
        assert_eq!(
            f(subscribe_xml(
                OK,
                "<e:Expires>PT1H</e:Expires><e:Expires>PT1H</e:Expires>",
                ""
            )),
            "InvalidMessage"
        );
        // management operations need the identifier
        let g = m.handle(&request(
            PrintOp::GetStatus.action().expect("a"),
            "",
            "<e:GetStatus/>",
        ));
        assert_eq!(fault_sub(&g), "InvalidMessage");
        // not an eventing action
        let w = m.handle(&request("urn:other", "", "<e:Subscribe/>"));
        let body = w.body.as_ref().expect("b");
        assert_eq!(
            body.child(ns::SOAP12, "Code")
                .expect("c")
                .child(ns::SOAP12, "Subcode")
                .expect("s")
                .child(ns::SOAP12, "Value")
                .expect("v")
                .text_qname()
                .expect("q")
                .local,
            "ActionNotSupported"
        );
        assert_eq!(m.subscription_count(), 0);
    }

    #[test]
    fn hostile_notify_to_is_rejected() {
        let (m, _) = mgr();
        for bad in [
            "http://8.8.8.8/x",
            "http://example.com/x",
            "https://127.0.0.1/x",
            "http://user:pw@127.0.0.1/x",
            "http://127.0.0.1:0/x",
            "http://0.0.0.0/x",
            "http://255.255.255.255/x",
            "http://239.255.255.250:3702/",
            "http://169.254.169.254/latest",
            "http://[::ffff:8.8.8.8]/x",
            "http://[2001:db8::1]/x",
            "http://127.0.0.1:99999/",
            "ftp://127.0.0.1/",
            "http://127.0.0.1/a b",
            "http://127.0.0.1/a#frag",
            "http://127.0.0.1\u{7f}/",
            "http:///x",
            "",
        ] {
            let r = do_subscribe(&m, bad, "");
            assert_eq!(fault_sub(&r), "InvalidMessage", "{bad:?} must be rejected");
        }
        assert_eq!(m.subscription_count(), 0);
        // an oversized address
        let long = format!("http://127.0.0.1/{}", "a".repeat(3000));
        assert_eq!(fault_sub(&do_subscribe(&m, &long, "")), "InvalidMessage");
        // EndTo is held to the same rule
        let body = subscribe_xml(
            OK,
            "<e:EndTo><a:Address>http://8.8.8.8/end</a:Address></e:EndTo>",
            "",
        );
        assert_eq!(
            fault_sub(&m.handle(&request(PrintOp::Subscribe.action().expect("a"), "", &body))),
            "InvalidMessage"
        );
    }

    #[test]
    fn good_targets_parse() {
        for ok in [
            "http://127.0.0.1:5357/x",
            "http://192.168.1.70:5357/uuid",
            "http://10.0.0.2/",
            "http://172.16.5.5:8080/a?b=c",
            "http://[::1]:5357/x",
            "http://[fe80::1]/x",
            "http://[fd00::5]:80/x",
            "http://[::ffff:192.168.0.1]/x",
            "HTTP://127.0.0.1/x",
        ] {
            assert!(parse_target(ok, AddressPolicy::PrivateOnly).is_ok(), "{ok}");
        }
        let t = parse_target("http://192.168.1.70", AddressPolicy::PrivateOnly).expect("t");
        assert_eq!((t.port, t.path.as_str()), (80, "/"));
        assert!(parse_target("http://printer.local:80/x", AddressPolicy::AllowAny).is_ok());
        assert!(parse_target("http://8.8.8.8/x", AddressPolicy::AllowAny).is_ok());
        assert!(parse_target("http://0.0.0.0/x", AddressPolicy::AllowAny).is_err());
    }

    #[test]
    fn limits_bound_subscriptions() {
        let clock = Arc::new(ManualClock::new());
        let mut cfg = EventingConfig::new(SVC);
        cfg.max_per_target = 2;
        cfg.max_subscriptions = 3;
        cfg.max_subscribes_per_window = 4;
        let m = EventManager::new(cfg, clock.clone());
        for i in 0..2 {
            let url = format!("http://127.0.0.1:5357/s{i}");
            assert!(sub_id(&do_subscribe(&m, &url, "")).starts_with("uuid:"));
        }
        assert_eq!(
            fault_sub(&do_subscribe(&m, "http://127.0.0.1:5357/s9", "")),
            "EventSourceUnableToProcess"
        );
        sub_id(&do_subscribe(&m, "http://127.0.0.1:6000/a", ""));
        assert_eq!(
            fault_sub(&do_subscribe(&m, "http://127.0.0.1:6001/a", "")),
            "EventSourceUnableToProcess"
        ); // table full
           // rate window: 3 accepted so far, 1 more allowed, then refused until the window passes
        m.cancel(&m.subscriptions()[0].id.clone(), "x");
        sub_id(&do_subscribe(&m, "http://127.0.0.1:6002/a", ""));
        m.cancel(&m.subscriptions()[0].id.clone(), "x");
        assert_eq!(
            fault_sub(&do_subscribe(&m, "http://127.0.0.1:6003/a", "")),
            "EventSourceUnableToProcess"
        );
        clock.advance(Duration::from_secs(11));
        sub_id(&do_subscribe(&m, "http://127.0.0.1:6003/a", ""));
    }

    #[test]
    fn repeat_subscribe_replaces() {
        let (m, _) = mgr();
        do_subscribe(&m, OK, "");
        do_subscribe(&m, OK, "");
        assert_eq!(m.subscription_count(), 1);
    }

    #[test]
    fn filter_by_action_and_category() {
        let (m, _) = mgr();
        let summary_action = summary().action();
        let end_action = PrintEvent::JobEndState {
            job: job(1, JobState::Completed),
        }
        .action();
        let f = format!(
            r#"<e:Filter Dialect="{DIALECT_ACTION}">{end_action} urn:unknown:x</e:Filter>"#
        );
        do_subscribe(&m, OK, &f);
        m.subscribe_direct(
            EndpointRef::new("http://127.0.0.1:5357/cat"),
            None,
            Filter::Category(EventCategory::CompleteState),
            Duration::from_secs(60),
        )
        .expect("direct");
        m.publish(summary());
        m.publish(PrintEvent::JobEndState {
            job: job(1, JobState::Completed),
        });
        let rec = Recorder::default();
        m.flush(&rec);
        let sent = rec.sent.lock().expect("lock").clone();
        assert_eq!(sent.len(), 2);
        assert!(sent.contains(&(OK.to_string(), end_action.to_string())));
        assert!(sent.contains(&(
            "http://127.0.0.1:5357/cat".to_string(),
            summary_action.to_string()
        )));
    }

    #[test]
    fn cumulative_events_keep_order_and_all() {
        let (m, _) = mgr();
        do_subscribe(&m, OK, "");
        let cond = DeviceCondition {
            id: 1,
            time: "t".into(),
            name: "n".into(),
            component: "c".into(),
            severity: Severity::Warning,
        };
        m.publish(PrintEvent::PrinterStatusCondition {
            condition: cond.clone(),
        });
        m.publish(PrintEvent::PrinterStatusConditionCleared {
            condition_id: 1,
            cleared: "t".into(),
        });
        m.publish(PrintEvent::PrinterStatusCondition {
            condition: DeviceCondition { id: 2, ..cond },
        });
        let rec = Recorder::default();
        assert_eq!(m.flush(&rec).delivered, 3);
        let acts = rec.actions();
        assert!(acts[0].ends_with("PrinterStatusConditionEvent"));
        assert!(acts[1].ends_with("PrinterStatusConditionClearedEvent"));
        assert!(acts[2].ends_with("PrinterStatusConditionEvent"));
    }

    #[test]
    fn complete_state_coalesces_and_obeys_rate() {
        let (m, clock) = mgr(); // rate 5 s
        do_subscribe(&m, OK, "");
        let status = |state| PrintEvent::PrinterStatusSummary {
            state,
            primary_reason: None,
            reasons: vec![],
        };
        m.publish(status(PrinterState::Idle));
        m.publish(status(PrinterState::Processing));
        m.publish(status(PrinterState::Stopped));
        let rec = Recorder::default();
        assert_eq!(
            m.flush(&rec).delivered,
            1,
            "three summaries coalesce to the newest"
        );
        // a new one inside the rate window waits
        m.publish(status(PrinterState::Idle));
        assert_eq!(m.flush(&rec).delivered, 0);
        assert_eq!(m.subscriptions()[0].queued, 1);
        clock.advance(Duration::from_secs(5));
        assert_eq!(m.flush(&rec).delivered, 1);
        // cumulative events are not held back by the rate
        m.publish(status(PrinterState::Idle));
        m.publish(PrintEvent::PrinterStatusConditionCleared {
            condition_id: 9,
            cleared: "t".into(),
        });
        assert_eq!(m.flush(&rec).delivered, 1);
        assert!(rec
            .actions()
            .last()
            .expect("last")
            .ends_with("ConditionClearedEvent"));
    }

    #[test]
    fn job_status_is_per_job_and_end_supersedes() {
        let (m, _) = mgr();
        do_subscribe(&m, OK, "");
        m.publish(PrintEvent::JobStatus {
            job: job(1, JobState::Pending),
        });
        m.publish(PrintEvent::JobStatus {
            job: job(2, JobState::Pending),
        });
        m.publish(PrintEvent::JobStatus {
            job: job(1, JobState::Processing),
        });
        assert_eq!(m.subscriptions()[0].queued, 2);
        m.publish(PrintEvent::JobEndState {
            job: job(1, JobState::Completed),
        });
        // job 1's status is gone; job 2's status and the end event remain
        assert_eq!(m.subscriptions()[0].queued, 2);
    }

    #[test]
    fn set_event_rate_bounds() {
        let (m, _) = mgr();
        assert!(m.set_event_rate(0).is_err());
        assert!(m.set_event_rate(601).is_err());
        assert!(m.set_event_rate(30).is_ok());
        assert_eq!(m.event_rate(), Duration::from_secs(30));
    }

    #[test]
    fn delivery_failure_requeues_in_order_then_ends_with_subscription_end() {
        let (m, clock) = mgr();
        let body = subscribe_xml(
            OK,
            "<e:EndTo><a:Address>http://127.0.0.1:5357/end</a:Address></e:EndTo>",
            "",
        );
        m.handle(&request(PrintOp::Subscribe.action().expect("a"), "", &body));
        m.publish(PrintEvent::PrinterStatusConditionCleared {
            condition_id: 1,
            cleared: "a".into(),
        });
        m.publish(PrintEvent::PrinterStatusConditionCleared {
            condition_id: 2,
            cleared: "b".into(),
        });
        let rec = Recorder::default();
        rec.set_fail(true);
        let r = m.flush(&rec);
        assert_eq!((r.delivered, r.failed), (0, 1));
        assert_eq!(m.subscriptions()[0].queued, 2, "nothing is lost");
        // backoff: an immediate retry does nothing
        assert_eq!(m.flush(&rec).failed, 0);
        clock.advance(Duration::from_secs(2));
        m.flush(&rec);
        clock.advance(Duration::from_secs(5));
        rec.set_fail(false);
        // third failure would end it; we recovered before that, so order is preserved
        assert_eq!(m.flush(&rec).delivered, 2);
        assert_eq!(m.subscriptions()[0].failures, 0);
        // now fail until the subscription is ended
        rec.set_fail(true);
        m.publish(PrintEvent::PrinterStatusConditionCleared {
            condition_id: 3,
            cleared: "c".into(),
        });
        for _ in 0..4 {
            clock.advance(Duration::from_secs(40));
            m.flush(&rec);
        }
        assert_eq!(m.subscription_count(), 0);
        rec.set_fail(false);
        let sent = rec.sent.lock().expect("lock").clone();
        let end = sent.last().expect("end");
        assert_eq!(end.0, "http://127.0.0.1:5357/end");
        assert_eq!(end.1, ACTION_SUBSCRIPTION_END);
    }

    #[test]
    fn subscription_end_message_shape_and_shutdown() {
        let (m, _) = mgr();
        let body = subscribe_xml(
            OK,
            "<e:EndTo><a:Address>http://127.0.0.1:5357/end</a:Address></e:EndTo>",
            "",
        );
        let id = sub_id(&m.handle(&request(PrintOp::Subscribe.action().expect("a"), "", &body)));
        // a plain subscription without EndTo produces no message
        do_subscribe(&m, "http://127.0.0.1:5358/x", "");
        m.shutdown();
        assert_eq!(m.subscription_count(), 0);
        let captured: Mutex<Vec<SoapReply>> = Mutex::new(Vec::new());
        struct Cap<'a>(&'a Mutex<Vec<SoapReply>>);
        impl Deliver for Cap<'_> {
            fn deliver(&self, _t: &EndpointRef, m: &SoapReply) -> Result<(), DeliverError> {
                self.0.lock().expect("lock").push(m.clone());
                Ok(())
            }
        }
        assert_eq!(m.flush(&Cap(&captured)).ends_sent, 1);
        let msgs = captured.lock().expect("lock");
        let el = msgs[0].body.as_ref().expect("b");
        assert_eq!(el.name, QName::new(ns::WSE, "SubscriptionEnd"));
        let kids: Vec<&str> = el.children.iter().map(|c| c.name.local.as_str()).collect();
        assert_eq!(kids, ["SubscriptionManager", "Status", "Reason"]);
        assert_eq!(
            el.child(ns::WSE, "Status").expect("s").text,
            STATUS_SOURCE_SHUTTING_DOWN
        );
        let ident = el
            .child(ns::WSE, "SubscriptionManager")
            .expect("m")
            .child(ns::WSA, "ReferenceParameters")
            .expect("p")
            .child(ns::WSE, "Identifier")
            .expect("i");
        assert_eq!(ident.text, id);
        assert_eq!(msgs[0].to, "http://127.0.0.1:5357/end");
    }

    #[test]
    fn unsubscribe_sends_no_subscription_end() {
        let (m, _) = mgr();
        let body = subscribe_xml(
            OK,
            "<e:EndTo><a:Address>http://127.0.0.1:5357/end</a:Address></e:EndTo>",
            "",
        );
        let id = sub_id(&m.handle(&request(PrintOp::Subscribe.action().expect("a"), "", &body)));
        m.handle(&request(
            PrintOp::Unsubscribe.action().expect("a"),
            &id_header(&id),
            "<e:Unsubscribe/>",
        ));
        let rec = Recorder::default();
        assert_eq!(m.flush(&rec).ends_sent, 0);
    }

    #[test]
    fn queue_overflow_ends_subscription_instead_of_dropping() {
        let clock = Arc::new(ManualClock::new());
        let mut cfg = EventingConfig::new(SVC);
        cfg.max_queue = 3;
        let m = EventManager::new(cfg, clock);
        do_subscribe(&m, OK, "");
        for i in 1..=4 {
            m.publish(PrintEvent::PrinterStatusConditionCleared {
                condition_id: i,
                cleared: "t".into(),
            });
        }
        assert_eq!(m.subscription_count(), 0);
    }

    #[test]
    fn notifications_carry_notify_to_reference_parameters() {
        let (m, _) = mgr();
        do_subscribe(&m, OK, "");
        m.publish(summary());
        let captured: Mutex<Vec<SoapReply>> = Mutex::new(Vec::new());
        struct Cap<'a>(&'a Mutex<Vec<SoapReply>>);
        impl Deliver for Cap<'_> {
            fn deliver(&self, _t: &EndpointRef, m: &SoapReply) -> Result<(), DeliverError> {
                self.0.lock().expect("lock").push(m.clone());
                Ok(())
            }
        }
        m.flush(&Cap(&captured));
        let msgs = captured.lock().expect("lock");
        assert_eq!(msgs[0].to, OK);
        assert_eq!(msgs[0].headers[0].name, QName::new("urn:x", "Ctx"));
    }

    #[test]
    fn duration_and_datetime_parsing() {
        assert_eq!(
            parse_duration("PT1H30M"),
            Ok(Some(Duration::from_secs(5400)))
        );
        assert_eq!(
            parse_duration("P1DT1S"),
            Ok(Some(Duration::from_secs(86_401)))
        );
        assert_eq!(
            parse_duration("PT0.5S"),
            Ok(Some(Duration::from_millis(500)))
        );
        assert_eq!(parse_duration("-PT1S"), Ok(None));
        for bad in [
            "",
            "P",
            "PT",
            "1H",
            "PT1X",
            "PT1M1H",
            "P1.5D",
            "PTH",
            "PT1..5S",
            "P99999999999Y",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(format_duration(Duration::from_secs(3661)), "PT1H1M1S");
        assert_eq!(format_duration(Duration::ZERO), "PT0S");
        let t = parse_datetime("2026-10-13T01:02:03Z").expect("dt");
        assert_eq!(format_datetime(t), "2026-10-13T01:02:03Z");
        assert_eq!(parse_datetime("2026-10-13T03:02:03+02:00"), Some(t));
        assert_eq!(parse_datetime("2026-10-13T01:02:03.250Z"), Some(t));
        for bad in [
            "",
            "2026-13-01T00:00:00Z",
            "2026-10-13 01:02:03Z",
            "2026-10-13T01:02:03",
            "garbage-garbage-garbage",
        ] {
            assert!(parse_datetime(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn expires_are_clamped() {
        let clock = Arc::new(ManualClock::new());
        let mut cfg = EventingConfig::new(SVC);
        cfg.max_expires = Duration::from_secs(120);
        let m = EventManager::new(cfg, clock);
        let r = do_subscribe(&m, OK, "<e:Expires>P30D</e:Expires>");
        assert_eq!(
            r.body
                .as_ref()
                .expect("b")
                .child(ns::WSE, "Expires")
                .expect("e")
                .text,
            "PT2M"
        );
    }

    #[test]
    fn http_deliver_posts_to_loopback_sink() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            s.set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let mut data = Vec::new();
            let mut buf = [0u8; 2048];
            loop {
                let n = s.read(&mut buf).expect("read");
                data.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&data).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len: usize = text[..i]
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if data.len() >= i + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            s.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                .expect("write");
            String::from_utf8_lossy(&data).to_string()
        });
        let target = EndpointRef::new(format!("http://127.0.0.1:{port}/events"));
        let ev = summary();
        let msg = events::notification(&target, &ev);
        HttpDeliver::default()
            .deliver(&target, &msg)
            .expect("delivered");
        let got = handle.join().expect("join");
        assert!(got.starts_with("POST /events HTTP/1.1\r\n"), "{got}");
        assert!(got.contains("application/soap+xml"));
        assert!(got.contains(ev.action()));
        assert!(got.contains("PrinterStatusSummaryEvent"));
    }

    #[test]
    fn http_deliver_refuses_public_targets_without_connecting() {
        let target = EndpointRef::new("http://8.8.8.8/x");
        let msg = events::notification(&target, &summary());
        assert!(HttpDeliver::default().deliver(&target, &msg).is_err());
    }

    #[test]
    fn http_deliver_reports_error_status() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.1 500 Oops\r\n\r\n").expect("write");
        });
        let target = EndpointRef::new(format!("http://127.0.0.1:{port}/"));
        let msg = events::notification(&target, &summary());
        let err = HttpDeliver::default()
            .deliver(&target, &msg)
            .expect_err("500");
        assert!(err.0.contains("500"));
        h.join().expect("join");
    }

    #[tokio::test]
    async fn pump_flushes_and_shuts_down() {
        let (m, _) = mgr();
        let m = Arc::new(m);
        do_subscribe(
            &m,
            OK,
            "<e:EndTo><a:Address>http://127.0.0.1:5357/end</a:Address></e:EndTo>",
        );
        m.publish(PrintEvent::PrinterStatusConditionCleared {
            condition_id: 1,
            cleared: "t".into(),
        });
        let rec = Arc::new(Recorder::default());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_pump(
            Arc::clone(&m),
            rec.clone(),
            Duration::from_millis(10),
            rx,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = tx.send(());
        task.await.expect("pump");
        let acts = rec.actions();
        assert!(acts.iter().any(|a| a.ends_with("ConditionClearedEvent")));
        assert!(acts.iter().any(|a| a == ACTION_SUBSCRIPTION_END));
    }
}
