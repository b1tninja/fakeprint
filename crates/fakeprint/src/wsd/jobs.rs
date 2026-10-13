//! The job table, the job state machine and the live printer status.
//!
//! [`JobEngine`] implements [`JobStore`] and [`StatusSource`].
//! Timers are evaluated lazily against the injected [`Clock`]: every public method first runs
//! [`JobEngine::tick`], so a test only advances a `ManualClock` and calls any method. A
//! background task (see `ctx::spawn_ticker`) calls `tick` so events also fire on idle servers.
//!
//! Life cycle (see `docs/reference/operations.md`):
//!
//! ```text
//! create            -> Pending  [JobIncoming], 60 s document clock starts
//! first document    -> Processing [JobPrinting (+JobIncoming until LastDocument)]
//! printing finished -> Completed [JobCompletedSuccessfully | ...WithWarnings]
//! CancelJob         -> Terminating -> Canceled [JobCanceledByUser | JobCanceledAtDevice]
//! document timeout  -> Aborted [DocumentTimeoutError]
//! stopping condition-> ProcessingStopped [PrinterStopped]; cleared => Processing again
//! ```
//!
//! Events are collected while the lock is held and published after it is released.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime};

use super::ctx::{
    format_datetime, Clock, ConditionRecord, DocumentFailure, ExtraCondition, FaultBody, FaultPlan,
    PrintCondition, PrintLimits, PrintTiming, PrinterStatusSnapshot, SharedBus, StatusSource,
};
use super::fault::{FaultKind, PrintFault};
use super::printer_model::{default_group, default_severity, parse_severity, state_reason_for};
use super::types::{
    DeviceCondition, EventBus, JobDocument, JobState, JobStateReason, JobStore, NewJob, PrintEvent,
    PrintJob, PrinterState, Severity,
};
use super::xml::Element;

/// First `DeviceCondition/@Id` of a condition raised by name.
const FIRST_EXTRA_ID: u32 = 100;

/// Largest job id the printer issues (`2^31 - 1`).
pub const MAX_JOB_ID: u32 = 0x7FFF_FFFF;

/// Whether the job state machine allows `from -> to`.
///
/// Terminal states have no exits. `Terminating` may still end as `Completed` (what was already
/// printed finished before the cancel took effect) or `Aborted`.
pub fn transition_allowed(from: JobState, to: JobState) -> bool {
    use JobState::*;
    match from {
        Pending => matches!(
            to,
            PendingHeld | Started | Processing | Terminating | Aborted
        ),
        PendingHeld => matches!(to, Pending | Terminating | Aborted),
        Started => matches!(to, Processing | Terminating | Aborted),
        Processing => matches!(to, ProcessingStopped | Terminating | Completed | Aborted),
        ProcessingStopped => matches!(to, Processing | Terminating | Aborted),
        Terminating => matches!(to, Canceled | Completed | Aborted),
        Completed | Canceled | Aborted => false,
    }
}

/// Who cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelSource {
    /// `CancelJob` from a client (`JobCanceledByUser`).
    User,
    /// The device panel (`JobCanceledAtDevice`).
    Device,
}

/// A document handed to [`JobEngine::commit_document`].
#[derive(Debug, Clone, Default)]
pub struct DocumentIn {
    pub name: Option<String>,
    pub format: String,
    pub compression: String,
    pub size: u64,
    pub url: Option<String>,
    pub capture_path: Option<PathBuf>,
    pub last: bool,
}

#[derive(Debug, Clone, Default)]
struct Meta {
    copies: u32,
    /// Abort the job if no further document arrives by then.
    doc_deadline: Option<SystemTime>,
    /// `Terminating` ends at this time.
    terminate_at: Option<SystemTime>,
    terminate_source: Option<bool>, // Some(true) = device
    busy_until: Option<SystemTime>,
    /// Printing window of everything received so far.
    proc_start: Option<SystemTime>,
    proc_end: Option<SystemTime>,
    paused_at: Option<SystemTime>,
    total_octets: u64,
    total_sheets: u32,
    fail: Option<DocumentFailure>,
}

#[derive(Debug, Clone)]
struct Entry {
    job: PrintJob,
    meta: Meta,
}

struct Inner {
    next_id: u32,
    jobs: BTreeMap<u32, Entry>,
    /// Terminal job ids, oldest first.
    history: VecDeque<u32>,
    faults: FaultPlan,
    conditions: Vec<ConditionRecord>,
    /// Next id for a condition raised by name (the fixed conditions use 1..=8).
    next_extra: u32,
    last_summary: (PrinterState, Vec<String>),
}

/// The in-memory job table. See the module documentation.
pub struct JobEngine {
    limits: PrintLimits,
    timing: RwLock<PrintTiming>,
    clock: Arc<dyn Clock>,
    bus: Arc<SharedBus>,
    event_rate: AtomicU32,
    inner: Mutex<Inner>,
}

type Events = Vec<PrintEvent>;

impl JobEngine {
    pub fn new(
        limits: PrintLimits,
        timing: PrintTiming,
        clock: Arc<dyn Clock>,
        bus: Arc<SharedBus>,
    ) -> Self {
        Self {
            limits,
            timing: RwLock::new(timing),
            clock,
            bus,
            event_rate: AtomicU32::new(30),
            inner: Mutex::new(Inner {
                next_id: 1,
                jobs: BTreeMap::new(),
                history: VecDeque::new(),
                faults: FaultPlan::default(),
                conditions: Vec::new(),
                next_extra: FIRST_EXTRA_ID,
                last_summary: (PrinterState::Idle, vec!["None".to_string()]),
            }),
        }
    }

