//! Shared in-memory `UnderlyingIo` shim used by the integration test suites.
//!
//! Each test file in `tests/` is compiled as its own crate, so this module is
//! pulled in via `mod common;` from each consumer. It is intentionally not
//! linted as production code.

#![allow(
    dead_code,
    missing_docs,
    unreachable_pub,
    clippy::missing_panics_doc,
    clippy::redundant_clone,
    clippy::use_self,
    clippy::significant_drop_tightening,
    clippy::equatable_if_let,
    clippy::option_if_let_else
)]

use sdre_stubborn_io::tokio::UnderlyingIo;
use std::future::Future;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A scripted poll outcome the dummy will replay for the next `poll_read` call.
pub type ReadScript = Vec<(Poll<io::Result<()>>, Vec<u8>)>;

/// A scripted poll outcome the dummy will replay for the next `poll_write` call.
/// Each entry is the value returned to the caller. `None` means "delegate to a
/// successful write of `buf.len()` bytes".
pub type WriteScript = Vec<Option<Poll<io::Result<usize>>>>;

/// What an `establish` call should do.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Succeed immediately.
    Ok,
    /// Fail immediately with the given `ErrorKind`.
    Err(ErrorKind),
    /// Sleep `delay` before returning `Ok`; useful for exercising connect timeouts.
    SlowOk(Duration),
}

#[derive(Default, Clone)]
pub struct DummyCtor {
    pub outcomes: Arc<Mutex<Vec<Outcome>>>,
    pub read_script: Arc<Mutex<ReadScript>>,
    pub write_script: Arc<Mutex<WriteScript>>,
    pub establish_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl DummyCtor {
    #[must_use]
    pub fn new(outcomes: Vec<Outcome>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(outcomes)),
            read_script: Arc::new(Mutex::new(Vec::new())),
            write_script: Arc::new(Mutex::new(Vec::new())),
            establish_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    #[must_use]
    pub fn with_read_script(self, script: ReadScript) -> Self {
        *self.read_script.lock().unwrap() = script;
        self
    }

    #[must_use]
    pub fn with_write_script(self, script: WriteScript) -> Self {
        *self.write_script.lock().unwrap() = script;
        self
    }

    pub fn establish_count(&self) -> usize {
        self.establish_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct DummyIo {
    pub read_script: Arc<Mutex<ReadScript>>,
    pub write_script: Arc<Mutex<WriteScript>>,
}

impl UnderlyingIo for DummyIo {
    type Context = DummyCtor;

    fn establish(ctor: DummyCtor) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>> {
        ctor.establish_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let outcome = {
            let mut outcomes = ctor.outcomes.lock().unwrap();
            if outcomes.is_empty() {
                Outcome::Err(ErrorKind::NotConnected)
            } else {
                outcomes.remove(0)
            }
        };
        let read_script = ctor.read_script.clone();
        let write_script = ctor.write_script.clone();
        Box::pin(async move {
            match outcome {
                Outcome::Ok => Ok(DummyIo {
                    read_script,
                    write_script,
                }),
                Outcome::Err(kind) => Err(io::Error::new(kind, "dummy: scripted failure")),
                Outcome::SlowOk(d) => {
                    tokio::time::sleep(d).await;
                    Ok(DummyIo {
                        read_script,
                        write_script,
                    })
                }
            }
        })
    }
}

impl AsyncRead for DummyIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let cloned = self.read_script.clone();
        let mut script = cloned.lock().unwrap();
        if script.is_empty() {
            // Default to pending: keeps the runtime alive without spinning.
            return Poll::Pending;
        }
        let (result, bytes) = script.remove(0);
        if let Poll::Ready(Err(ref e)) = result
            && e.kind() == ErrorKind::WouldBlock
        {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if let Poll::Ready(Ok(())) = &result {
            buf.put_slice(&bytes);
        }
        result
    }
}

impl AsyncWrite for DummyIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let cloned = self.write_script.clone();
        let mut script = cloned.lock().unwrap();
        if script.is_empty() {
            return Poll::Ready(Ok(buf.len()));
        }
        match script.remove(0) {
            Some(p) => p,
            None => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
