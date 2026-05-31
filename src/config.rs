//! Provides options to configure the behavior of stubborn-io items,
//! specifically related to reconnect behavior.

use crate::strategies::ExpBackoffStrategy;
use std::time::Duration;

/// Boxed iterator yielding the wait durations between reconnection attempts.
pub type DurationIterator = Box<dyn Iterator<Item = Duration> + Send + Sync>;

/// User specified options that control the behavior of the stubborn-io upon disconnect.
pub struct ReconnectOptions {
    /// Represents a function that generates an `Iterator`
    /// to schedule the wait between reconnection attempts.
    pub retries_to_attempt_fn: Box<dyn Fn() -> DurationIterator + Send + Sync>,

    /// If this is set to true, if the initial connect method of the stubborn-io item fails,
    /// then no further reconnects will be attempted.
    pub exit_if_first_connect_fails: bool,

    /// Invoked when the `StubbornIo` establishes a connection.
    pub on_connect_callback: Box<dyn Fn() + Send + Sync>,

    /// Invoked when the `StubbornIo` loses its active connection.
    pub on_disconnect_callback: Box<dyn Fn() + Send + Sync>,

    /// Invoked when the `StubbornIo` fails a connection attempt.
    pub on_connect_fail_callback: Box<dyn Fn() + Send + Sync>,

    /// Identifier for this connection, used in log messages.
    pub connection_name: String,

    /// If this is set to false (default), then the `StubbornIo` will NOT block
    /// on write failures.
    pub block_on_write_failures: bool,
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
            on_connect_callback: Box::new(|| {}),
            on_disconnect_callback: Box::new(|| {}),
            on_connect_fail_callback: Box::new(|| {}),
            connection_name: String::new(),
            block_on_write_failures: false,
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
        I: 'static + Send + Sync + Iterator<Item = Duration>,
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
        self.on_connect_callback = Box::new(cb);
        self
    }

    /// Sets the callback invoked when the active connection is lost.
    #[must_use]
    pub fn with_on_disconnect_callback(mut self, cb: impl Fn() + 'static + Send + Sync) -> Self {
        self.on_disconnect_callback = Box::new(cb);
        self
    }

    /// Sets the callback invoked when a connect/reconnect attempt fails.
    #[must_use]
    pub fn with_on_connect_fail_callback(mut self, cb: impl Fn() + 'static + Send + Sync) -> Self {
        self.on_connect_fail_callback = Box::new(cb);
        self
    }

    /// Sets the human-readable name used in log lines for this connection.
    #[must_use]
    pub fn with_connection_name(mut self, name: impl Into<String>) -> Self {
        self.connection_name = name.into();
        self
    }

    /// Configures whether writes to a disconnected stream return `Poll::Pending`
    /// (block, `true`) or silently report `Ok(buf.len())` (drop, `false`, current default).
    #[must_use]
    pub const fn with_block_on_write_failures(mut self, value: bool) -> Self {
        self.block_on_write_failures = value;
        self
    }
}
