//! Provider-neutral streaming transport plumbing shared by the direct
//! providers (N07 Anthropic, N08 OpenAI).
//!
//! Every direct adapter runs its blocking HTTPS/SSE read on a worker thread
//! and supervises it from the caller's thread so cancellation, first-event
//! and idle deadlines stay observable even while the socket blocks. The
//! bounded event channel is the backpressure seam: a slow consumer stalls
//! the reader instead of growing an unbounded queue.

#![allow(dead_code)] // N09 wires the direct providers into the runtime loop.

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

/// `Retry-After` is delta-seconds in every response these providers send; an
/// HTTP-date form is left unparsed rather than guessed at.
pub(crate) fn parse_retry_after_ms(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
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
    fn retry_after_seconds_convert_and_http_dates_stay_unparsed() {
        assert_eq!(parse_retry_after_ms(" 3 "), Some(3_000));
        assert_eq!(parse_retry_after_ms("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }
}
