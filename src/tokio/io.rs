use crate::config::{ReconnectEvent, ReconnectOptions, WriteFailurePolicy, format_log_prefix};
use log::{error, info, warn};
use std::future::Future;
use std::io::{self, ErrorKind, IoSlice};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{sleep, timeout};

/// Run `establish` with the optional per-attempt timeout from `ReconnectOptions`.
/// Elapsed timeouts surface as `io::ErrorKind::TimedOut` so the reconnect machinery
/// treats them as a failed attempt and proceeds to the next backoff step.
async fn establish_with_timeout<T: UnderlyingIo>(
    ctx: T::Context,
    deadline: Option<Duration>,
) -> io::Result<T> {
    if let Some(d) = deadline {
        timeout(d, T::establish(ctx)).await.unwrap_or_else(|_| {
            Err(io::Error::new(
                ErrorKind::TimedOut,
                "connect attempt exceeded configured connect_timeout",
            ))
        })
    } else {
        T::establish(ctx).await
    }
}

/// Trait that should be implemented for an [`AsyncRead`] and/or [`AsyncWrite`]
/// item to enable it to work with the [`StubbornIo`] struct.
///
/// Each implementer declares its own [`Context`](UnderlyingIo::Context) type — the
/// caller-supplied value handed to [`establish`](UnderlyingIo::establish) on every
/// (re)connect. Implementers that need no context should set `type Context = ();`
/// explicitly; no default is provided.
pub trait UnderlyingIo: Sized + Unpin {
    /// The caller-supplied value passed to [`Self::establish`] for every
    /// (re)connect attempt. Must be cloneable so it can be reused across attempts.
    type Context: Clone + Send + Unpin + 'static;

    /// The creation function is used by `StubbornIo` in order to establish both the initial IO connection
    /// in addition to performing reconnects.
    ///
    /// This is also the canonical hook for any **post-connect configuration**
    /// that must be re-applied on every reconnect — e.g. TCP keepalive,
    /// `TCP_NODELAY`, socket buffer sizes, TLS handshake parameters, or
    /// application-level handshakes. Wrap the underlying constructor inside
    /// `establish` and apply the desired configuration on the freshly-built
    /// value before returning it. Configuration applied externally (e.g. via
    /// `Deref` on a [`StubbornIo`]) is silently lost on reconnect; configuration
    /// applied here is not.
    fn establish(ctx: Self::Context) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>>;

    /// When IO items experience an [`io::Error`] during operation, it does not necessarily mean
    /// it is a disconnect/termination (ex: `WouldBlock`). This trait provides sensible defaults to classify
    /// which errors are considered "disconnects", but this can be overridden based on the user's needs.
    ///
    /// The default set is the union of error kinds that any IO transport is likely to surface on
    /// a real disconnect: connection-lifecycle kinds, network-path kinds, write/read termination
    /// kinds, and timeouts. Transports that legitimately encounter some of these in normal
    /// operation should override this method (e.g. `TcpStream` overrides to drop `UnexpectedEof`,
    /// which it can never observe directly).
    fn is_disconnect_error(&self, err: &io::Error) -> bool {
        matches!(
            err.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::AddrInUse
                | ErrorKind::AddrNotAvailable
                | ErrorKind::BrokenPipe
                | ErrorKind::TimedOut
                | ErrorKind::UnexpectedEof
                | ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::NetworkDown
        )
    }

    /// If the underlying IO item implements `AsyncRead`, this method allows the user to specify
    /// if a technically successful read actually means that the connect is closed.
    /// For example, tokio's `TcpStream` successfully performs a read of 0 bytes when closed.
    fn is_final_read(&self, bytes_read: usize) -> bool {
        // definitely true for tcp, perhaps true for other io as well,
        // indicative of EOF hit
        bytes_read == 0
    }
}

struct AttemptsTracker {
    attempt_num: usize,
    retries_remaining: Box<dyn Iterator<Item = Duration> + Send>,
}

struct ReconnectStatus<T: UnderlyingIo> {
    attempts_tracker: AttemptsTracker,
    /// `None` while no reconnect has been scheduled yet; replaced by `on_disconnect`
    /// before any poll on this status occurs.
    reconnect_attempt: Option<Pin<Box<dyn Future<Output = io::Result<T>> + Send>>>,
    _phantom_data: PhantomData<T::Context>,
}

