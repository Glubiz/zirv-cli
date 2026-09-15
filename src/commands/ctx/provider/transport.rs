//! Provider-neutral streaming transport plumbing shared by the direct
//! providers (N07 Anthropic, N08 OpenAI).
//!
//! Every direct adapter runs its blocking HTTPS/SSE read on a worker thread
//! and supervises it from the caller's thread so cancellation, first-event
//! and idle deadlines stay observable even while the socket blocks. The
//! bounded event channel is the backpressure seam: a slow consumer stalls
//! the reader instead of growing an unbounded queue.

#![allow(dead_code)] // N09 wires the direct providers into the runtime loop.

use std::io::{BufRead, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use super::adapter::{
    Cancellation, EventSink, FailureClass, FailureScope, FailureScopeKind, ProviderFailure,
    ProviderResponse, ProviderStreamEvent, ProviderTarget,
};

pub(crate) const MAX_ERROR_BODY_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
/// Caps one accumulated content block (text, thinking, or partial tool JSON).
/// A single SSE line is already bounded by `MAX_SSE_LINE_BYTES`, but a block
/// is rebuilt from an unbounded number of deltas, so it needs its own ceiling.
pub(crate) const MAX_BLOCK_ACCUMULATOR_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_RESPONSE_BLOCKS: usize = 4_096;
/// Bounds how long a worker's blocking body read may go without new bytes
/// before it loops back and rechecks cancellation. The real first-event/idle
/// deadlines are enforced independently by [`supervise`]'s wall-clock loop, so
/// this only bounds how promptly a cancelled or timed-out worker notices and
/// exits instead of blocking for up to `StreamTimeouts::idle`.
pub(crate) const WORKER_READ_POLL: Duration = Duration::from_millis(250);
const CANCELLATION_POLL: Duration = Duration::from_millis(25);
const STREAM_EVENT_QUEUE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTimeouts {
    pub connect: Duration,
    pub first_event: Duration,
    pub idle: Duration,
}

impl Default for StreamTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_event: Duration::from_secs(60),
            idle: Duration::from_secs(120),
        }
    }
}

impl StreamTimeouts {
    pub(crate) fn has_zero(self) -> bool {
        [self.connect, self.first_event, self.idle].contains(&Duration::ZERO)
    }
}

pub(crate) enum TransportUpdate {
    Activity,
    Event(ProviderStreamEvent),
    Done(Result<ProviderResponse, ProviderFailure>),
}

struct ChannelSink(SyncSender<TransportUpdate>);

impl EventSink for ChannelSink {
    fn push(&mut self, event: ProviderStreamEvent) {
        let update = if event == ProviderStreamEvent::ProtocolActivity {
            TransportUpdate::Activity
        } else {
            TransportUpdate::Event(event)
        };
        let _ = self.0.send(update);
    }
}

#[derive(Debug)]
struct WorkerCancellation(Arc<AtomicBool>);

impl Cancellation for WorkerCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Runs `blocking` on a named worker thread and forwards its stream events
/// while enforcing cancellation plus the first-event and idle deadlines.
pub(crate) fn supervise<F>(
    provider: &'static str,
    thread_name: &'static str,
    timeouts: StreamTimeouts,
    target: &ProviderTarget,
    cancellation: &dyn Cancellation,
    sink: &mut dyn EventSink,
    blocking: F,
) -> Result<ProviderResponse, ProviderFailure>
where
    F: FnOnce(&dyn Cancellation, &mut dyn EventSink) -> Result<ProviderResponse, ProviderFailure>
        + Send
        + 'static,
{
    if cancellation.is_cancelled() {
        return Err(cancelled(provider));
    }
    let worker_cancelled = Arc::new(AtomicBool::new(false));
    let worker_flag = WorkerCancellation(Arc::clone(&worker_cancelled));
    let (sender, receiver) = mpsc::sync_channel(STREAM_EVENT_QUEUE);
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let mut stream_sink = ChannelSink(sender.clone());
            let result = blocking(&worker_flag, &mut stream_sink);
            let _ = sender.send(TransportUpdate::Done(result));
        })
        .map_err(|error| {
            transport_failure(
                format!("failed to start {provider} transport worker: {error}"),
                target,
            )
        })?;

    let mut saw_event = false;
    let mut deadline = Instant::now() + timeouts.first_event;
    loop {
        if cancellation.is_cancelled() {
            worker_cancelled.store(true, Ordering::Release);
            return Err(cancelled(provider));
        }
        let now = Instant::now();
        if now >= deadline {
            worker_cancelled.store(true, Ordering::Release);
            return Err(timeout_failure(provider, saw_event, target));
        }
        let wait = deadline
            .saturating_duration_since(now)
            .min(CANCELLATION_POLL);
        match receiver.recv_timeout(wait) {
            Ok(TransportUpdate::Activity) => {
                saw_event = true;
                deadline = Instant::now() + timeouts.idle;
            }
            Ok(TransportUpdate::Event(event)) => {
                saw_event = true;
                deadline = Instant::now() + timeouts.idle;
                sink.push(event);
            }
            Ok(TransportUpdate::Done(result)) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(transport_failure(
                    format!("{provider} transport worker stopped unexpectedly"),
                    target,
                ));
            }
        }
    }
}

