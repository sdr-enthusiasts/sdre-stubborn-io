use crate::config::{ReconnectOptions, format_log_prefix};
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

fn disconnected_err<T>() -> Poll<io::Result<T>> {
    poll_err(ErrorKind::NotConnected, "Underlying I/O is disconnected.")
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

    /// Returns the current `block_on_write_failures` setting from `ReconnectOptions`.
    #[must_use]
    pub const fn get_block_on_write_failures(&self) -> bool {
        self.options.block_on_write_failures
    }

    /// Returns `true` if the stream is currently connected and ready for I/O.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self.status, Status::Connected)
    }

    /// Returns `true` if the stream is in a terminal state and will never reconnect
    /// (currently: retries exhausted).
    #[must_use]
    pub const fn is_terminated(&self) -> bool {
        matches!(self.status, Status::FailedAndExhausted)
    }

    /// Connects (or attempts to reconnect) using the supplied [`ReconnectOptions`].
    pub async fn connect_with_options(
        ctor_arg: T::Context,
        options: ReconnectOptions,
    ) -> io::Result<Self> {
        let log_prefix = format_log_prefix(&options.connection_name);
        let tcp = match establish_with_timeout::<T>(ctor_arg.clone(), options.connect_timeout).await
        {
            Ok(tcp) => {
                info!("{log_prefix}Initial connection succeeded.");
                (options.on_connect_callback)();
                tcp
            }
            Err(e) => {
                error!("{log_prefix}Initial connection failed due to: {e:?}.");
                (options.on_connect_fail_callback)();

                if options.exit_if_first_connect_fails {
                    error!("{log_prefix}Bailing after initial connection failure.");
                    return Err(e);
                }

                let mut result = Err(e);

                for (i, duration) in (options.retries_to_attempt_fn)().enumerate() {
                    let reconnect_num = i + 1;

                    info!(
                        "{log_prefix}Will re-perform initial connect attempt #{reconnect_num} in {duration:?}."
                    );

                    sleep(duration).await;

                    info!("{log_prefix}Attempting reconnect #{reconnect_num} now.");

                    match establish_with_timeout::<T>(ctor_arg.clone(), options.connect_timeout)
                        .await
                    {
                        Ok(tcp) => {
                            result = Ok(tcp);
                            (options.on_connect_callback)();
                            info!("{log_prefix}Initial connection successfully established.");
                            break;
                        }
                        Err(e) => {
                            (options.on_connect_fail_callback)();
                            result = Err(e);
                        }
                    }
                }

                match result {
                    Ok(tcp) => tcp,
                    Err(e) => {
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

    #[allow(clippy::needless_pass_by_ref_mut)] // cx will become &Context in D7 rework
    fn on_disconnect(mut self: Pin<&mut Self>, cx: &mut Context<'_>) {
        let prefix = Arc::clone(&self.log_prefix);
        match &mut self.status {
            // initial disconnect
            Status::Connected => {
                error!("{prefix}Disconnect occurred");
                (self.options.on_disconnect_callback)();
                self.status = Status::Disconnected(ReconnectStatus::new(&self.options));
            }
            Status::Disconnected(_) => {
                (self.options.on_connect_fail_callback)();
            }
            Status::FailedAndExhausted => {
                unreachable!("{prefix}on_disconnect will not occur for already exhausted state.")
            }
        }

        let ctor_arg = self.ctor_arg.clone();
        let connect_timeout = self.options.connect_timeout;

        // this is ensured to be true now
        if let Status::Disconnected(reconnect_status) = &mut self.status {
            let Some(next_duration) = reconnect_status.attempts_tracker.retries_remaining.next()
            else {
                error!("{prefix}No more re-connect retries remaining. Giving up.");
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

            cx.waker().wake_by_ref();
        }
    }

    fn poll_disconnect(mut self: Pin<&mut Self>, cx: &mut Context<'_>) {
        let prefix = Arc::clone(&self.log_prefix);
        let (attempt, attempt_num) = match &mut self.status {
            Status::Connected | Status::FailedAndExhausted => unreachable!(),
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
                (self.options.on_connect_callback)();
                self.underlying_io = underlying_io;
            }
            Poll::Ready(Err(err)) => {
                error!("{prefix}Connection attempt #{attempt_num} failed: {err:?}");
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
        }
    }
}

impl<T> AsyncWrite for StubbornIo<T>
where
    T: UnderlyingIo + AsyncWrite,
{
    /// Method for writing to the underlying IO item.
    /// If the write results in a disconnect: when `ReconnectOptions::block_on_write_failures` is true,
    /// `Poll::Pending` is returned to the caller and the buffer is held. Otherwise, the write is skipped.
    /// No error is returned to the caller.
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let prefix = Arc::clone(&self.log_prefix);
        match &mut self.status {
            Status::Connected => {
                let poll = AsyncWrite::poll_write(Pin::new(&mut self.underlying_io), cx, buf);

                if self.is_write_disconnect_detected(&poll) {
                    if self.get_block_on_write_failures() {
                        warn!("{prefix}Write disconnect detected. Blocking on write");
                        self.on_disconnect(cx);
                        Poll::Pending
                    } else {
                        error!("{prefix}Write disconnect detected. Skipping message");
                        self.on_disconnect(cx);
                        Poll::Ready(Ok(buf.len()))
                    }
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => {
                if self.get_block_on_write_failures() {
                    warn!("{prefix}Write disconnect detected. Blocking on write");
                    self.poll_disconnect(cx);
                    Poll::Pending
                } else {
                    error!("{prefix}Write disconnect detected. Skipping Message");
                    self.poll_disconnect(cx);
                    Poll::Ready(Ok(buf.len()))
                }
            }
            Status::FailedAndExhausted => exhausted_err(),
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
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.status {
            Status::Connected => {
                let poll = AsyncWrite::poll_shutdown(Pin::new(&mut self.underlying_io), cx);
                if poll.is_ready() {
                    // if completed, we are disconnected whether error or not
                    self.on_disconnect(cx);
                }

                poll
            }
            Status::Disconnected(_) => disconnected_err(),
            Status::FailedAndExhausted => exhausted_err(),
        }
    }

    /// Method for writing to the underlying IO item.
    /// If the write results in a disconnect: when `ReconnectOptions::block_on_write_failures` is true,
    /// `Poll::Pending` is returned to the caller and the buffer is held. Otherwise, the write is skipped.
    /// No error is returned to the caller.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let prefix = Arc::clone(&self.log_prefix);
        match &mut self.status {
            Status::Connected => {
                let poll =
                    AsyncWrite::poll_write_vectored(Pin::new(&mut self.underlying_io), cx, bufs);

                if self.is_write_disconnect_detected(&poll) {
                    if self.get_block_on_write_failures() {
                        warn!("{prefix}Write disconnect detected. Blocking on write");
                        self.on_disconnect(cx);
                        Poll::Pending
                    } else {
                        error!("{prefix}Write disconnect detected. Skipping message");
                        self.on_disconnect(cx);
                        Poll::Ready(Ok(bufs.iter().map(|buf| buf.len()).sum()))
                    }
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => {
                if self.get_block_on_write_failures() {
                    warn!("{prefix}Write disconnect detected. Blocking on write");
                    self.poll_disconnect(cx);
                    Poll::Pending
                } else {
                    error!("{prefix}Write disconnect detected. Skipping Message");
                    self.poll_disconnect(cx);
                    Poll::Ready(Ok(bufs.iter().map(|buf| buf.len()).sum()))
                }
            }
            Status::FailedAndExhausted => exhausted_err(),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.underlying_io.is_write_vectored()
    }
}