impl<T> ReconnectStatus<T>
where
    T: UnderlyingIo,
{
    pub(crate) fn new(options: &ReconnectOptions) -> Self {
        Self {
            attempts_tracker: AttemptsTracker {
                attempt_num: 0,
                retries_remaining: (options.retries_to_attempt_fn)(),
            },
            reconnect_attempt: None,
            _phantom_data: PhantomData,
        }
    }
}

/// Wrapper over a tokio `AsyncRead`/`AsyncWrite` item that will automatically
/// invoke the [`UnderlyingIo::establish`] upon initialization and when a reconnect is needed.
///
/// Because it implements deref, you are able to invoke all of the original methods on the wrapped IO.
pub struct StubbornIo<T: UnderlyingIo> {
    status: Status<T>,
    underlying_io: T,
    options: ReconnectOptions,
    ctor_arg: T::Context,
    /// Pre-formatted log prefix (e.g. `StubbornIo(foo): `), cached once at construction.
    log_prefix: Arc<str>,
}

enum Status<T: UnderlyingIo> {
    Connected,
    Disconnected(ReconnectStatus<T>),
    FailedAndExhausted,
    /// Terminal state entered after a successful (or errored) `poll_shutdown`.
    /// No further reconnects will be attempted; subsequent reads/writes/shutdowns
    /// return `io::ErrorKind::NotConnected`.
    Closed,
}

#[inline]
fn poll_err<T>(
    kind: ErrorKind,
    reason: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> Poll<io::Result<T>> {
    let io_err = io::Error::new(kind, reason);
    Poll::Ready(Err(io_err))
}

fn exhausted_err<T>() -> Poll<io::Result<T>> {
    poll_err(
        ErrorKind::NotConnected,
        "Disconnected. Connection attempts have been exhausted.",
    )
}

fn closed_err<T>() -> Poll<io::Result<T>> {
    poll_err(
        ErrorKind::NotConnected,
        "Stream has been explicitly shut down.",
    )
}

impl<T: UnderlyingIo> Deref for StubbornIo<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.underlying_io
    }
}

impl<T: UnderlyingIo> DerefMut for StubbornIo<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.underlying_io
    }
}

