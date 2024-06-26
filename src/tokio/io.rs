use crate::config::ReconnectOptions;
use log::{error, info, warn};
use std::future::Future;
use std::io::{self, ErrorKind, IoSlice};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::sleep;

/// Trait that should be implemented for an [AsyncRead] and/or [AsyncWrite]
/// item to enable it to work with the [StubbornIo] struct.
pub trait UnderlyingIo<C>: Sized + Unpin
where
    C: Clone + Send + Unpin,
{
    /// The creation function is used by StubbornIo in order to establish both the initial IO connection
    /// in addition to performing reconnects.
    fn establish(ctor_arg: C) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>>;

    /// When IO items experience an [io::Error](io::Error) during operation, it does not necessarily mean
    /// it is a disconnect/termination (ex: WouldBlock). This trait provides sensible defaults to classify
    /// which errors are considered "disconnects", but this can be overridden based on the user's needs.
    fn is_disconnect_error(&self, err: &io::Error) -> bool {
        use std::io::ErrorKind::*;

        matches!(
            err.kind(),
            NotFound
                | PermissionDenied
                | ConnectionRefused
                | ConnectionReset
                | ConnectionAborted
                | NotConnected
                | AddrInUse
                | AddrNotAvailable
                | BrokenPipe
                | AlreadyExists
        )
    }

    /// If the underlying IO item implements AsyncRead, this method allows the user to specify
    /// if a technically successful read actually means that the connect is closed.
    /// For example, tokio's TcpStream successfully performs a read of 0 bytes when closed.
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

struct ReconnectStatus<T, C> {
    attempts_tracker: AttemptsTracker,
    reconnect_attempt: Pin<Box<dyn Future<Output = io::Result<T>> + Send>>,
    _phantom_data: PhantomData<C>,
}

impl<T, C> ReconnectStatus<T, C>
where
    T: UnderlyingIo<C>,
    C: Clone + Send + Unpin + 'static,
{
    pub fn new(options: &ReconnectOptions) -> Self {
        ReconnectStatus {
            attempts_tracker: AttemptsTracker {
                attempt_num: 0,
                retries_remaining: (options.retries_to_attempt_fn)(),
            },
            reconnect_attempt: Box::pin(async { unreachable!("Not going to happen") }),
            _phantom_data: PhantomData,
        }
    }
}

/// The StubbornIo is a wrapper over a tokio AsyncRead/AsyncWrite item that will automatically
/// invoke the [UnderlyingIo::establish] upon initialization and when a reconnect is needed.
/// Because it implements deref, you are able to invoke all of the original methods on the wrapped IO.
pub struct StubbornIo<T, C> {
    status: Status<T, C>,
    underlying_io: T,
    options: ReconnectOptions,
    ctor_arg: C,
}

enum Status<T, C> {
    Connected,
    Disconnected(ReconnectStatus<T, C>),
    FailedAndExhausted, // the way one feels after programming in dynamically typed languages
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

impl<T, C> Deref for StubbornIo<T, C> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.underlying_io
    }
}

impl<T, C> DerefMut for StubbornIo<T, C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.underlying_io
    }
}

trait FormatName {
    fn format_name(&self) -> String;
}

impl FormatName for String {
    fn format_name(&self) -> String {
        if self.trim().is_empty() {
            String::from("StubbornIo: ")
        } else {
            format!("StubbornIo({}): ", self.trim())
        }
    }
}

