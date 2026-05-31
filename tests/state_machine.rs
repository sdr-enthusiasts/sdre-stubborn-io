//! Deterministic state-machine and behaviour tests for `StubbornIo`, using
//! the in-memory `DummyIo` shim from `tests/common`.

#![allow(
    missing_docs,
    clippy::missing_panics_doc,
    clippy::significant_drop_tightening
)]

mod common;

use common::{DummyCtor, DummyIo, Outcome};
use sdre_stubborn_io::ReconnectOptions;
use sdre_stubborn_io::config::{ReconnectEvent, WriteFailurePolicy};
use sdre_stubborn_io::tokio::{StubbornIo, UnderlyingIo};
use std::io::{self, ErrorKind, IoSlice};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type StubbornDummy = StubbornIo<DummyIo>;

fn fast_retries(n: usize) -> impl Fn() -> Vec<Duration> + Send + Sync + 'static {
    move || vec![Duration::from_millis(5); n]
}

/// Collect events emitted over the lifetime of a stream into a shared `Vec`.
fn event_sink() -> (
    Arc<Mutex<Vec<String>>>,
    impl Fn(ReconnectEvent<'_>) + Send + Sync + 'static,
) {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_cb = log.clone();
    let cb = move |ev: ReconnectEvent<'_>| {
        log_cb.lock().unwrap().push(format!("{ev:?}"));
    };
    (log, cb)
}

// ---------------------------------------------------------------------------
// Initial connect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initial_connect_success_yields_connected() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]);
    let s = StubbornDummy::connect(ctor).await.unwrap();
    assert!(s.is_connected());
    assert!(!s.is_terminated());
    assert!(!s.is_closed());
}

#[tokio::test]
async fn initial_connect_failure_with_exit_on_first_fail_bails() {
    let ctor = DummyCtor::new(vec![Outcome::Err(ErrorKind::ConnectionRefused)]);
    let opts = ReconnectOptions::new().with_exit_if_first_connect_fails(true);
    let err = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .err()
        .expect("expected initial connect to fail");
    assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
}

#[tokio::test]
async fn initial_connect_failure_default_retries_until_success() {
    let ctor = DummyCtor::new(vec![
        Outcome::Err(ErrorKind::ConnectionRefused),
        Outcome::Err(ErrorKind::ConnectionRefused),
        Outcome::Ok,
    ]);
    let (log, cb) = event_sink();
    let opts = ReconnectOptions::new()
        .with_retries_generator(fast_retries(5))
        .with_event_callback(cb);
    let s = StubbornDummy::connect_with_options(ctor.clone(), opts)
        .await
        .unwrap();
    assert!(s.is_connected());
    assert_eq!(ctor.establish_count(), 3);
    let events = log.lock().unwrap();
    // Expect two ConnectFailed followed by one Connected.
    assert_eq!(
        events
            .iter()
            .filter(|e| e.contains("ConnectFailed"))
            .count(),
        2
    );
    assert_eq!(events.iter().filter(|e| e.contains("Connected")).count(), 1);
}

#[tokio::test]
async fn initial_connect_failure_exhausts_and_errors() {
    let ctor = DummyCtor::new(vec![
        Outcome::Err(ErrorKind::ConnectionRefused),
        Outcome::Err(ErrorKind::ConnectionRefused),
        Outcome::Err(ErrorKind::ConnectionRefused),
    ]);
    let (log, cb) = event_sink();
    let opts = ReconnectOptions::new()
        .with_retries_generator(fast_retries(2))
        .with_event_callback(cb);
    let err = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .err()
        .expect("expected exhausted initial connect to fail");
    assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
    assert!(log.lock().unwrap().iter().any(|e| e.contains("Exhausted")));
}

// ---------------------------------------------------------------------------
// Mid-flight reconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fatal_read_error_triggers_reconnect() {
    let ctor = DummyCtor::new(vec![Outcome::Ok, Outcome::Ok]).with_read_script(vec![
        (
            Poll::Ready(Err(io::Error::new(
                ErrorKind::ConnectionAborted,
                "lost peer",
            ))),
            vec![],
        ),
        (Poll::Ready(Ok(())), b"recovered".to_vec()),
    ]);
    let disconnects = Arc::new(AtomicUsize::new(0));
    let dc = disconnects.clone();
    let opts = ReconnectOptions::new()
        .with_retries_generator(fast_retries(3))
        .with_event_callback(move |ev| {
            if matches!(ev, ReconnectEvent::Disconnected) {
                dc.fetch_add(1, Ordering::Relaxed);
            }
        });
    let ctor_clone = ctor.clone();
    let mut s = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .unwrap();
    let mut buf = vec![0u8; 9];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"recovered");
    assert_eq!(disconnects.load(Ordering::Relaxed), 1);
    assert_eq!(ctor_clone.establish_count(), 2);
}