impl<T> StubbornIo<T>
where
    T: UnderlyingIo,
{
    /// Connects or creates a handle to the `UnderlyingIo` item,
    /// using the default reconnect options.
    pub async fn connect(ctor_arg: T::Context) -> io::Result<Self> {
        let options = ReconnectOptions::new();
        Self::connect_with_options(ctor_arg, options).await
    }

    /// Returns the connection name as it will appear in log messages, e.g. `StubbornIo(foo): `.
    #[must_use]
    pub fn get_connection_name(&self) -> String {
        (*self.log_prefix).to_string()
    }

    /// Returns the configured [`WriteFailurePolicy`].
    #[must_use]
    pub const fn get_write_failure_policy(&self) -> WriteFailurePolicy {
        self.options.write_failure_policy
    }

    /// Returns `true` if the stream is currently connected and ready for I/O.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self.status, Status::Connected)
    }

    /// Returns `true` if the stream is in a terminal state and will never reconnect
    /// (retries exhausted, or the stream has been explicitly shut down).
    #[must_use]
    pub const fn is_terminated(&self) -> bool {
        matches!(self.status, Status::FailedAndExhausted | Status::Closed)
    }

    /// Returns `true` if the stream has been explicitly shut down via
    /// `AsyncWrite::poll_shutdown` (or `shutdown().await`). Distinct from
    /// [`Self::is_terminated`], which also covers retry exhaustion.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        matches!(self.status, Status::Closed)
    }

    /// Connects (or attempts to reconnect) using the supplied [`ReconnectOptions`].
    pub async fn connect_with_options(
        ctor_arg: T::Context,
        options: ReconnectOptions,
    ) -> io::Result<Self> {
        let log_prefix = format_log_prefix(&options.connection_name);
        let emit = |ev: ReconnectEvent<'_>| (options.event_callback)(ev);

        let tcp = match establish_with_timeout::<T>(ctor_arg.clone(), options.connect_timeout).await
        {
            Ok(tcp) => {
                info!("{log_prefix}Initial connection succeeded.");
                emit(ReconnectEvent::Connected { attempt: 0 });
                tcp
            }
            Err(e) => {
                warn!("{log_prefix}Initial connection failed due to: {e:?}.");
                emit(ReconnectEvent::ConnectFailed {
                    error: &e,
                    attempt: 0,
                });

                if options.exit_if_first_connect_fails {
                    error!("{log_prefix}Bailing after initial connection failure.");
                    return Err(e);
                }

                let mut result = Err(e);

                for (i, duration) in (options.retries_to_attempt_fn)().enumerate() {
                    let reconnect_num = i + 1;

                    emit(ReconnectEvent::ReconnectScheduled {
                        attempt: reconnect_num,
                        delay: duration,
                    });
                    warn!(
                        "{log_prefix}Will re-perform initial connect attempt #{reconnect_num} in {duration:?}."
                    );

                    sleep(duration).await;

                    info!("{log_prefix}Attempting reconnect #{reconnect_num} now.");

                    match establish_with_timeout::<T>(ctor_arg.clone(), options.connect_timeout)
                        .await
                    {
                        Ok(tcp) => {
                            emit(ReconnectEvent::Connected {
                                attempt: reconnect_num,
                            });
                            info!("{log_prefix}Initial connection successfully established.");
                            result = Ok(tcp);
                            break;
                        }
                        Err(e) => {
                            emit(ReconnectEvent::ConnectFailed {
                                error: &e,
                                attempt: reconnect_num,
                            });
                            result = Err(e);
                        }
                    }
                }

                match result {
                    Ok(tcp) => tcp,
                    Err(e) => {
                        emit(ReconnectEvent::Exhausted);
                        error!(
                            "{log_prefix}No more re-connect retries remaining. Never able to establish initial connection."
                        );
                        return Err(e);
                    }
                }
            }
        };

        Ok(Self {
            status: Status::Connected,
            ctor_arg,
            underlying_io: tcp,
            options,
            log_prefix,
        })
    }

    fn on_disconnect(mut self: Pin<&mut Self>, cx: &Context<'_>) {
        let prefix = Arc::clone(&self.log_prefix);
        match &mut self.status {
            // initial disconnect
            Status::Connected => {
                error!("{prefix}Disconnect occurred");
                (self.options.event_callback)(ReconnectEvent::Disconnected);
                self.status = Status::Disconnected(ReconnectStatus::new(&self.options));
            }
            // Already disconnected; a previous reconnect attempt failed. The
            // ConnectFailed event was emitted at the call site (poll_disconnect)
            // where the error is in scope. No additional emit here.
            Status::Disconnected(_) => {}
            Status::FailedAndExhausted | Status::Closed => {
                unreachable!("{prefix}on_disconnect will not occur for already-terminal state.")
            }
        }

        let ctor_arg = self.ctor_arg.clone();
        let connect_timeout = self.options.connect_timeout;

        // this is ensured to be true now
        if let Status::Disconnected(reconnect_status) = &mut self.status {
            let Some(next_duration) = reconnect_status.attempts_tracker.retries_remaining.next()
            else {
                error!("{prefix}No more re-connect retries remaining. Giving up.");
                (self.options.event_callback)(ReconnectEvent::Exhausted);
                self.status = Status::FailedAndExhausted;
                cx.waker().wake_by_ref();
                return;
            };

            let future_instant = sleep(next_duration);

            reconnect_status.attempts_tracker.attempt_num += 1;
            let cur_num = reconnect_status.attempts_tracker.attempt_num;
            let log_prefix = Arc::clone(&prefix);

            let reconnect_attempt = async move {
                future_instant.await;
                info!("{log_prefix}Attempting reconnect #{cur_num} now.");
                establish_with_timeout::<T>(ctor_arg, connect_timeout).await
            };

            reconnect_status.reconnect_attempt = Some(Box::pin(reconnect_attempt));

            info!("{prefix}Will perform reconnect attempt #{cur_num} in {next_duration:?}.");
            (self.options.event_callback)(ReconnectEvent::ReconnectScheduled {
                attempt: cur_num,
                delay: next_duration,
            });

            cx.waker().wake_by_ref();
        }
    }

    fn poll_disconnect(mut self: Pin<&mut Self>, cx: &mut Context<'_>) {
        let prefix = Arc::clone(&self.log_prefix);
        let (attempt, attempt_num) = match &mut self.status {
            Status::Connected | Status::FailedAndExhausted | Status::Closed => unreachable!(),
            Status::Disconnected(status) => {
                let Some(fut) = status.reconnect_attempt.as_mut() else {
                    // No attempt scheduled yet; on_disconnect will populate it.
                    return;
                };
                (Pin::new(fut), status.attempts_tracker.attempt_num)
            }
        };

        match attempt.poll(cx) {
            Poll::Ready(Ok(underlying_io)) => {
                info!("{prefix}Connection re-established");
                cx.waker().wake_by_ref();
                self.status = Status::Connected;
                (self.options.event_callback)(ReconnectEvent::Connected {
                    attempt: attempt_num,
                });
                self.underlying_io = underlying_io;
            }
            Poll::Ready(Err(err)) => {
                warn!("{prefix}Connection attempt #{attempt_num} failed: {err:?}");
                (self.options.event_callback)(ReconnectEvent::ConnectFailed {
                    error: &err,
                    attempt: attempt_num,
                });
                self.on_disconnect(cx);
            }
            Poll::Pending => {}
        }
    }

    fn is_read_disconnect_detected(
        &self,
        poll_result: &Poll<io::Result<()>>,
        bytes_read: usize,
    ) -> bool {
        match poll_result {
            Poll::Ready(Ok(())) if self.is_final_read(bytes_read) => true,
            Poll::Ready(Err(err)) => self.is_disconnect_error(err),
            _ => false,
        }
    }

    fn is_write_disconnect_detected<X>(&self, poll_result: &Poll<io::Result<X>>) -> bool {
        match poll_result {
            Poll::Ready(Err(err)) => self.is_disconnect_error(err),
            _ => false,
        }
    }
}

