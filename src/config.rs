//! Provides options to configure the behavior of stubborn-io items,
//! specifically related to reconnect behavior.

use crate::strategies::ExpBackoffStrategy;
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// Boxed iterator yielding the wait durations between reconnection attempts.
///
/// Only `Send` is required: the iterator is owned and advanced by a single task.
pub type DurationIterator = Box<dyn Iterator<Item = Duration> + Send>;

/// Events emitted by [`StubbornIo`](crate::tokio::StubbornIo) over the lifetime of
/// a connection. Delivered to the single observer installed via
/// [`ReconnectOptions::with_event_callback`].
///
/// Borrowed payloads (e.g. error references) are scoped to the callback invocation;
/// implementations that need to retain data must clone it.
///
/// Non-exhaustive so new variants can be added without breaking existing matches.
#[non_exhaustive]
#[derive(Debug)]
pub enum ReconnectEvent<'a> {
    /// A (re)connection just completed successfully.
    ///
    /// `attempt` is 0 for the initial connect, and `n >= 1` for the n-th reconnect
    /// (or the n-th retry of the initial connect after an initial failure).
    Connected {
        /// 0 = initial connect; >= 1 = (re)connect attempt count.
        attempt: usize,
    },
    /// An established connection was lost; the reconnect machinery is engaging.
    Disconnected,
    /// A connect or reconnect attempt failed. `attempt` is the same counter as
    /// [`Self::Connected::attempt`].
    ConnectFailed {
        /// The error returned by `UnderlyingIo::establish` (or surfaced by the
        /// per-attempt connect timeout).
        error: &'a io::Error,
        /// Which attempt failed (0 = initial).
        attempt: usize,
    },
    /// A reconnect attempt has been scheduled and will run after `delay`.
    ReconnectScheduled {
        /// Which attempt is being scheduled.
        attempt: usize,
        /// How long the machinery will sleep before invoking `establish` again.
        delay: Duration,
    },
    /// A write was issued while the stream was not Connected, and the configured
    /// [`WriteFailurePolicy::DropAndNotify`](crate::config::WriteFailurePolicy)
    /// caused the bytes to be discarded.
    WriteWhileDisconnected {
        /// Number of bytes the caller asked to write and the crate dropped.
        bytes_dropped: usize,
    },
    /// The retries iterator has been exhausted and the stream has entered the
    /// terminal `FailedAndExhausted` state. No further events will be emitted.
    Exhausted,
}

/// Receiver for [`ReconnectEvent`]s. Stored as an `Arc<dyn Fn>` on
/// [`ReconnectOptions`] so it can be cheaply cloned into reconnect futures.
pub type EventCallback = Arc<dyn for<'a> Fn(ReconnectEvent<'a>) + Send + Sync>;

/// How [`StubbornIo`](crate::tokio::StubbornIo) should treat write requests issued
/// while the underlying connection is down (or while a write itself revealed the
/// disconnect).
///
/// Non-exhaustive so new strategies can be added without breaking existing matches.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteFailurePolicy {
    /// Hold the write: return `Poll::Pending` and wake when (re)connection completes.
    /// Caller-side framing semantics are preserved; back-pressure propagates to producers.
    ///
    /// This is the new default in 0.7.0 (a flip from the prior 0.6.x behavior, which
    /// silently dropped writes during disconnect).
    #[default]
    Backpressure,
    /// Pretend the write succeeded (`Poll::Ready(Ok(buf.len()))`) and drop the
    /// bytes on the floor. Schedules a reconnect attempt as a side effect, but
    /// the caller's framing layer will believe those bytes were delivered.
    ///
    /// Only appropriate for fire-and-forget transports where loss is acceptable
    /// and back-pressure is not.
    DropAndNotify,
}

/// User specified options that control the behavior of the stubborn-io upon disconnect.
///
/// All fields are crate-private; configure through the builder methods on this
/// type (`with_*`). The struct is intentionally opaque so the builder remains
/// the single supported API surface.
pub struct ReconnectOptions {
    /// Represents a function that generates an `Iterator`
    /// to schedule the wait between reconnection attempts.
    pub(crate) retries_to_attempt_fn: Box<dyn Fn() -> DurationIterator + Send + Sync>,

    /// If this is set to true, if the initial connect method of the stubborn-io item fails,
    /// then no further reconnects will be attempted.
    pub(crate) exit_if_first_connect_fails: bool,

    /// Invoked for every [`ReconnectEvent`] over the lifetime of this connection.
    /// Defaults to a no-op. See [`Self::with_event_callback`].
    pub(crate) event_callback: EventCallback,

    /// Identifier for this connection, used in log messages.
    ///
    /// Stored as `Arc<str>` so that the formatted log prefix (held internally by
    /// `StubbornIo`) and any clones share a single allocation.
    pub(crate) connection_name: Arc<str>,

