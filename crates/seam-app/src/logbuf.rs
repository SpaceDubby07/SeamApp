//! In-memory ring buffer of recent log records so the frontend can show a
//! live log view and export it, without the user hunting for the on-disk
//! file.
//!
//! A [`BufferLayer`] is installed alongside the file/stdout layers in
//! `init_logging`; it copies every record that passes the global
//! `EnvFilter` into a bounded [`VecDeque`]. `commands::{get_logs,
//! export_logs, clear_logs}` read and manage it. Everything here is
//! process-global (one capture instance per process, like the rest of the
//! app) and lock-guarded; the layer does only a format + push under the
//! lock, so it stays cheap enough to sit on every logging call.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Most recent lines kept. ~5k lines is enough to span a full connect →
/// handoff → reclaim → disconnect cycle at `debug`, and small enough to
/// hand to the webview in one poll.
const CAPACITY: usize = 5000;

/// One captured log record, as the frontend consumes it.
#[derive(Clone, Serialize)]
pub struct LogLine {
    /// Monotonic id. Lets the frontend ask for "everything after N" rather
    /// than refetching the whole buffer each poll.
    pub seq: u64,
    /// Milliseconds since the Unix epoch (UTC).
    pub ts_millis: u64,
    /// `TRACE` | `DEBUG` | `INFO` | `WARN` | `ERROR`.
    pub level: &'static str,
    /// Emitting module path, e.g. `seam_core::session`.
    pub target: String,
    /// The rendered message, followed by any structured fields rendered as
    /// `{key=value ...}`.
    pub message: String,
}

static BUFFER: Mutex<VecDeque<LogLine>> = Mutex::new(VecDeque::new());
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Lines with `seq > after` (all of them if `after` is `None`), oldest
/// first.
#[must_use]
pub fn snapshot(after: Option<u64>) -> Vec<LogLine> {
    let after = after.unwrap_or(0);
    let buf = BUFFER.lock().expect("log buffer poisoned");
    buf.iter()
        .filter(|line| line.seq > after)
        .cloned()
        .collect()
}

/// Empties the buffer. The `seq` counter keeps climbing, so an in-flight
/// frontend poll can't re-surface stale lines afterwards.
pub fn clear() {
    BUFFER.lock().expect("log buffer poisoned").clear();
}

/// The whole buffer as plain text, one line per record — what
/// `export_logs` writes to a file.
#[must_use]
pub fn render_text() -> String {
    let buf = BUFFER.lock().expect("log buffer poisoned");
    let mut out = String::with_capacity(buf.len() * 96);
    for line in buf.iter() {
        let _ = writeln!(
            out,
            "{} {:>5} {} — {}",
            format_time_of_day(line.ts_millis),
            line.level,
            line.target,
            line.message
        );
    }
    out
}

/// `HH:MM:SS.mmm` (UTC) from epoch millis — no date, no crate. Enough to
/// line records up against each other while debugging a session.
fn format_time_of_day(ms: u64) -> String {
    let millis = ms % 1000;
    let secs = ms / 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{millis:03}")
}

/// The `tracing` layer that copies records into [`BUFFER`].
pub struct BufferLayer;

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = RecordVisitor::default();
        event.record(&mut visitor);

        let line = LogLine {
            seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
            ts_millis: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            level: meta.level().as_str(),
            target: meta.target().to_owned(),
            message: visitor.finish(),
        };

        let mut buf = BUFFER.lock().expect("log buffer poisoned");
        while buf.len() >= CAPACITY {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

/// The layer to hand to `tracing_subscriber::registry().with(..)`.
#[must_use]
pub fn layer() -> BufferLayer {
    BufferLayer
}

/// Pulls the `message` field out on its own and renders every other field
/// as `key=value`, matching how `tracing_subscriber`'s own fmt layer
/// splits them.
#[derive(Default)]
struct RecordVisitor {
    message: String,
    fields: String,
}

impl RecordVisitor {
    fn finish(mut self) -> String {
        if !self.fields.is_empty() {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push('{');
            self.message.push_str(self.fields.trim_start());
            self.message.push('}');
        }
        self.message
    }
}

impl Visit for RecordVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}