impl<T, C> StubbornIo<T, C>
where
    T: UnderlyingIo<C>,
    C: Clone + Send + Unpin + 'static,
{
    /// Connects or creates a handle to the UnderlyingIo item,
    /// using the default reconnect options.
    pub async fn connect(ctor_arg: C) -> io::Result<Self> {
        let options = ReconnectOptions::new();
        Self::connect_with_options(ctor_arg, options).await
    }

    pub fn get_connection_name(&self) -> String {
        self.options.connection_name.format_name()
    }

    pub fn get_block_on_write_failures(&self) -> bool {
        self.options.block_on_write_failures
    }

    pub async fn connect_with_options(ctor_arg: C, options: ReconnectOptions) -> io::Result<Self> {
        let tcp = match T::establish(ctor_arg.clone()).await {
            Ok(tcp) => {
                info!(
                    "{}Initial connection succeeded.",
                    options.connection_name.format_name()
                );
                (options.on_connect_callback)();
                tcp
            }
            Err(e) => {
                error!(
                    "{}Initial connection failed due to: {:?}.",
                    options.connection_name.format_name(),
                    e
                );
                (options.on_connect_fail_callback)();

                if options.exit_if_first_connect_fails {
                    error!(
                        "{}Bailing after initial connection failure.",
                        options.connection_name.format_name()
                    );
                    return Err(e);
                }

                let mut result = Err(e);

                for (i, duration) in (options.retries_to_attempt_fn)().enumerate() {
                    let reconnect_num = i + 1;

                    info!(
                        "{}Will re-perform initial connect attempt #{} in {:?}.",
                        options.connection_name.format_name(),
                        reconnect_num,
                        duration
                    );

                    sleep(duration).await;

                    info!(
                        "{}Attempting reconnect #{} now.",
                        options.connection_name.format_name(),
                        reconnect_num
                    );

                    match T::establish(ctor_arg.clone()).await {
                        Ok(tcp) => {
                            result = Ok(tcp);
                            (options.on_connect_callback)();
                            info!(
                                "{}Initial connection successfully established.",
                                options.connection_name.format_name()
                            );
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
                            "{}No more re-connect retries remaining. Never able to establish initial connection.",
                            options.connection_name.format_name()
                        );
                        return Err(e);
                    }
                }
            }
        };

        Ok(StubbornIo {
            status: Status::Connected,
            ctor_arg,
            underlying_io: tcp,
            options,
        })
    }

    fn on_disconnect(mut self: Pin<&mut Self>, cx: &mut Context) {
        match &mut self.status {
            // initial disconnect
            Status::Connected => {
                error!("{}Disconnect occurred", self.get_connection_name());
                (self.options.on_disconnect_callback)();
                self.status = Status::Disconnected(ReconnectStatus::new(&self.options));
            }
            Status::Disconnected(_) => {
                (self.options.on_connect_fail_callback)();
            }
            Status::FailedAndExhausted => {
                unreachable!(
                    "{}on_disconnect will not occur for already exhausted state.",
                    self.get_connection_name()
                )
            }
        };

        let ctor_arg = self.ctor_arg.clone();
        let connection_name = self.get_connection_name();
        let connection_name_alt = self.get_connection_name();

        // this is ensured to be true now
        if let Status::Disconnected(reconnect_status) = &mut self.status {
            let next_duration = match reconnect_status.attempts_tracker.retries_remaining.next() {
                Some(duration) => duration,
                None => {
                    error!(
                        "{}No more re-connect retries remaining. Giving up.",
                        self.get_connection_name()
                    );
                    self.status = Status::FailedAndExhausted;
                    cx.waker().wake_by_ref();
                    return;
                }
            };

            let future_instant = sleep(next_duration);

            reconnect_status.attempts_tracker.attempt_num += 1;
            let cur_num = reconnect_status.attempts_tracker.attempt_num;

            let reconnect_attempt = async move {
                future_instant.await;
                info!("{}Attempting reconnect #{} now.", connection_name, cur_num);
                T::establish(ctor_arg).await
            };

            reconnect_status.reconnect_attempt = Box::pin(reconnect_attempt);

            info!(
                "{}Will perform reconnect attempt #{} in {:?}.",
                connection_name_alt, reconnect_status.attempts_tracker.attempt_num, next_duration
            );

            cx.waker().wake_by_ref();
        }
    }

    fn poll_disconnect(mut self: Pin<&mut Self>, cx: &mut Context) {
        let (attempt, attempt_num) = match &mut self.status {
            Status::Connected => unreachable!(),
            Status::Disconnected(ref mut status) => (
                Pin::new(&mut status.reconnect_attempt),
                status.attempts_tracker.attempt_num,
            ),
            Status::FailedAndExhausted => unreachable!(),
        };

        match attempt.poll(cx) {
            Poll::Ready(Ok(underlying_io)) => {
                info!("{}Connection re-established", self.get_connection_name());
                cx.waker().wake_by_ref();
                self.status = Status::Connected;
                (self.options.on_connect_callback)();
                self.underlying_io = underlying_io;
            }
            Poll::Ready(Err(err)) => {
                error!(
                    "{}Connection attempt #{} failed: {:?}",
                    self.get_connection_name(),
                    attempt_num,
                    err
                );
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

impl<T, C> AsyncRead for StubbornIo<T, C>
where
    T: UnderlyingIo<C> + AsyncRead,
    C: Clone + Send + Unpin + 'static,
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

impl<T, C> AsyncWrite for StubbornIo<T, C>
where
    T: UnderlyingIo<C> + AsyncWrite,
    C: Clone + Send + Unpin + 'static,
{
    /// Method for writing to the underlying IO item.
    /// If the write results in a disconnect. If ReconectOptions::block_on_write_failures is true,
    /// Poll::Pending is returned to the caller and the buffer is held. Otherwise, the write is skipped
    /// No error is returned to the caller.
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.status {
            Status::Connected => {
                let poll = AsyncWrite::poll_write(Pin::new(&mut self.underlying_io), cx, buf);

                if self.is_write_disconnect_detected(&poll) {
                    if !self.get_block_on_write_failures() {
                        error!(
                            "{}Write disconnect detected. Skipping message",
                            &self.get_connection_name()
                        );

                        self.on_disconnect(cx);
                        Poll::Ready(Ok(buf.len()))
                    } else {
                        warn!(
                            "{}Write disconnect detected. Blocking on write",
                            &self.get_connection_name()
                        );
                        self.on_disconnect(cx);
                        Poll::Pending
                    }
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => {
                if !self.get_block_on_write_failures() {
                    error!(
                        "{}Write disconnect detected. Skipping Message",
                        &self.get_connection_name()
                    );

                    self.poll_disconnect(cx);
                    Poll::Ready(Ok(buf.len()))
                } else {
                    warn!(
                        "{}Write disconnect detected. Blocking on write",
                        &self.get_connection_name()
                    );
                    self.poll_disconnect(cx);
                    Poll::Pending
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
    /// If the write results in a disconnect. If ReconectOptions::block_on_write_failures is true,
    /// Poll::Pending is returned to the caller and the buffer is held. Otherwise, the write is skipped
    /// No error is returned to the caller.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut self.status {
            Status::Connected => {
                let poll =
                    AsyncWrite::poll_write_vectored(Pin::new(&mut self.underlying_io), cx, bufs);

                if self.is_write_disconnect_detected(&poll) {
                    if !self.get_block_on_write_failures() {
                        error!(
                            "{}Write disconnect detected. Skipping message",
                            &self.get_connection_name()
                        );

                        self.on_disconnect(cx);
                        Poll::Ready(Ok(bufs.iter().map(|buf| buf.len()).sum()))
                    } else {
                        warn!(
                            "{}Write disconnect detected. Blocking on write",
                            &self.get_connection_name()
                        );
                        self.on_disconnect(cx);
                        Poll::Pending
                    }
                } else {
                    poll
                }
            }
            Status::Disconnected(_) => {
                if !self.get_block_on_write_failures() {
                    error!(
                        "{}Write disconnect detected. Skipping Message",
                        &self.get_connection_name()
                    );

                    self.poll_disconnect(cx);
                    Poll::Ready(Ok(bufs.iter().map(|buf| buf.len()).sum()))
                } else {
                    warn!(
                        "{}Write disconnect detected. Blocking on write",
                        &self.get_connection_name()
                    );
                    self.poll_disconnect(cx);
                    Poll::Pending
                }
            }
            Status::FailedAndExhausted => exhausted_err(),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.underlying_io.is_write_vectored()
    }
}