    /// Strategy for handling writes that arrive while disconnected, or whose
    /// underlying poll revealed a disconnect. Defaults to
    /// [`WriteFailurePolicy::Backpressure`].
    pub(crate) write_failure_policy: WriteFailurePolicy,

    /// Optional per-attempt connect timeout. When `Some(d)`, each invocation of
    /// [`UnderlyingIo::establish`](crate::tokio::UnderlyingIo::establish) (initial,
    /// initial-retry, and reconnect) is wrapped in `tokio::time::timeout(d, ...)`;
    /// elapsing surfaces as `io::ErrorKind::TimedOut` to the reconnect machinery,
    /// which then schedules the next attempt as if the underlying `establish` had
    /// failed. `None` (default) preserves the prior behavior of waiting forever
    /// on a single attempt.
    pub(crate) connect_timeout: Option<Duration>,
}

impl Default for ReconnectOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ReconnectOptions {
    /// By default, the stubborn-io will not try to reconnect if the first connect attempt fails.
    ///
    /// By default, the retries iterator waits longer and longer between reconnection attempts,
    /// until it eventually perpetually tries to reconnect every 30 minutes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            retries_to_attempt_fn: Box::new(|| Box::new(ExpBackoffStrategy::default().into_iter())),
            exit_if_first_connect_fails: false,
            event_callback: Arc::new(|_| {}),
            connection_name: Arc::from(""),
            write_failure_policy: WriteFailurePolicy::Backpressure,
            connect_timeout: None,
        }
    }

    /// This convenience function allows the user to provide any function that returns a value
    /// that is convertible into an iterator, such as an actual iterator or a `Vec`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use sdre_stubborn_io::ReconnectOptions;
    ///
    /// // With the below vector, the stubborn-io item will try to reconnect three times,
    /// // waiting 2 seconds between each attempt. Once all three tries are exhausted,
    /// // it will stop attempting.
    /// let options = ReconnectOptions::new().with_retries_generator(|| {
    ///     vec![
    ///         Duration::from_secs(2),
    ///         Duration::from_secs(2),
    ///         Duration::from_secs(2),
    ///     ]
    /// });
    /// ```
    #[must_use]
    pub fn with_retries_generator<F, I, IN>(mut self, retries_generator: F) -> Self
    where
        F: 'static + Send + Sync + Fn() -> IN,
        I: 'static + Send + Iterator<Item = Duration>,
        IN: IntoIterator<IntoIter = I, Item = Duration>,
    {
        self.retries_to_attempt_fn = Box::new(move || Box::new(retries_generator().into_iter()));
        self
    }

    /// Configures whether to give up after the first connect attempt fails.
    #[must_use]
    pub const fn with_exit_if_first_connect_fails(mut self, value: bool) -> Self {
        self.exit_if_first_connect_fails = value;
        self
    }

    /// Sets the single observer invoked for every [`ReconnectEvent`].
    ///
    /// Replaces the prior `with_on_connect_callback` /
    /// `with_on_disconnect_callback` / `with_on_connect_fail_callback` trio.
    /// The callback is stored in an `Arc` and is cloned into each scheduled
    /// reconnect future, so it must be `Send + Sync + 'static`.
    #[must_use]
    pub fn with_event_callback(
        mut self,
        cb: impl for<'a> Fn(ReconnectEvent<'a>) + Send + Sync + 'static,
    ) -> Self {
        self.event_callback = Arc::new(cb);
        self
    }

    /// Sets the human-readable name used in log lines for this connection.
    ///
    /// Accepts anything convertible into `Arc<str>` — `&str` and `String`
    /// allocate; an existing `Arc<str>` is moved without copying.
    #[must_use]
    pub fn with_connection_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.connection_name = name.into();
        self
    }

    /// Configures how writes are handled while disconnected. See
    /// [`WriteFailurePolicy`] for the semantics of each variant. Defaults to
    /// [`WriteFailurePolicy::Backpressure`] in 0.7.0 (flipped from prior
    /// drop-on-floor default).
    #[must_use]
    pub const fn with_write_failure_policy(mut self, policy: WriteFailurePolicy) -> Self {
        self.write_failure_policy = policy;
        self
    }

    /// Sets a per-attempt timeout applied to every call to
    /// [`UnderlyingIo::establish`](crate::tokio::UnderlyingIo::establish). Pass
    /// `None` to disable (default).
    #[must_use]
    pub const fn with_connect_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.connect_timeout = timeout;
        self
    }
}

/// Build the formatted log prefix for a given connection name.
///
/// Internal helper; the result is cached once per `StubbornIo` at construction.
#[must_use]
pub(crate) fn format_log_prefix(name: &str) -> Arc<str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        Arc::from("StubbornIo: ")
    } else {
        Arc::from(format!("StubbornIo({trimmed}): "))
    }
}
