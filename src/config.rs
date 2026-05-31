//! Provides options to configure the behavior of stubborn-io items,
//! specifically related to reconnect behavior.

use crate::strategies::ExpBackoffStrategy;
use std::sync::Arc;
use std::time::Duration;

/// Boxed iterator yielding the wait durations between reconnection attempts.
///
/// Only `Send` is required: the iterator is owned and advanced by a single task.
pub type DurationIterator = Box<dyn Iterator<Item = Duration> + Send>;

/// User specified options that control the behavior of the stubborn-io upon disconnect.
pub struct ReconnectOptions {
    /// Represents a function that generates an `Iterator`
    /// to schedule the wait between reconnection attempts.
    pub retries_to_attempt_fn: Box<dyn Fn() -> DurationIterator + Send + Sync>,

    /// If this is set to true, if the initial connect method of the stubborn-io item fails,
    /// then no further reconnects will be attempted.
    pub exit_if_first_connect_fails: bool,

    /// Invoked when the `StubbornIo` establishes a connection.
    pub on_connect_callback: Arc<dyn Fn() + Send + Sync>,

    /// Invoked when the `StubbornIo` loses its active connection.
    pub on_disconnect_callback: Arc<dyn Fn() + Send + Sync>,

    /// Invoked when the `StubbornIo` fails a connection attempt.
    pub on_connect_fail_callback: Arc<dyn Fn() + Send + Sync>,

    /// Identifier for this connection, used in log messages.
    ///
    /// Stored as `Arc<str>` so that the formatted log prefix (held internally by
    /// `StubbornIo`) and any clones share a single allocation.
    pub connection_name: Arc<str>,

    /// If this is set to false (default), then the `StubbornIo` will NOT block
    /// on write failures.
    pub block_on_write_failures: bool,

    /// Optional per-attempt connect timeout. When `Some(d)`, each invocation of
    /// [`UnderlyingIo::establish`](crate::tokio::UnderlyingIo::establish) (initial,
    /// initial-retry, and reconnect) is wrapped in `tokio::time::timeout(d, ...)`;
    /// elapsing surfaces as `io::ErrorKind::TimedOut` to the reconnect machinery,
    /// which then schedules the next attempt as if the underlying `establish` had
    /// failed. `None` (default) preserves the prior behavior of waiting forever
    /// on a single attempt.
    pub connect_timeout: Option<Duration>,
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
            exit_if_first_connect_fails: true,
            on_connect_callback: Arc::new(|| {}),
            on_disconnect_callback: Arc::new(|| {}),
            on_connect_fail_callback: Arc::new(|| {}),
            connection_name: Arc::from(""),
            block_on_write_failures: false,
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

    /// Sets the callback invoked on every successful (re)connect.
    #[must_use]
    pub fn with_on_connect_callback(mut self, cb: impl Fn() + 'static + Send + Sync) -> Self {
        self.on_connect_callback = Arc::new(cb);
        self
    }

    /// Sets the callback invoked when the active connection is lost.
    #[must_use]
    pub fn with_on_disconnect_callback(mut self, cb: impl Fn() + 'static + Send + Sync) -> Self {
        self.on_disconnect_callback = Arc::new(cb);
        self
    }

    /// Sets the callback invoked when a connect/reconnect attempt fails.
    #[must_use]
    pub fn with_on_connect_fail_callback(mut self, cb: impl Fn() + 'static + Send + Sync) -> Self {
        self.on_connect_fail_callback = Arc::new(cb);
        self
    }

    /// Sets the human-readable name used in log lines for this connection.
    #[must_use]
    pub fn with_connection_name(mut self, name: impl AsRef<str>) -> Self {
        self.connection_name = Arc::from(name.as_ref());
        self
    }

    /// Configures whether writes to a disconnected stream return `Poll::Pending`
    /// (block, `true`) or silently report `Ok(buf.len())` (drop, `false`, current default).
    #[must_use]
    pub const fn with_block_on_write_failures(mut self, value: bool) -> Self {
        self.block_on_write_failures = value;
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