impl<T> AsyncRead for StubbornIo<T>
where
    T: UnderlyingIo + AsyncRead,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.status {
            Status::Connected => {
                let pre_len = buf.filled().len();
                let poll = AsyncRead::poll_read(Pin::new(&mut self.underlying_io), cx, buf);
                let post_len = buf.filled().len();
                let bytes_read = post_len - pre_len;
                if self.is_read_disconnect_detected(&poll, bytes_read) {
                    self.on_disconnect(cx);
                    Poll::Pending
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => {
                self.poll_disconnect(cx);
                Poll::Pending
            }
            Status::FailedAndExhausted => exhausted_err(),
            Status::Closed => closed_err(),
        }
    }
}

impl<T> AsyncWrite for StubbornIo<T>
where
    T: UnderlyingIo + AsyncWrite,
{
    /// Writes to the underlying IO item.
    ///
    /// If a write reveals a disconnect (or one is already in progress), behavior
    /// depends on [`WriteFailurePolicy`]:
    ///
    /// * `Backpressure` (default): return `Poll::Pending`, hold the buffer, wake
    ///   when (re)connection completes.
    /// * `DropAndNotify`: return `Poll::Ready(Ok(buf.len()))` to keep the caller's
    ///   framing layer moving, while the bytes themselves are discarded. The
    ///   reconnect machinery is engaged either way.
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let prefix = Arc::clone(&self.log_prefix);
        let policy = self.get_write_failure_policy();
        match &mut self.status {
            Status::Connected => {
                let poll = AsyncWrite::poll_write(Pin::new(&mut self.underlying_io), cx, buf);

                if self.is_write_disconnect_detected(&poll) {
                    match policy {
                        WriteFailurePolicy::Backpressure => {
                            warn!("{prefix}Write disconnect detected. Applying back-pressure");
                            self.on_disconnect(cx);
                            Poll::Pending
                        }
                        WriteFailurePolicy::DropAndNotify => {
                            error!(
                                "{prefix}Write disconnect detected. Dropping {} byte(s)",
                                buf.len()
                            );
                            (self.options.event_callback)(ReconnectEvent::WriteWhileDisconnected {
                                bytes_dropped: buf.len(),
                            });
                            self.on_disconnect(cx);
                            Poll::Ready(Ok(buf.len()))
                        }
                    }
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => match policy {
                WriteFailurePolicy::Backpressure => {
                    warn!("{prefix}Write while disconnected. Applying back-pressure");
                    self.poll_disconnect(cx);
                    Poll::Pending
                }
                WriteFailurePolicy::DropAndNotify => {
                    error!(
                        "{prefix}Write while disconnected. Dropping {} byte(s)",
                        buf.len()
                    );
                    (self.options.event_callback)(ReconnectEvent::WriteWhileDisconnected {
                        bytes_dropped: buf.len(),
                    });
                    self.poll_disconnect(cx);
                    Poll::Ready(Ok(buf.len()))
                }
            },
            Status::FailedAndExhausted => exhausted_err(),
            Status::Closed => closed_err(),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.status {
            Status::Connected => {
                let poll = AsyncWrite::poll_flush(Pin::new(&mut self.underlying_io), cx);

                if self.is_write_disconnect_detected(&poll) {
                    self.on_disconnect(cx);
                    Poll::Pending
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => {
                self.poll_disconnect(cx);
                Poll::Pending
            }
            Status::FailedAndExhausted => exhausted_err(),
            Status::Closed => closed_err(),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.status {
            Status::Connected => {
                let poll = AsyncWrite::poll_shutdown(Pin::new(&mut self.underlying_io), cx);
                if poll.is_ready() {
                    // Whether the shutdown succeeded or errored, the caller has
                    // expressed intent to close. Transition to the terminal
                    // Closed state so we never reconnect, and so that further
                    // ops surface a clean NotConnected (rather than triggering
                    // a reconnect via on_disconnect).
                    self.status = Status::Closed;
                }

                poll
            }
            // A disconnected stream that the caller now wants closed is
            // semantically already "closed enough" — transition and report it.
            Status::Disconnected(_) => {
                self.status = Status::Closed;
                closed_err()
            }
            Status::FailedAndExhausted => exhausted_err(),
            Status::Closed => closed_err(),
        }
    }

    /// Vectored variant of [`Self::poll_write`]; same policy semantics apply.
    ///
    /// Under `DropAndNotify` the returned count is the sum of all input buffer
    /// lengths — i.e. the bytes the caller asked to write — which keeps the
    /// caller's framing cursor advancing. Those bytes are not actually
    /// transmitted; this is the documented drop semantic of the policy.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let prefix = Arc::clone(&self.log_prefix);
        let policy = self.get_write_failure_policy();
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        match &mut self.status {
            Status::Connected => {
                let poll =
                    AsyncWrite::poll_write_vectored(Pin::new(&mut self.underlying_io), cx, bufs);

                if self.is_write_disconnect_detected(&poll) {
                    match policy {
                        WriteFailurePolicy::Backpressure => {
                            warn!("{prefix}Write disconnect detected. Applying back-pressure");
                            self.on_disconnect(cx);
                            Poll::Pending
                        }
                        WriteFailurePolicy::DropAndNotify => {
                            error!(
                                "{prefix}Write disconnect detected. Dropping {total} byte(s) across {} buffer(s)",
                                bufs.len()
                            );
                            (self.options.event_callback)(ReconnectEvent::WriteWhileDisconnected {
                                bytes_dropped: total,
                            });
                            self.on_disconnect(cx);
                            Poll::Ready(Ok(total))
                        }
                    }
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => match policy {
                WriteFailurePolicy::Backpressure => {
                    warn!("{prefix}Write while disconnected. Applying back-pressure");
                    self.poll_disconnect(cx);
                    Poll::Pending
                }
                WriteFailurePolicy::DropAndNotify => {
                    error!(
                        "{prefix}Write while disconnected. Dropping {total} byte(s) across {} buffer(s)",
                        bufs.len()
                    );
                    (self.options.event_callback)(ReconnectEvent::WriteWhileDisconnected {
                        bytes_dropped: total,
                    });
                    self.poll_disconnect(cx);
                    Poll::Ready(Ok(total))
                }
            },
            Status::FailedAndExhausted => exhausted_err(),
            Status::Closed => closed_err(),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.underlying_io.is_write_vectored()
    }
}