#[tokio::test]
async fn would_block_does_not_trigger_reconnect() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]).with_read_script(vec![
        (
            Poll::Ready(Err(io::Error::new(ErrorKind::WouldBlock, "try again"))),
            vec![],
        ),
        (Poll::Ready(Ok(())), b"hi".to_vec()),
    ]);
    let ctor_clone = ctor.clone();
    let mut s = StubbornDummy::connect(ctor).await.unwrap();
    let mut buf = [0u8; 2];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hi");
    assert_eq!(ctor_clone.establish_count(), 1);
}

#[tokio::test]
async fn eof_zero_byte_read_triggers_reconnect() {
    let ctor = DummyCtor::new(vec![Outcome::Ok, Outcome::Ok]).with_read_script(vec![
        (Poll::Ready(Ok(())), vec![]),
        (Poll::Ready(Ok(())), b"x".to_vec()),
    ]);
    let opts = ReconnectOptions::new().with_retries_generator(fast_retries(2));
    let mut s = StubbornDummy::connect_with_options(ctor.clone(), opts)
        .await
        .unwrap();
    let mut buf = [0u8; 1];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"x");
    assert_eq!(ctor.establish_count(), 2);
}

// ---------------------------------------------------------------------------
// Exhaustion → terminal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_after_exhaustion_returns_not_connected() {
    let ctor = DummyCtor::new(vec![
        Outcome::Ok,
        Outcome::Err(ErrorKind::ConnectionAborted),
        Outcome::Err(ErrorKind::ConnectionAborted),
    ])
    .with_read_script(vec![
        (
            Poll::Ready(Err(io::Error::new(ErrorKind::ConnectionAborted, "boom"))),
            vec![],
        ),
        // Subsequent polls will be Pending by default, which is irrelevant
        // once reconnection is exhausted.
    ]);
    let opts = ReconnectOptions::new().with_retries_generator(fast_retries(1));
    let mut s = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .unwrap();
    let mut buf = [0u8; 1];
    let err = s.read_exact(&mut buf).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotConnected);
    assert!(s.is_terminated());
    assert!(!s.is_closed());
}

// ---------------------------------------------------------------------------
// Explicit shutdown → Closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_transitions_to_closed() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]);
    let mut s = StubbornDummy::connect(ctor).await.unwrap();
    s.shutdown().await.unwrap();
    assert!(s.is_closed());
    assert!(s.is_terminated());
    assert!(!s.is_connected());
}

#[tokio::test]
async fn shutdown_after_shutdown_returns_not_connected() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]);
    let mut s = StubbornDummy::connect(ctor).await.unwrap();
    s.shutdown().await.unwrap();
    let err = s.shutdown().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotConnected);
}

#[tokio::test]
async fn read_after_shutdown_returns_not_connected() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]);
    let mut s = StubbornDummy::connect(ctor).await.unwrap();
    s.shutdown().await.unwrap();
    let mut buf = [0u8; 1];
    let err = s.read_exact(&mut buf).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotConnected);
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accessors_report_configured_values() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]);
    let opts = ReconnectOptions::new()
        .with_connection_name("my-name")
        .with_write_failure_policy(WriteFailurePolicy::DropAndNotify);
    let s = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .unwrap();
    assert_eq!(s.get_connection_name(), "StubbornIo(my-name): ");
    assert_eq!(
        s.get_write_failure_policy(),
        WriteFailurePolicy::DropAndNotify
    );
}

#[tokio::test]
async fn empty_connection_name_uses_bare_prefix() {
    let ctor = DummyCtor::new(vec![Outcome::Ok]);
    let s = StubbornDummy::connect(ctor).await.unwrap();
    assert_eq!(s.get_connection_name(), "StubbornIo: ");
}

// ---------------------------------------------------------------------------
// Connect timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_timeout_elapses_and_retries() {
    let ctor = DummyCtor::new(vec![Outcome::SlowOk(Duration::from_secs(60)), Outcome::Ok]);
    let opts = ReconnectOptions::new()
        .with_connect_timeout(Some(Duration::from_millis(10)))
        .with_retries_generator(fast_retries(3));
    let s = StubbornDummy::connect_with_options(ctor.clone(), opts)
        .await
        .unwrap();
    assert!(s.is_connected());
    assert_eq!(ctor.establish_count(), 2);
}