/// Reads one SSE line into `line`, which the caller clears between lines.
///
/// The worker's socket read is deliberately bounded to [`WORKER_READ_POLL`] so
/// a cancelled worker notices promptly, so a bare poll timeout is not a stream
/// failure: it is retried (rechecking cancellation each time) with whatever
/// partial line was already buffered left in place. Returns the bytes read,
/// `0` at end of stream.
pub(crate) fn read_sse_line<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    provider: &'static str,
    cancellation: &dyn Cancellation,
    target: &ProviderTarget,
) -> Result<usize, ProviderFailure> {
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled(provider));
        }
        let remaining = (MAX_SSE_LINE_BYTES + 1).saturating_sub(line.len());
        match Read::take(&mut *reader, remaining as u64).read_line(line) {
            Ok(read) => {
                if line.len() > MAX_SSE_LINE_BYTES {
                    return Err(invalid_stream(format!(
                        "{provider} SSE line exceeds {MAX_SSE_LINE_BYTES} bytes"
                    )));
                }
                return Ok(read);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) || error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<ureq::Error>())
                    .is_some_and(|source| matches!(source, ureq::Error::Timeout(_))) =>
            {
                // ureq's BodyReader wraps its timeout as ErrorKind::Other.
                // The supervisor owns the first-event/idle deadline;
                // don't turn a read poll into a transport outage. Yield
                // in case a reader keeps returning an expired deadline.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(invalid_stream(format!(
                    "{provider} SSE contains invalid UTF-8"
                )));
            }
            Err(error) => {
                return Err(transport_failure(
                    format!("{provider} stream read failed: {error}"),
                    target,
                ));
            }
        }
    }
}