    /// The timers in force.
    pub fn timing(&self) -> PrintTiming {
        match self.timing.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Replace the timers. Jobs already running keep the deadlines computed when their
    /// document arrived; everything that starts later uses the new values.
    pub fn set_timing(&self, timing: PrintTiming) {
        match self.timing.write() {
            Ok(mut g) => *g = timing,
            Err(p) => *p.into_inner() = timing,
        }
    }

    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    pub fn event_rate(&self) -> u32 {
        self.event_rate.load(Ordering::Relaxed)
    }

    /// Store a `SetEventRate` value (the operation validates 1..=600).
    pub fn set_event_rate(&self, secs: u32) {
        self.event_rate.store(secs, Ordering::Relaxed);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Run `f` with the lock held after due timers ran; publish the collected events (and a
    /// status-summary event when the summary changed) after the lock is released.
    fn with<R>(&self, f: impl FnOnce(&mut Inner, SystemTime, &mut Events) -> R) -> R {
        let now = self.clock.now();
        let timing = self.timing();
        let mut events: Events = Vec::new();
        let out = {
            let mut g = self.lock();
            run_timers(&mut g, &timing, &self.limits, now, &mut events);
            let r = f(&mut g, now, &mut events);
            summary_event(&mut g, &mut events);
            r
        };
        for e in events {
            self.bus.publish(e);
        }
        out
    }

    /// Run due timers.
    pub fn tick(&self) {
        self.with(|_, _, _| ());
    }

    // ----- faults -------------------------------------------------------------------------

    pub fn faults(&self) -> FaultPlan {
        self.lock().faults.clone()
    }

    /// Replace the whole plan (conditions are raised/cleared to match).
    pub fn set_fault_plan(&self, plan: FaultPlan) {
        self.with(|g, now, ev| {
            for c in PrintCondition::ALL {
                set_condition(g, &self.limits, now, c, plan.conditions.contains(&c), ev);
            }
            g.faults = plan;
            reconcile_stop(g, now, ev);
        });
    }

    pub fn apply_faults(&self, body: &FaultBody) {
        self.with(|g, now, ev| {
            if body.reset {
                for c in PrintCondition::ALL {
                    set_condition(g, &self.limits, now, c, false, ev);
                }
                let ids: Vec<u32> = g.faults.extra.iter().map(|c| c.id).collect();
                for id in ids {
                    clear_extra(g, now, id, ev);
                }
                g.faults = FaultPlan::default();
            }
            let pairs = [
                (PrintCondition::PaperOut, body.paper_out),
                (PrintCondition::PaperLow, body.paper_low),
                (PrintCondition::TonerLow, body.toner_low),
                (PrintCondition::TonerEmpty, body.toner_empty),
                (PrintCondition::CoverOpen, body.cover_open),
                (PrintCondition::PaperJam, body.paper_jam),
                (PrintCondition::OutputFull, body.output_full),
                (PrintCondition::Error, body.error),
            ];
            for (c, v) in pairs {
                if let Some(on) = v {
                    set_condition(g, &self.limits, now, c, on, ev);
                }
            }
            if let Some(v) = body.not_accepting_jobs {
                g.faults.not_accepting_jobs = v;
            }
            if let Some(v) = body.temporary_error {
                g.faults.temporary_error = v;
            }
            if let Some(v) = body.internal_error {
                g.faults.internal_error = v;
            }
            if let Some(v) = body.document_active {
                g.faults.document_active = v;
            }
            if let Some(v) = &body.fail_next_document {
                g.faults.fail_next_document = DocumentFailure::parse(v).ok().flatten();
            }
            for name in &body.clear {
                clear_named(g, &self.limits, now, name, ev);
            }
            for spec in &body.raise {
                let severity = spec.severity.as_deref().and_then(parse_severity);
                raise_extra(
                    g,
                    &self.limits,
                    now,
                    &spec.name,
                    spec.component.as_deref(),
                    severity,
                    ev,
                );
            }
            reconcile_stop(g, now, ev);
        });
    }

    /// Raise a condition by name; the printer state, jobs and events follow. `component` is
    /// `Group` or `Group/Name` (absent: derived from the name), `severity` absent: the usual
    /// one. `None` when too many conditions are already raised.
    pub fn raise_condition(
        &self,
        name: &str,
        component: Option<&str>,
        severity: Option<Severity>,
    ) -> Option<DeviceCondition> {
        self.with(|g, now, ev| {
            let c = raise_extra(g, &self.limits, now, name, component, severity, ev);
            reconcile_stop(g, now, ev);
            c
        })
    }

    /// Clear one raised condition by id (fixed or by-name); false when it is not active.
    pub fn clear_condition(&self, id: u32) -> bool {
        self.with(|g, now, ev| {
            let done = if let Some(c) = PrintCondition::ALL.iter().copied().find(|c| c.id() == id) {
                let was = g.faults.conditions.contains(&c);
                set_condition(g, &self.limits, now, c, false, ev);
                was
            } else {
                clear_extra(g, now, id, ev)
            };
            reconcile_stop(g, now, ev);
            done
        })
    }

    /// Whether `CreatePrintJob` is currently accepted (the lab can switch it off).
    pub fn status_accepting(&self) -> bool {
        !self.lock().faults.not_accepting_jobs
    }

    /// `ServerErrorTemporaryError` / `ServerErrorInternalError` when the lab switched them on.
    pub fn check_service_faults(&self) -> Result<(), PrintFault> {
        let f = self.lock().faults.clone();
        if f.internal_error {
            return Err(PrintFault::new(FaultKind::ServerErrorInternalError));
        }
        if f.temporary_error {
            return Err(PrintFault::new(FaultKind::ServerErrorTemporaryError));
        }
        Ok(())
    }

    // ----- creation -----------------------------------------------------------------------

    /// Create a job (ticket already validated). `copies` scales the simulated sheet count.
    pub fn create_job(&self, new: NewJob, copies: u32) -> Result<u32, PrintFault> {
        let timing = self.timing();
        self.with(|g, now, ev| {
            if g.faults.internal_error {
                return Err(PrintFault::new(FaultKind::ServerErrorInternalError));
            }
            if g.faults.temporary_error {
                return Err(PrintFault::new(FaultKind::ServerErrorTemporaryError));
            }
            if g.faults.not_accepting_jobs {
                return Err(PrintFault::new(FaultKind::ServerErrorNotAcceptingJobs));
            }
            let active = g
                .jobs
                .values()
                .filter(|e| !e.job.state.is_terminal())
                .count();
            if active >= self.limits.max_active_jobs {
                return Err(PrintFault::new(FaultKind::ServerErrorNotAcceptingJobs)
                    .with_reason("too many active jobs"));
            }
            let id = alloc_id(g)?;
            let mut job = PrintJob::new(id, new.name, new.user);
            job.created = now;
            job.ticket = new.ticket;
            let meta = Meta {
                copies: copies.max(1),
                doc_deadline: Some(now + timing.document_timeout),
                ..Meta::default()
            };
            ev.push(PrintEvent::JobStatus { job: job.clone() });
            g.jobs.insert(id, Entry { job, meta });
            Ok(id)
        })
    }

    // ----- documents ----------------------------------------------------------------------

    /// State checks of `SendDocument`/`AddDocument` in the order of `faults.md` (everything
    /// before the Format check).
    pub fn precheck_document(&self, id: u32, last: bool, multi: bool) -> Result<(), PrintFault> {
        self.with(|g, now, _| precheck(g, &self.limits, now, id, last, multi))
    }

    /// Record a rejected document (bad Format or Compression): the job stays alive and shows
    /// the reason until the document clock aborts it or a good document arrives.
    pub fn note_rejected_document(&self, id: u32, reason: JobStateReason) {
        self.with(|g, _, ev| {
            if let Some(e) = g.jobs.get_mut(&id) {
                if e.job.state == JobState::Pending && !e.job.reasons.contains(&reason) {
                    e.job.reasons.push(reason);
                    ev.push(PrintEvent::JobStatus { job: e.job.clone() });
                }
            }
        });
    }

    /// Accept a document: re-checks the state, then updates the job and starts printing.
    /// Returns the updated job and the document's 1-based id.
    pub fn commit_document(
        &self,
        id: u32,
        doc: DocumentIn,
        multi: bool,
    ) -> Result<(PrintJob, u32), PrintFault> {
        let timing = self.timing();
        self.with(|g, now, ev| {
            precheck(g, &self.limits, now, id, doc.last, multi)?;
            let stopped = stopping(g);
            let fail = g.faults.fail_next_document.take();
            let e = g
                .jobs
                .get_mut(&id)
                .ok_or_else(|| PrintFault::job_id_not_found(id))?;
            let doc_id = e.job.number_of_documents().saturating_add(1);
            e.job.documents.push(JobDocument {
                id: doc_id,
                name: doc.name,
                format: doc.format,
                compression: doc.compression,
                size: doc.size,
                url: doc.url,
                capture_path: doc.capture_path,
            });
            e.job.last_document_received |= doc.last;
            e.meta.total_octets = e.meta.total_octets.saturating_add(doc.size);
            e.meta.total_sheets = sheets_for(e.meta.total_octets, e.meta.copies, &timing);
            e.meta.doc_deadline = None;
            e.meta.busy_until = if timing.document_busy.is_zero() {
                None
            } else {
                Some(now + timing.document_busy)
            };
            if fail.is_some() {
                e.meta.fail = fail;
            }
            let dur = timing.processing_base + scale(timing.processing_per_mib, doc.size, 1 << 20);
            let start = e.meta.proc_end.filter(|t| *t > now).unwrap_or(now);
            e.meta.proc_start.get_or_insert(now);
            e.meta.proc_end = Some(start + dur);
            e.job.reasons.retain(|r| {
                !matches!(
                    r,
                    JobStateReason::UnsupportedDocumentFormat
                        | JobStateReason::UnsupportedCompression
                )
            });
            let mut moved = false;
            if e.job.state == JobState::Pending {
                let r = printing_reasons(&e.job);
                moved |= go(&mut e.job, JobState::Processing, r, now, ev);
            } else {
                let r = if e.job.state == JobState::ProcessingStopped {
                    vec![JobStateReason::PrinterStopped]
                } else {
                    printing_reasons(&e.job)
                };
                if e.job.reasons != r {
                    e.job.reasons = r;
                    moved = true;
                }
            }
            if stopped && e.job.state == JobState::Processing {
                e.meta.paused_at = Some(now);
                moved |= go(
                    &mut e.job,
                    JobState::ProcessingStopped,
                    vec![JobStateReason::PrinterStopped],
                    now,
                    ev,
                );
            }
            if !moved {
                ev.push(PrintEvent::JobStatus { job: e.job.clone() });
            }
            Ok((e.job.clone(), doc_id))
        })
    }

    // ----- cancel / end -------------------------------------------------------------------

    /// Cancel a job. Unknown id => `ClientErrorJobIdNotFound`; a remembered finished job =>
    /// `OperationFailed`; an already terminating job succeeds again (idempotent).
    pub fn cancel(&self, id: u32, source: CancelSource) -> Result<PrintJob, PrintFault> {
        let timing = self.timing();
        self.with(|g, now, ev| {
            let e = g
                .jobs
                .get_mut(&id)
                .ok_or_else(|| PrintFault::job_id_not_found(id))?;
            match e.job.state {
                JobState::Terminating => return Ok(e.job.clone()),
                s if s.is_terminal() => {
                    return Err(PrintFault::new(FaultKind::OperationFailed)
                        .with_reason("the job already ended; there is nothing to cancel"))
                }
                _ => {}
            }
            e.meta.paused_at = None;
            e.meta.terminate_source = Some(source == CancelSource::Device);
            e.meta.terminate_at = Some(now + timing.terminating);
            freeze_progress(e, now);
            go(
                &mut e.job,
                JobState::Terminating,
                vec![cancel_reason(source == CancelSource::Device)],
                now,
                ev,
            );
            finish_terminating(g, id, now, ev, &self.limits);
            g.jobs
                .get(&id)
                .map(|e| e.job.clone())
                .ok_or_else(|| PrintFault::job_id_not_found(id))
        })
    }

    /// Finish a job right away (raw/IPP paths where the data is already complete): the job
    /// goes through `Processing` to `Completed` with all counters filled.
    pub fn complete_now(&self, id: u32) -> Option<PrintJob> {
        self.with(|g, now, ev| {
            let e = g.jobs.get_mut(&id)?;
            if e.job.state.is_terminal() {
                return Some(e.job.clone());
            }
            if e.job.state == JobState::Terminating {
                return Some(e.job.clone());
            }
            if e.job.state == JobState::Pending || e.job.state == JobState::PendingHeld {
                go(
                    &mut e.job,
                    JobState::Processing,
                    vec![JobStateReason::JobPrinting],
                    now,
                    ev,
                );
            }
            if e.job.state == JobState::ProcessingStopped {
                go(
                    &mut e.job,
                    JobState::Processing,
                    vec![JobStateReason::JobPrinting],
                    now,
                    ev,
                );
            }
            e.job.last_document_received = true;
            e.meta.paused_at = None;
            e.job.octets_processed = e.meta.total_octets;
            e.job.media_sheets_completed = e.meta.total_sheets;
            complete(g, id, now, ev, &self.limits);
            g.jobs.get(&id).map(|e| e.job.clone())
        })
    }

    /// Drop every job and condition (lab reset). Ids restart at 1.
    pub fn reset(&self) {
        self.with(|g, now, ev| {
            for c in PrintCondition::ALL {
                set_condition(g, &self.limits, now, c, false, ev);
            }
            let ids: Vec<u32> = g.faults.extra.iter().map(|c| c.id).collect();
            for id in ids {
                clear_extra(g, now, id, ev);
            }
            g.jobs.clear();
            g.history.clear();
            g.conditions.clear();
            g.faults = FaultPlan::default();
            g.next_id = 1;
        });
    }

    pub fn queued_jobs(&self) -> usize {
        self.with(|g, _, _| {
            g.jobs
                .values()
                .filter(|e| !e.job.state.is_terminal())
                .count()
        })
    }
}

// ---------------------------------------------------------------------------------------------
// JobStore / StatusSource
// ---------------------------------------------------------------------------------------------

impl JobStore for JobEngine {
    fn create(&self, new: NewJob) -> Result<u32, PrintFault> {
        self.create_job(new, 1)
    }

    fn get(&self, id: u32) -> Option<PrintJob> {
        self.with(|g, _, _| g.jobs.get(&id).map(|e| e.job.clone()))
    }

    fn update(
        &self,
        id: u32,
        change: &mut dyn FnMut(&mut PrintJob),
    ) -> Result<PrintJob, PrintFault> {
        self.with(|g, now, ev| {
            let cap = self.limits.history_cap;
            let e = g
                .jobs
                .get_mut(&id)
                .ok_or_else(|| PrintFault::job_id_not_found(id))?;
            let before = e.job.clone();
            change(&mut e.job);
            if e.job.id != before.id
                || (e.job.state != before.state && !transition_allowed(before.state, e.job.state))
            {
                e.job = before;
                return Err(PrintFault::new(FaultKind::OperationFailed)
                    .with_reason("illegal job state transition"));
            }
            let after = e.job.clone();
            if after.state != before.state && after.state.is_terminal() {
                e.job.ended = Some(now);
                let done = e.job.clone();
                ev.push(PrintEvent::JobEndState { job: done });
                g.history.push_back(id);
                trim_history(g, cap);
            } else if after != before {
                ev.push(PrintEvent::JobStatus { job: after });
            }
            g.jobs
                .get(&id)
                .map(|e| e.job.clone())
                .ok_or_else(|| PrintFault::job_id_not_found(id))
        })
    }

    fn active(&self) -> Vec<PrintJob> {
        self.with(|g, _, _| {
            g.jobs
                .values()
                .filter(|e| !e.job.state.is_terminal())
                .map(|e| e.job.clone())
                .collect()
        })
    }

    fn history(&self) -> Vec<PrintJob> {
        self.with(|g, _, _| {
            g.jobs
                .values()
                .filter(|e| e.job.state.is_terminal())
                .map(|e| e.job.clone())
                .collect()
        })
    }
}

impl StatusSource for JobEngine {
    fn status(&self) -> PrinterStatusSnapshot {
        self.with(|g, now, _| snapshot(g, now))
    }
}

impl EventBus for JobEngine {
    /// Lets callers inject an event as if the engine had produced it.
    fn publish(&self, event: PrintEvent) {
        self.bus.publish(event);
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

fn alloc_id(g: &mut Inner) -> Result<u32, PrintFault> {
    for _ in 0..4 {
        let mut tries = 0u32;
        loop {
            let id = g.next_id;
            g.next_id = if id >= MAX_JOB_ID { 1 } else { id + 1 };
            if !g.jobs.contains_key(&id) {
                return Ok(id);
            }
            tries += 1;
            if tries > 100_000 {
                break;
            }
        }
    }
    Err(PrintFault::new(FaultKind::ServerErrorTemporaryError).with_reason("no free job id"))
}

fn scale(per_unit: Duration, amount: u64, unit: u64) -> Duration {
    let ms = per_unit.as_millis().saturating_mul(u128::from(amount)) / u128::from(unit.max(1));
    Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX))
}

fn sheets_for(octets: u64, copies: u32, t: &PrintTiming) -> u32 {
    let per = octets.div_ceil(t.bytes_per_sheet.max(1)).max(1);
    u32::try_from(per.saturating_mul(u64::from(copies.max(1)))).unwrap_or(u32::MAX)
}

fn stopping(g: &Inner) -> bool {
    g.faults.conditions.iter().any(|c| c.stops_printer())
        || g.faults
            .extra
            .iter()
            .any(|c| c.severity == Severity::Critical)
}

fn printing_reasons(j: &PrintJob) -> Vec<JobStateReason> {
    if j.last_document_received {
        vec![JobStateReason::JobPrinting]
    } else {
        vec![JobStateReason::JobPrinting, JobStateReason::JobIncoming]
    }
}

fn cancel_reason(device: bool) -> JobStateReason {
    if device {
        JobStateReason::JobCanceledAtDevice
    } else {
        JobStateReason::JobCanceledByUser
    }
}

fn precheck(
    g: &Inner,
    limits: &PrintLimits,
    now: SystemTime,
    id: u32,
    last: bool,
    multi: bool,
) -> Result<(), PrintFault> {
    let e = g
        .jobs
        .get(&id)
        .ok_or_else(|| PrintFault::job_id_not_found(id))?;
    match e.job.state {
        JobState::Terminating | JobState::Canceled => {
            return Err(PrintFault::new(FaultKind::ServerErrorJobCancelled))
        }
        _ => {}
    }
    if e.job.last_document_received {
        return Err(PrintFault::new(
            FaultKind::ClientErrorLastDocumentAlreadySent,
        ));
    }
    if e.job.state.is_terminal() {
        return Err(PrintFault::new(FaultKind::OperationFailed)
            .with_reason("the job already ended without receiving its last document"));
    }
    let busy = e.meta.busy_until.is_some_and(|t| t > now);
    if g.faults.document_active || busy {
        return Err(PrintFault::new(FaultKind::ServerErrorDocumentActive));
    }
    if !multi && !last {
        return Err(PrintFault::new(
            FaultKind::ClientErrorMultipleDocumentsNotSupported,
        ));
    }
    if e.job.number_of_documents() >= limits.max_documents_per_job {
        return Err(PrintFault::new(FaultKind::OperationFailed)
            .with_reason("too many documents in this job"));
    }
    Ok(())
}

/// Apply a state change if the machine allows it; emit the event. Returns whether it moved.
fn go(
    job: &mut PrintJob,
    to: JobState,
    reasons: Vec<JobStateReason>,
    now: SystemTime,
    ev: &mut Events,
) -> bool {
    if job.state != to && !transition_allowed(job.state, to) {
        return false;
    }
    let changed = job.state != to || job.reasons != reasons;
    job.state = to;
    job.reasons = reasons;
    if to.is_terminal() {
        job.ended = Some(now);
        ev.push(PrintEvent::JobEndState { job: job.clone() });
    } else if changed {
        ev.push(PrintEvent::JobStatus { job: job.clone() });
    }
    changed
}

fn trim_history(g: &mut Inner, cap: usize) {
    while g.history.len() > cap {
        if let Some(old) = g.history.pop_front() {
            g.jobs.remove(&old);
        }
    }
}

fn mark_ended(g: &mut Inner, id: u32, limits: &PrintLimits) {
    if !g.history.contains(&id) {
        g.history.push_back(id);
    }
    trim_history(g, limits.history_cap);
}

fn complete(g: &mut Inner, id: u32, now: SystemTime, ev: &mut Events, limits: &PrintLimits) {
    let warn = g.faults.conditions.iter().any(|c| !c.stops_printer())
        || g.faults
            .extra
            .iter()
            .any(|c| c.severity != Severity::Critical);
    if let Some(e) = g.jobs.get_mut(&id) {
        let reason = if warn {
            JobStateReason::JobCompletedWithWarnings
        } else {
            JobStateReason::JobCompletedSuccessfully
        };
        if go(&mut e.job, JobState::Completed, vec![reason], now, ev) {
            e.meta = Meta {
                total_octets: e.meta.total_octets,
                total_sheets: e.meta.total_sheets,
                copies: e.meta.copies,
                ..Meta::default()
            };
            mark_ended(g, id, limits);
        }
    }
}

fn abort(
    g: &mut Inner,
    id: u32,
    reason: JobStateReason,
    now: SystemTime,
    ev: &mut Events,
    limits: &PrintLimits,
) {
    if let Some(e) = g.jobs.get_mut(&id) {
        if go(&mut e.job, JobState::Aborted, vec![reason], now, ev) {
            e.meta.doc_deadline = None;
            e.meta.paused_at = None;
            mark_ended(g, id, limits);
        }
    }
}

fn finish_terminating(
    g: &mut Inner,
    id: u32,
    now: SystemTime,
    ev: &mut Events,
    limits: &PrintLimits,
) {
    if let Some(e) = g.jobs.get_mut(&id) {
        if e.job.state == JobState::Terminating && e.meta.terminate_at.is_some_and(|t| t <= now) {
            let device = e.meta.terminate_source.unwrap_or(false);
            if go(
                &mut e.job,
                JobState::Canceled,
                vec![cancel_reason(device)],
                now,
                ev,
            ) {
                e.meta.terminate_at = None;
                mark_ended(g, id, limits);
            }
        }
    }
}

/// Progress of the simulated print run, 0..=1000 (per mille).
fn permille(e: &Entry, now: SystemTime) -> u64 {
    match (e.meta.proc_start, e.meta.proc_end) {
        (Some(s), Some(en)) => {
            let total = en.duration_since(s).unwrap_or_default().as_millis();
            if total == 0 || now >= en {
                return 1000;
            }
            let done = now.duration_since(s).unwrap_or_default().as_millis();
            u64::try_from(done * 1000 / total).unwrap_or(1000).min(1000)
        }
        _ => 0,
    }
}

fn apply_progress(e: &mut Entry, now: SystemTime) -> bool {
    let pm = permille(e, now);
    let octets = e.meta.total_octets.saturating_mul(pm) / 1000;
    let sheets =
        u32::try_from(u64::from(e.meta.total_sheets).saturating_mul(pm) / 1000).unwrap_or(u32::MAX);
    let changed = e.job.octets_processed.div_ceil(1024) != octets.div_ceil(1024)
        || e.job.media_sheets_completed != sheets;
    e.job.octets_processed = octets;
    e.job.media_sheets_completed = sheets;
    changed
}

fn freeze_progress(e: &mut Entry, now: SystemTime) {
    if e.job.state == JobState::Processing {
        let _ = apply_progress(e, now);
    }
}

fn run_timers(
    g: &mut Inner,
    timing: &PrintTiming,
    limits: &PrintLimits,
    now: SystemTime,
    ev: &mut Events,
) {
    let ids: Vec<u32> = g
        .jobs
        .iter()
        .filter(|(_, e)| !e.job.state.is_terminal())
        .map(|(i, _)| *i)
        .collect();
    for id in ids {
        finish_terminating(g, id, now, ev, limits);
        let Some(e) = g.jobs.get_mut(&id) else {
            continue;
        };
        if e.job.state == JobState::Processing {
            {
                let progressed = apply_progress(e, now);
                let finished = e.meta.proc_end.is_some_and(|t| t <= now);
                if finished {
                    if let Some(f) = e.meta.fail.take() {
                        let reason = match f {
                            DocumentFailure::Format => JobStateReason::DocumentFormatError,
                            DocumentFailure::Compression => JobStateReason::CompressionError,
                            DocumentFailure::Transfer => JobStateReason::DocumentTransferError,
                        };
                        e.job.octets_processed /= 2;
                        e.job.media_sheets_completed /= 2;
                        abort(g, id, reason, now, ev, limits);
                        continue;
                    }
                    if e.job.last_document_received {
                        complete(g, id, now, ev, limits);
                        continue;
                    }
                    // waiting for the next document: its clock starts when printing ended
                    let from = e.meta.proc_end.unwrap_or(now);
                    e.meta
                        .doc_deadline
                        .get_or_insert(from + timing.document_timeout);
                    let r = vec![JobStateReason::JobIncoming];
                    if e.job.reasons != r {
                        e.job.reasons = r;
                        ev.push(PrintEvent::JobStatus { job: e.job.clone() });
                        continue;
                    }
                }
                if progressed {
                    ev.push(PrintEvent::JobStatus { job: e.job.clone() });
                }
            }
        }
        let Some(e) = g.jobs.get(&id) else { continue };
        if !e.job.last_document_received
            && e.meta.paused_at.is_none()
            && e.job.state != JobState::Terminating
            && e.meta.doc_deadline.is_some_and(|t| t <= now)
        {
            abort(g, id, JobStateReason::DocumentTimeoutError, now, ev, limits);
        }
    }
}

// ----- printer status and conditions ------------------------------------------------------

fn set_condition(
    g: &mut Inner,
    limits: &PrintLimits,
    now: SystemTime,
    c: PrintCondition,
    on: bool,
    ev: &mut Events,
) {
    let was = g.faults.conditions.contains(&c);
    if on == was {
        return;
    }
    let stamp = format_datetime(now);
    if on {
        g.faults.conditions.insert(c);
        let cond = DeviceCondition {
            id: c.id(),
            time: stamp,
            name: c.name().to_string(),
            component: c.component().to_string(),
            severity: c.severity(),
        };
        g.conditions.push(ConditionRecord {
            condition: cond.clone(),
            cleared: None,
        });
        while g.conditions.len() > limits.max_condition_history {
            g.conditions.remove(0);
        }
        ev.push(PrintEvent::PrinterStatusCondition { condition: cond });
    } else {
        g.faults.conditions.remove(&c);
        for r in g.conditions.iter_mut().rev() {
            if r.condition.id == c.id() && r.cleared.is_none() {
                r.cleared = Some(stamp.clone());
                break;
            }
        }
        ev.push(PrintEvent::PrinterStatusConditionCleared {
            condition_id: c.id(),
            cleared: stamp,
        });
    }
}

/// Raise a condition by name. `None` when the bound on raised conditions is reached.
fn raise_extra(
    g: &mut Inner,
    limits: &PrintLimits,
    now: SystemTime,
    name: &str,
    component: Option<&str>,
    severity: Option<Severity>,
    ev: &mut Events,
) -> Option<DeviceCondition> {
    if g.faults.extra.len() >= limits.max_condition_history {
        return None;
    }
    let id = g.next_extra;
    g.next_extra = g.next_extra.saturating_add(1);
    let cond = DeviceCondition {
        id,
        time: format_datetime(now),
        name: name.to_string(),
        component: component.map_or_else(|| default_group(name).to_string(), str::to_string),
        severity: severity.unwrap_or_else(|| default_severity(name)),
    };
    g.faults.extra.push(ExtraCondition {
        id,
        name: cond.name.clone(),
        component: cond.component.clone(),
        severity: cond.severity,
        reason: state_reason_for(name).to_string(),
    });
    g.conditions.push(ConditionRecord {
        condition: cond.clone(),
        cleared: None,
    });
    while g.conditions.len() > limits.max_condition_history {
        g.conditions.remove(0);
    }
    ev.push(PrintEvent::PrinterStatusCondition {
        condition: cond.clone(),
    });
    Some(cond)
}

/// Clear one condition raised by name.
fn clear_extra(g: &mut Inner, now: SystemTime, id: u32, ev: &mut Events) -> bool {
    let Some(pos) = g.faults.extra.iter().position(|c| c.id == id) else {
        return false;
    };
    g.faults.extra.remove(pos);
    let stamp = format_datetime(now);
    for r in g.conditions.iter_mut().rev() {
        if r.condition.id == id && r.cleared.is_none() {
            r.cleared = Some(stamp.clone());
            break;
        }
    }
    ev.push(PrintEvent::PrinterStatusConditionCleared {
        condition_id: id,
        cleared: stamp,
    });
    true
}

/// Clear every active condition (fixed or by name) whose name or lab key is `name`.
fn clear_named(g: &mut Inner, limits: &PrintLimits, now: SystemTime, name: &str, ev: &mut Events) {
    for c in PrintCondition::ALL {
        if g.faults.conditions.contains(&c)
            && (c.name() == name || PrintCondition::from_token(name) == Some(c))
        {
            set_condition(g, limits, now, c, false, ev);
        }
    }
    let ids: Vec<u32> = g
        .faults
        .extra
        .iter()
        .filter(|c| c.name == name)
        .map(|c| c.id)
        .collect();
    for id in ids {
        clear_extra(g, now, id, ev);
    }
}

/// After the condition set changed: stop or resume the jobs that are printing.
fn reconcile_stop(g: &mut Inner, now: SystemTime, ev: &mut Events) {
    let stopped = stopping(g);
    for e in g.jobs.values_mut() {
        match (stopped, e.job.state) {
            (true, JobState::Processing) => {
                let _ = apply_progress(e, now);
                e.meta.paused_at = Some(now);
                go(
                    &mut e.job,
                    JobState::ProcessingStopped,
                    vec![JobStateReason::PrinterStopped],
                    now,
                    ev,
                );
            }
            (false, JobState::ProcessingStopped) => {
                let paused = e
                    .meta
                    .paused_at
                    .take()
                    .and_then(|p| now.duration_since(p).ok())
                    .unwrap_or_default();
                e.meta.proc_start = e.meta.proc_start.map(|t| t + paused);
                e.meta.proc_end = e.meta.proc_end.map(|t| t + paused);
                e.meta.doc_deadline = e.meta.doc_deadline.map(|t| t + paused);
                let r = printing_reasons(&e.job);
                go(&mut e.job, JobState::Processing, r, now, ev);
            }
            _ => {}
        }
    }
}

fn state_and_reasons(g: &Inner) -> (PrinterState, Vec<String>, Option<String>) {
    // (stops the printer, id, reason): most severe first, then by id
    let mut active: Vec<(bool, u32, String)> = g
        .faults
        .conditions
        .iter()
        .map(|c| (c.stops_printer(), c.id(), c.state_reason().to_string()))
        .chain(
            g.faults
                .extra
                .iter()
                .map(|c| (c.severity == Severity::Critical, c.id, c.reason.clone())),
        )
        .collect();
    active.sort_by_key(|(stops, id, _)| (!*stops, *id));
    let mut reasons: Vec<String> = Vec::new();
    for (_, _, r) in active {
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    }
    let state = if stopping(g) {
        PrinterState::Stopped
    } else if g
        .jobs
        .values()
        .any(|e| matches!(e.job.state, JobState::Processing | JobState::Started))
    {
        PrinterState::Processing
    } else {
        PrinterState::Idle
    };
    let primary = reasons.first().cloned();
    let reasons = if reasons.is_empty() {
        vec!["None".to_string()]
    } else {
        reasons
    };
    (state, reasons, primary)
}

fn summary_event(g: &mut Inner, ev: &mut Events) {
    let (state, reasons, primary) = state_and_reasons(g);
    if g.last_summary != (state, reasons.clone()) {
        g.last_summary = (state, reasons.clone());
        ev.push(PrintEvent::PrinterStatusSummary {
            state,
            primary_reason: primary,
            reasons,
        });
    }
}

fn snapshot(g: &Inner, now: SystemTime) -> PrinterStatusSnapshot {
    let (state, reasons, primary) = state_and_reasons(g);
    PrinterStatusSnapshot {
        current_time: format_datetime(now),
        state,
        primary_reason: primary,
        reasons,
        queued_jobs: u32::try_from(
            g.jobs
                .values()
                .filter(|e| !e.job.state.is_terminal())
                .count(),
        )
        .unwrap_or(u32::MAX),
        active_conditions: g
            .conditions
            .iter()
            .filter(|r| r.cleared.is_none())
            .map(|r| r.condition.clone())
            .collect(),
        condition_history: g.conditions.clone(),
        accepting_jobs: !g.faults.not_accepting_jobs,
    }
}

/// Build a `<JobStateReasons>`-ready list of tokens (never empty).
pub fn reason_tokens(job: &PrintJob) -> Vec<&'static str> {
    if job.reasons.is_empty() {
        vec![JobStateReason::None.token()]
    } else {
        job.reasons.iter().map(|r| r.token()).collect()
    }
}

/// The effective stored ticket as typed JSON (see [`PrintTicket::to_json`]), with
/// `document_format_requested` (the format of the job's first document, or `doc_format` when
/// the job has none yet). `null` when the job has no ticket (IPP, raw).
pub fn ticket_json(job: &PrintJob, doc_format: Option<&str>) -> serde_json::Value {
    use super::ctx::{PrintLimits, Quirks};
    use super::ticket::PrintTicket;
    let Some(el) = job.ticket.as_ref() else {
        return serde_json::Value::Null;
    };
    let quirks = Quirks {
        lenient_job_description: true,
        ..Quirks::default()
    };
    let mut v = match PrintTicket::parse(el, &PrintLimits::default(), &quirks) {
        Ok(t) => t.to_json(),
        Err(_) => return serde_json::Value::Null,
    };
    let fmt = job
        .documents
        .first()
        .map(|d| d.format.as_str())
        .or(doc_format);
    if let Some(o) = v.as_object_mut() {
        o.insert("document_format_requested".into(), serde_json::json!(fmt));
    }
    v
}

/// Milliseconds since the Unix epoch (0 before it).
pub fn unix_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso8601(t: std::time::SystemTime) -> String {
    let ms = unix_ms(t);
    let (secs, milli) = (ms / 1000, ms % 1000);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil-from-days (Howard Hinnant)
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{milli:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The job-level fields shared by the control plane rows and the capture sidecar.
pub fn job_meta_json(job: &PrintJob, doc_format: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "name": job.name,
        "user": job.user,
        "ticket": ticket_json(job, doc_format),
        "state": job.state.token(),
        "reasons": reason_tokens(job),
        "created": iso8601(job.created),
        "created_unix_ms": unix_ms(job.created),
        "ended": job.ended.map(iso8601),
        "ended_unix_ms": job.ended.map(unix_ms),
    })
}

/// The extra sidecar keys of a captured document: the job fields of [`job_meta_json`] plus
/// this document's `seq`, `format` and `compression`.
pub fn capture_meta_json(
    job: &PrintJob,
    seq: u32,
    format: &str,
    compression: &str,
) -> serde_json::Value {
    let mut v = job_meta_json(job, Some(format));
    if let Some(o) = v.as_object_mut() {
        o.insert("seq".into(), seq.into());
        o.insert("format".into(), format.into());
        o.insert("compression".into(), compression.into());
    }
    v
}

/// The ticket element stored on a job, if any.
pub fn job_ticket(job: &PrintJob) -> Option<&Element> {
    job.ticket.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::ctx::ManualClock;
    use crate::wsd::types::RecordingEventBus;

    fn engine() -> (Arc<JobEngine>, Arc<ManualClock>, Arc<RecordingEventBus>) {
        let clock = Arc::new(ManualClock::new());
        let bus = Arc::new(SharedBus::default());
        let rec = Arc::new(RecordingEventBus::new());
        bus.set(rec.clone());
        let e = Arc::new(JobEngine::new(
            PrintLimits::default(),
            PrintTiming::default(),
            clock.clone(),
            bus,
        ));
        (e, clock, rec)
    }

    fn new_job(e: &JobEngine) -> u32 {
        e.create_job(
            NewJob {
                name: "j".into(),
                user: "u".into(),
                ticket: None,
            },
            1,
        )
        .unwrap()
    }

    fn doc(size: u64, last: bool) -> DocumentIn {
        DocumentIn {
            format: "application/pdf".into(),
            compression: "None".into(),
            size,
            last,
            ..DocumentIn::default()
        }
    }

    #[test]
    fn transition_matrix_is_exact() {
        use JobState::*;
        let legal = [
            (Pending, PendingHeld),
            (Pending, Started),
            (Pending, Processing),
            (Pending, Terminating),
            (Pending, Aborted),
            (PendingHeld, Pending),
            (PendingHeld, Terminating),
            (PendingHeld, Aborted),
            (Started, Processing),
            (Started, Terminating),
            (Started, Aborted),
            (Processing, ProcessingStopped),
            (Processing, Terminating),
            (Processing, Completed),
            (Processing, Aborted),
            (ProcessingStopped, Processing),
            (ProcessingStopped, Terminating),
            (ProcessingStopped, Aborted),
            (Terminating, Canceled),
            (Terminating, Completed),
            (Terminating, Aborted),
        ];
        for from in JobState::ALL {
            for to in JobState::ALL {
                let want = legal.contains(&(from, to));
                assert_eq!(transition_allowed(from, to), want, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn ids_increase_and_wrap() {
        let (e, _, _) = engine();
        assert_eq!(new_job(&e), 1);
        assert_eq!(new_job(&e), 2);
        e.lock().next_id = MAX_JOB_ID;
        assert_eq!(new_job(&e), MAX_JOB_ID);
        assert_eq!(new_job(&e), 3); // wraps to 1, skips the ids in use (1, 2)
    }

    #[test]
    fn document_timeout_aborts_after_sixty_seconds() {
        let (e, clock, rec) = engine();
        let id = new_job(&e);
        clock.advance(Duration::from_secs(59));
        assert_eq!(e.get(id).unwrap().state, JobState::Pending);
        clock.advance(Duration::from_secs(1));
        let j = e.get(id).unwrap();
        assert_eq!(j.state, JobState::Aborted);
        assert_eq!(j.reasons, vec![JobStateReason::DocumentTimeoutError]);
        assert!(e.active().is_empty());
        assert_eq!(e.history().len(), 1);
        assert!(rec
            .events()
            .iter()
            .any(|v| matches!(v, PrintEvent::JobEndState { job } if job.id == id)));
    }

    #[test]
    fn single_document_job_completes_with_counters() {
        let (e, clock, rec) = engine();
        let id = new_job(&e);
        let (j, doc_id) = e.commit_document(id, doc(100 * 1024, true), true).unwrap();
        assert_eq!(doc_id, 1);
        assert_eq!(j.state, JobState::Processing);
        assert_eq!(j.reasons, vec![JobStateReason::JobPrinting]);
        clock.advance(Duration::from_millis(400));
        let mid = e.get(id).unwrap();
        assert_eq!(mid.state, JobState::Processing);
        assert!(mid.octets_processed > 0 && mid.octets_processed < 100 * 1024);
        clock.advance(Duration::from_secs(5));
        let done = e.get(id).unwrap();
        assert_eq!(done.state, JobState::Completed);
        assert_eq!(done.reasons, vec![JobStateReason::JobCompletedSuccessfully]);
        assert_eq!(done.k_octets_processed(), 100);
        assert_eq!(done.media_sheets_completed, 4);
        let kinds: Vec<&str> = rec.events().iter().map(|v| v.name()).collect();
        assert!(kinds.contains(&"JobStatusEvent") && kinds.contains(&"JobEndStateEvent"));
        assert!(kinds.contains(&"PrinterStatusSummaryEvent"));
    }

    #[test]
    fn multi_document_job_waits_and_times_out_between_documents() {
        let (e, clock, _) = engine();
        let id = new_job(&e);
        e.commit_document(id, doc(10, false), true).unwrap();
        clock.advance(Duration::from_secs(2));
        let j = e.get(id).unwrap();
        assert_eq!(j.state, JobState::Processing);
        assert_eq!(j.reasons, vec![JobStateReason::JobIncoming]);
        clock.advance(Duration::from_secs(61));
        assert_eq!(e.get(id).unwrap().state, JobState::Aborted);
    }

    #[test]
    fn multi_document_job_completes_on_last() {
        let (e, clock, _) = engine();
        let id = new_job(&e);
        e.commit_document(id, doc(10, false), true).unwrap();
        clock.advance(Duration::from_secs(2));
        let (j, d2) = e.commit_document(id, doc(10, true), true).unwrap();
        assert_eq!(d2, 2);
        assert!(j.last_document_received);
        clock.advance(Duration::from_secs(5));
        assert_eq!(e.get(id).unwrap().state, JobState::Completed);
        assert_eq!(e.get(id).unwrap().number_of_documents(), 2);
    }

    #[test]
    fn precheck_error_matrix() {
        let (e, clock, _) = engine();
        let kind = |r: Result<(), PrintFault>| r.unwrap_err().kind();
        assert_eq!(
            kind(e.precheck_document(99, true, true)),
            Some(FaultKind::ClientErrorJobIdNotFound)
        );
        let id = new_job(&e);
        assert_eq!(
            kind(e.precheck_document(id, false, false)),
            Some(FaultKind::ClientErrorMultipleDocumentsNotSupported)
        );
        assert!(e.precheck_document(id, true, false).is_ok());
        e.commit_document(id, doc(10, true), true).unwrap();
        assert_eq!(
            kind(e.precheck_document(id, true, true)),
            Some(FaultKind::ClientErrorLastDocumentAlreadySent)
        );
        clock.advance(Duration::from_secs(10));
        assert_eq!(e.get(id).unwrap().state, JobState::Completed);
        assert_eq!(
            kind(e.precheck_document(id, true, true)),
            Some(FaultKind::ClientErrorLastDocumentAlreadySent)
        );
        // cancelled job
        let c = new_job(&e);
        e.cancel(c, CancelSource::User).unwrap();
        assert_eq!(
            kind(e.precheck_document(c, true, true)),
            Some(FaultKind::ServerErrorJobCancelled)
        );
        // aborted by timeout without a last document
        let t = new_job(&e);
        clock.advance(Duration::from_secs(61));
        assert_eq!(
            kind(e.precheck_document(t, true, true)),
            Some(FaultKind::OperationFailed)
        );
        // document active
        let a = new_job(&e);
        e.apply_faults(&FaultBody {
            document_active: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(
            kind(e.precheck_document(a, true, true)),
            Some(FaultKind::ServerErrorDocumentActive)
        );
    }

    #[test]
    fn busy_window_gives_document_active_then_clears() {
        let clock = Arc::new(ManualClock::new());
        let timing = PrintTiming {
            document_busy: Duration::from_secs(2),
            ..PrintTiming::default()
        };
        let e = JobEngine::new(
            PrintLimits::default(),
            timing,
            clock.clone(),
            Arc::new(SharedBus::default()),
        );
        let id = new_job(&e);
        e.commit_document(id, doc(10, false), true).unwrap();
        assert_eq!(
            e.precheck_document(id, true, true).unwrap_err().kind(),
            Some(FaultKind::ServerErrorDocumentActive)
        );
        clock.advance(Duration::from_secs(3));
        assert!(e.precheck_document(id, true, true).is_ok());
    }

    #[test]
    fn cancel_matrix() {
        let (e, clock, rec) = engine();
        assert_eq!(
            e.cancel(7, CancelSource::User).unwrap_err().kind(),
            Some(FaultKind::ClientErrorJobIdNotFound)
        );
        let id = new_job(&e);
        let j = e.cancel(id, CancelSource::User).unwrap();
        assert_eq!(j.state, JobState::Canceled);
        assert_eq!(j.reasons, vec![JobStateReason::JobCanceledByUser]);
        assert_eq!(
            e.cancel(id, CancelSource::User).unwrap_err().kind(),
            Some(FaultKind::OperationFailed)
        );
        // device cancel while printing
        let p = new_job(&e);
        e.commit_document(p, doc(1 << 20, true), true).unwrap();
        clock.advance(Duration::from_millis(200));
        let j = e.cancel(p, CancelSource::Device).unwrap();
        assert_eq!(j.state, JobState::Canceled);
        assert_eq!(j.reasons, vec![JobStateReason::JobCanceledAtDevice]);
        assert!(j.octets_processed < 1 << 20);
        // the Terminating step was published
        let states: Vec<JobState> = rec
            .events()
            .iter()
            .filter_map(|v| match v {
                PrintEvent::JobStatus { job } if job.id == p => Some(job.state),
                _ => None,
            })
            .collect();
        assert!(states.contains(&JobState::Terminating));
    }

    #[test]
    fn terminating_lasts_for_the_configured_time() {
        let clock = Arc::new(ManualClock::new());
        let timing = PrintTiming {
            terminating: Duration::from_secs(3),
            ..PrintTiming::default()
        };
        let e = JobEngine::new(
            PrintLimits::default(),
            timing,
            clock.clone(),
            Arc::new(SharedBus::default()),
        );
        let id = new_job(&e);
        assert_eq!(
            e.cancel(id, CancelSource::User).unwrap().state,
            JobState::Terminating
        );
        // idempotent while terminating
        assert_eq!(
            e.cancel(id, CancelSource::User).unwrap().state,
            JobState::Terminating
        );
        assert_eq!(
            e.precheck_document(id, true, true).unwrap_err().kind(),
            Some(FaultKind::ServerErrorJobCancelled)
        );
        clock.advance(Duration::from_secs(3));
        assert_eq!(e.get(id).unwrap().state, JobState::Canceled);
    }

    #[test]
    fn history_ring_is_capped() {
        let clock = Arc::new(ManualClock::new());
        let limits = PrintLimits {
            history_cap: 3,
            ..PrintLimits::default()
        };
        let e = JobEngine::new(
            limits,
            PrintTiming::default(),
            clock,
            Arc::new(SharedBus::default()),
        );
        for _ in 0..5 {
            let id = new_job(&e);
            e.cancel(id, CancelSource::User).unwrap();
        }
        let ids: Vec<u32> = e.history().iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        assert!(e.get(1).is_none());
    }

    #[test]
    fn stopping_condition_stops_and_resumes_printing() {
        let (e, clock, rec) = engine();
        let id = new_job(&e);
        e.commit_document(id, doc(1 << 20, true), true).unwrap();
        clock.advance(Duration::from_millis(300));
        e.apply_faults(&FaultBody {
            paper_out: Some(true),
            ..FaultBody::default()
        });
        let j = e.get(id).unwrap();
        assert_eq!(j.state, JobState::ProcessingStopped);
        assert_eq!(j.reasons, vec![JobStateReason::PrinterStopped]);
        let st = e.status();
        assert_eq!(st.state, PrinterState::Stopped);
        assert_eq!(st.primary_reason.as_deref(), Some("MediaEmpty"));
        assert_eq!(st.active_conditions.len(), 1);
        clock.advance(Duration::from_secs(120)); // no progress, no timeout while stopped
        assert_eq!(e.get(id).unwrap().state, JobState::ProcessingStopped);
        e.apply_faults(&FaultBody {
            paper_out: Some(false),
            ..FaultBody::default()
        });
        assert_eq!(e.get(id).unwrap().state, JobState::Processing);
        clock.advance(Duration::from_secs(5));
        assert_eq!(e.get(id).unwrap().state, JobState::Completed);
        let names: Vec<&str> = rec.events().iter().map(|v| v.name()).collect();
        assert!(names.contains(&"PrinterStatusConditionEvent"));
        assert!(names.contains(&"PrinterStatusConditionClearedEvent"));
        let st = e.status();
        assert_eq!(st.state, PrinterState::Idle);
        assert_eq!(st.condition_history.len(), 1);
        assert!(st.condition_history[0].cleared.is_some());
    }

    #[test]
    fn warning_condition_completes_with_warnings() {
        let (e, clock, _) = engine();
        e.apply_faults(&FaultBody {
            toner_low: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(e.status().state, PrinterState::Idle);
        let id = new_job(&e);
        e.commit_document(id, doc(10, true), true).unwrap();
        clock.advance(Duration::from_secs(2));
        assert_eq!(
            e.get(id).unwrap().reasons,
            vec![JobStateReason::JobCompletedWithWarnings]
        );
    }

    #[test]
    fn fail_next_document_aborts_with_reason() {
        let (e, clock, _) = engine();
        e.apply_faults(&FaultBody {
            fail_next_document: Some("Format".into()),
            ..FaultBody::default()
        });
        let id = new_job(&e);
        e.commit_document(id, doc(2048, true), true).unwrap();
        clock.advance(Duration::from_secs(5));
        let j = e.get(id).unwrap();
        assert_eq!(j.state, JobState::Aborted);
        assert_eq!(j.reasons, vec![JobStateReason::DocumentFormatError]);
    }

    #[test]
    fn service_faults_refuse_creation() {
        let (e, _, _) = engine();
        e.apply_faults(&FaultBody {
            not_accepting_jobs: Some(true),
            ..FaultBody::default()
        });
        let err = e.create_job(NewJob::default(), 1).unwrap_err();
        assert_eq!(err.kind(), Some(FaultKind::ServerErrorNotAcceptingJobs));
        assert!(!e.status().accepting_jobs);
        e.apply_faults(&FaultBody {
            reset: true,
            temporary_error: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(
            e.create_job(NewJob::default(), 1).unwrap_err().kind(),
            Some(FaultKind::ServerErrorTemporaryError)
        );
        assert!(e.check_service_faults().is_err());
        e.apply_faults(&FaultBody {
            reset: true,
            ..FaultBody::default()
        });
        assert!(e.create_job(NewJob::default(), 1).is_ok());
    }

    #[test]
    fn active_job_cap_refuses() {
        let clock = Arc::new(ManualClock::new());
        let limits = PrintLimits {
            max_active_jobs: 2,
            ..PrintLimits::default()
        };
        let e = JobEngine::new(
            limits,
            PrintTiming::default(),
            clock,
            Arc::new(SharedBus::default()),
        );
        new_job(&e);
        new_job(&e);
        assert_eq!(
            e.create_job(NewJob::default(), 1).unwrap_err().kind(),
            Some(FaultKind::ServerErrorNotAcceptingJobs)
        );
    }

    #[test]
    fn update_rejects_illegal_transitions_and_emits_events() {
        let (e, _, rec) = engine();
        let id = new_job(&e);
        let err = e
            .update(id, &mut |j| j.state = JobState::Completed)
            .unwrap_err();
        assert_eq!(err.kind(), Some(FaultKind::OperationFailed));
        assert_eq!(e.get(id).unwrap().state, JobState::Pending);
        let j = e
            .update(id, &mut |j| j.state = JobState::Processing)
            .unwrap();
        assert_eq!(j.state, JobState::Processing);
        let j = e
            .update(id, &mut |j| j.state = JobState::Completed)
            .unwrap();
        assert!(j.ended.is_some());
        assert_eq!(e.history().len(), 1);
        assert!(rec
            .events()
            .iter()
            .any(|v| matches!(v, PrintEvent::JobEndState { .. })));
        assert!(e.update(55, &mut |_| {}).is_err());
        // a terminal job has no exits
        assert!(e
            .update(id, &mut |j| j.state = JobState::Processing)
            .is_err());
    }

    #[test]
    fn complete_now_finishes_a_pending_job() {
        let (e, _, _) = engine();
        let id = new_job(&e);
        let j = e.complete_now(id).unwrap();
        assert_eq!(j.state, JobState::Completed);
        assert!(e.complete_now(404).is_none());
    }

    #[test]
    fn rejected_document_leaves_a_reason_until_timeout() {
        let (e, clock, _) = engine();
        let id = new_job(&e);
        e.note_rejected_document(id, JobStateReason::UnsupportedDocumentFormat);
        let j = e.get(id).unwrap();
        assert_eq!(j.state, JobState::Pending);
        assert!(j
            .reasons
            .contains(&JobStateReason::UnsupportedDocumentFormat));
        clock.advance(Duration::from_secs(61));
        assert_eq!(e.get(id).unwrap().state, JobState::Aborted);
    }

    #[test]
    fn reset_restarts_ids() {
        let (e, _, _) = engine();
        new_job(&e);
        e.reset();
        assert_eq!(new_job(&e), 1);
        assert_eq!(e.queued_jobs(), 1);
    }

    #[test]
    fn conditions_raised_by_name_stop_and_resume_jobs_and_publish_events() {
        use crate::wsd::ctx::RaiseSpec;
        let (e, _clock, rec) = engine();
        let id = new_job(&e);
        e.commit_document(id, doc(10, true), true).unwrap();
        assert_eq!(e.get(id).unwrap().state, JobState::Processing);

        let c = e.raise_condition("Jam", None, None).unwrap();
        assert!(c.id >= FIRST_EXTRA_ID);
        assert_eq!(c.severity, Severity::Critical);
        assert_eq!(e.get(id).unwrap().state, JobState::ProcessingStopped);
        let st = e.status();
        assert_eq!(st.state, PrinterState::Stopped);
        assert_eq!(st.reasons, ["MediaJam"]);
        assert_eq!(st.active_conditions.len(), 1);
        assert!(rec.events().iter().any(
            |ev| matches!(ev, PrintEvent::PrinterStatusCondition { condition } if condition.name == "Jam")
        ));

        assert!(e.clear_condition(c.id));
        assert!(!e.clear_condition(c.id));
        assert_eq!(e.get(id).unwrap().state, JobState::Processing);
        assert!(rec.events().iter().any(
            |ev| matches!(ev, PrintEvent::PrinterStatusConditionCleared { condition_id, .. } if *condition_id == c.id)
        ));

        // a warning does not stop anything; a lab request can raise and clear by name
        e.apply_faults(&FaultBody {
            raise: vec![RaiseSpec {
                name: "ConsumableLow".into(),
                component: Some("Consumable/Black".into()),
                severity: None,
            }],
            ..FaultBody::default()
        });
        let st = e.status();
        assert_eq!(st.state, PrinterState::Processing);
        assert_eq!(st.reasons, ["MarkerSupplyLow"]);
        assert_eq!(st.active_conditions[0].severity, Severity::Warning);
        assert_eq!(st.active_conditions[0].component, "Consumable/Black");
        e.apply_faults(&FaultBody {
            clear: vec!["ConsumableLow".into()],
            ..FaultBody::default()
        });
        assert!(e.status().active_conditions.is_empty());
        assert_eq!(e.status().reasons, ["None"]);
    }

    #[test]
    fn iso8601_formats_utc_timestamps() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 2024-02-29T12:34:56.789Z
        let t = UNIX_EPOCH + Duration::from_millis(1_709_210_096_789);
        assert_eq!(iso8601(t), "2024-02-29T12:34:56.789Z");
        assert_eq!(unix_ms(t), 1_709_210_096_789);
    }

    #[test]
    fn ticket_json_is_null_without_a_ticket() {
        let j = PrintJob::new(1, "n", "u");
        assert!(ticket_json(&j, Some("application/pdf")).is_null());
        let m = job_meta_json(&j, None);
        assert_eq!(m["name"], "n");
        assert_eq!(m["user"], "u");
        assert!(m["ticket"].is_null());
        assert!(m["ended"].is_null());
        let c = capture_meta_json(&j, 2, "application/pdf", "None");
        assert_eq!(c["seq"], 2);
        assert_eq!(c["compression"], "None");
    }
}