// ---------------------------------------------------------------------------
// Write failure policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drop_and_notify_returns_buf_len_and_emits_event() {
    let ctor = DummyCtor::new(vec![Outcome::Ok, Outcome::Ok]).with_write_script(vec![Some(
        Poll::Ready(Err(io::Error::new(ErrorKind::BrokenPipe, "peer gone"))),
    )]);
    let dropped = Arc::new(AtomicUsize::new(0));
    let dc = dropped.clone();
    let opts = ReconnectOptions::new()
        .with_write_failure_policy(WriteFailurePolicy::DropAndNotify)
        .with_retries_generator(fast_retries(2))
        .with_event_callback(move |ev| {
            if let ReconnectEvent::WriteWhileDisconnected { bytes_dropped } = ev {
                dc.fetch_add(bytes_dropped, Ordering::Relaxed);
            }
        });
    let mut s = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .unwrap();
    let n = s.write(b"hello").await.unwrap();
    assert_eq!(n, 5);
    assert_eq!(dropped.load(Ordering::Relaxed), 5);
}

#[tokio::test]
async fn drop_and_notify_vectored_returns_sum_of_lengths() {
    // Connected-path vectored write that reveals a disconnect via the underlying
    // poll. The crate-level `poll_write_vectored` lives over `StubbornIo`, but
    // tokio's default impl falls back to `poll_write` for non-vectored sinks,
    // so we exercise the same DropAndNotify accounting via a sequence of writes.
    let bufs = [b"hello".as_slice(), b" ".as_slice(), b"world".as_slice()];
    let total: usize = bufs.iter().map(|b| b.len()).sum();
    let ctor = DummyCtor::new(vec![Outcome::Ok, Outcome::Ok]).with_write_script(vec![Some(
        Poll::Ready(Err(io::Error::new(ErrorKind::BrokenPipe, "peer gone"))),
    )]);
    let dropped = Arc::new(AtomicUsize::new(0));
    let dc = dropped.clone();
    let opts = ReconnectOptions::new()
        .with_write_failure_policy(WriteFailurePolicy::DropAndNotify)
        .with_retries_generator(fast_retries(2))
        .with_event_callback(move |ev| {
            if let ReconnectEvent::WriteWhileDisconnected { bytes_dropped } = ev {
                dc.fetch_add(bytes_dropped, Ordering::Relaxed);
            }
        });
    let mut s = StubbornDummy::connect_with_options(ctor, opts)
        .await
        .unwrap();
    // Use the vectored entry point directly via the AsyncWriteExt::write_vectored.
    let slices: Vec<IoSlice<'_>> = bufs.iter().map(|b| IoSlice::new(b)).collect();
    let n = s.write_vectored(&slices).await.unwrap();
    assert_eq!(n, total);
    assert_eq!(dropped.load(Ordering::Relaxed), total);
}

// ---------------------------------------------------------------------------
// Disconnect-kind defaults
// ---------------------------------------------------------------------------

#[test]
fn default_is_disconnect_error_covers_canonical_set() {
    let io_dummy = DummyIo {
        read_script: Arc::new(Mutex::new(Vec::new())),
        write_script: Arc::new(Mutex::new(Vec::new())),
    };
    let disconnect_kinds = [
        ErrorKind::ConnectionRefused,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::NotConnected,
        ErrorKind::AddrInUse,
        ErrorKind::AddrNotAvailable,
        ErrorKind::BrokenPipe,
        ErrorKind::TimedOut,
        ErrorKind::UnexpectedEof,
        ErrorKind::HostUnreachable,
        ErrorKind::NetworkUnreachable,
        ErrorKind::NetworkDown,
    ];
    for kind in disconnect_kinds {
        let err = io::Error::new(kind, "x");
        assert!(io_dummy.is_disconnect_error(&err), "should match: {kind:?}");
    }
    let benign = [
        ErrorKind::WouldBlock,
        ErrorKind::Interrupted,
        ErrorKind::Other,
        ErrorKind::InvalidData,
    ];
    for kind in benign {
        let err = io::Error::new(kind, "x");
        assert!(
            !io_dummy.is_disconnect_error(&err),
            "should NOT match: {kind:?}"
        );
    }
}

#[test]
fn tcp_overrides_drop_unexpected_eof() {
    use tokio::net::TcpStream;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tcp = rt.block_on(async move { TcpStream::connect(addr).await.unwrap() });
    let err = io::Error::new(ErrorKind::UnexpectedEof, "x");
    assert!(!tcp.is_disconnect_error(&err));
    let err = io::Error::new(ErrorKind::ConnectionReset, "x");
    assert!(tcp.is_disconnect_error(&err));
}