/// Rejects a content block whose accumulated text/thinking/partial-JSON buffer
/// would exceed [`MAX_BLOCK_ACCUMULATOR_BYTES`] once the next delta is
/// appended, settling the stream to the same typed failure class used for an
/// oversized SSE line instead of growing the buffer without bound.
pub(crate) fn check_block_accumulator_cap(
    provider: &'static str,
    current_len: usize,
    delta_len: usize,
) -> Result<(), ProviderFailure> {
    if current_len.saturating_add(delta_len) > MAX_BLOCK_ACCUMULATOR_BYTES {
        return Err(invalid_stream(format!(
            "{provider} content block exceeds {MAX_BLOCK_ACCUMULATOR_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(crate) struct ResponseLimits {
    total_bytes: usize,
    max_total_bytes: usize,
}

impl ResponseLimits {
    pub(crate) fn new() -> Self {
        Self::with_total_bytes(MAX_RESPONSE_BYTES)
    }

    fn with_total_bytes(max_total_bytes: usize) -> Self {
        Self {
            total_bytes: 0,
            max_total_bytes,
        }
    }

    pub(crate) fn record_bytes(
        &mut self,
        provider: &'static str,
        bytes: usize,
    ) -> Result<(), ProviderFailure> {
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        if self.total_bytes > self.max_total_bytes {
            return Err(invalid_stream(format!(
                "{provider} response exceeds {} bytes",
                self.max_total_bytes
            )));
        }
        Ok(())
    }
}

pub(crate) fn check_response_block_cap(
    provider: &'static str,
    blocks: usize,
) -> Result<(), ProviderFailure> {
    if blocks > MAX_RESPONSE_BLOCKS {
        return Err(invalid_stream(format!(
            "{provider} response exceeds {MAX_RESPONSE_BLOCKS} content blocks"
        )));
    }
    Ok(())
}

pub(crate) fn cancelled(provider: &str) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Cancelled,
        FailureScope::request(),
        format!("{provider} request cancelled"),
    )
}

pub(crate) fn timeout_failure(
    provider: &str,
    saw_event: bool,
    target: &ProviderTarget,
) -> ProviderFailure {
    let (class, message) = if saw_event {
        (
            FailureClass::IdleTimeout,
            format!("{provider} stream became idle"),
        )
    } else {
        (
            FailureClass::FirstEventTimeout,
            format!("{provider} did not produce a first event before the timeout"),
        )
    };
    let mut failure = ProviderFailure::new(
        class,
        target_scope(target, FailureScopeKind::Endpoint),
        message,
    );
    failure.retry.retryable = true;
    failure
}

pub(crate) fn transport_failure(message: String, target: &ProviderTarget) -> ProviderFailure {
    let mut failure = ProviderFailure::new(
        FailureClass::Transport,
        target_scope(target, FailureScopeKind::Endpoint),
        message,
    );
    failure.retry.retryable = true;
    failure
}

pub(crate) fn invalid_stream(message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::InvalidStream,
        FailureScope::request(),
        message,
    )
}

pub(crate) fn target_scope(target: &ProviderTarget, kind: FailureScopeKind) -> FailureScope {
    let id = match kind {
        FailureScopeKind::Request => None,
        FailureScopeKind::Model => Some(target.model.id.clone()),
        FailureScopeKind::Account => Some(target.account.to_string()),
        FailureScopeKind::BillingPool => Some(target.billing_pool.to_string()),
        FailureScopeKind::Endpoint => Some(target.endpoint.to_string()),
        FailureScopeKind::Provider => Some(target.provider.to_string()),
    };
    FailureScope { kind, id }
}

pub(crate) fn parse_retry_after_ms(value: &str) -> Option<u64> {
    parse_retry_after_ms_at(value, std::time::SystemTime::now())
}

fn parse_retry_after_ms_at(value: &str, now: std::time::SystemTime) -> Option<u64> {
    if let Some(ms) = value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
    {
        return Some(ms);
    }
    let fields: Vec<&str> = value.split_whitespace().collect();
    if fields.len() != 6 || !fields[0].ends_with(',') || fields[5] != "GMT" {
        return None;
    }
    let day = fields[1].parse::<u32>().ok()?;
    let month = match fields[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = fields[3].parse::<i32>().ok()?;
    let mut clock = fields[4].split(':');
    let hour = clock.next()?.parse::<u32>().ok()?;
    let minute = clock.next()?.parse::<u32>().ok()?;
    let second = clock.next()?.parse::<u32>().ok()?;
    if clock.next().is_some() || day == 0 || day > 31 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let timestamp = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour * 3_600 + minute * 60 + second))?;
    let now = i64::try_from(now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs()).ok()?;
    u64::try_from(timestamp.saturating_sub(now).max(0))
        .ok()?
        .checked_mul(1000)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::adapter::NeverCancelled;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
    };

    fn target() -> ProviderTarget {
        ProviderTarget {
            route: RouteId::new("work").unwrap(),
            provider: ProviderId::new("openai").unwrap(),
            endpoint: EndpointId::new("openai").unwrap(),
            account: AccountId::new("work").unwrap(),
            billing_pool: BillingPoolId::new("work").unwrap(),
            protocol: Protocol::OpenAiResponses,
            base_url: "https://api.openai.com".into(),
            model: ModelId {
                vendor: "openai".into(),
                id: "gpt-5.6-sol".into(),
            },
        }
    }

    #[test]
    fn provider_response_total_bytes_and_blocks_are_bounded() {
        let mut limits = ResponseLimits::with_total_bytes(10);
        for line in [b"data".as_slice(), b": ok", b"\n\n"] {
            limits.record_bytes("Test", line.len()).unwrap();
        }
        let bytes = limits.record_bytes("Test", 1).unwrap_err();
        assert_eq!(bytes.class, FailureClass::InvalidStream);

        for blocks in 0..=MAX_RESPONSE_BLOCKS + 1 {
            let result = check_response_block_cap("Test", blocks);
            if blocks <= MAX_RESPONSE_BLOCKS {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().class, FailureClass::InvalidStream);
            }
        }
    }

    #[test]
    fn a_worker_that_never_speaks_trips_the_first_event_deadline() {
        let target = target();
        let error = supervise(
            "OpenAI",
            "zirv-test-stream",
            StreamTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_millis(20),
                idle: Duration::from_secs(1),
            },
            &target,
            &NeverCancelled,
            &mut Vec::new(),
            |_, _| {
                std::thread::sleep(Duration::from_millis(400));
                Err(invalid_stream("unreachable".into()))
            },
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::FirstEventTimeout);
        assert!(error.retry.retryable);
    }

    #[test]
    fn a_wrapped_ureq_timeout_preserves_partial_sse_until_the_next_read() {
        struct Reader {
            polls: u8,
        }
        impl Read for Reader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.polls += 1;
                let chunk: &[u8] = match self.polls {
                    1 => b"data: ",
                    2 => return Err(ureq::Error::Timeout(ureq::Timeout::RecvBody).into_io()),
                    3 => b"hello\n",
                    _ => return Ok(0),
                };
                buffer[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }
        let mut reader = std::io::BufReader::new(Reader { polls: 0 });
        let mut line = String::new();
        read_sse_line(&mut reader, &mut line, "Test", &NeverCancelled, &target()).unwrap();
        assert_eq!(line, "data: hello\n");
    }

    #[test]
    fn retry_after_http_date_is_parsed() {
        assert_eq!(parse_retry_after_ms(" 3 "), Some(3_000));
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_792_567_677);
        assert_eq!(
            parse_retry_after_ms_at("Wed, 21 Oct 2026 07:28:00 GMT", now),
            Some(3_000)
        );
    }
}
