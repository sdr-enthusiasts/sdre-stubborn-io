# sdre-stubborn-io

Tokio `AsyncRead`/`AsyncWrite` wrapper that transparently reconnects when the
underlying transport drops.

Forked from [stubborn-io](https://github.com/craftytrickster/stubborn-io); the
0.7.x line is a rewrite of the public API around an associated `Context` type,
a single typed event observer, and an opaque builder. Thanks to
[craftytrickster](https://github.com/craftytrickster) for the original work.

## Installation

```toml
[dependencies]
sdre-stubborn-io = "0.7"
```

Requires Rust edition 2024, MSRV `1.85`.

## Quick start: TCP

`StubbornTcpStream` is `StubbornIo<TcpStream>` with `type Context = SocketAddr`.
DNS resolution is intentionally **not** performed by this crate — callers pass
in an already-resolved `SocketAddr`. The rationale is that re-resolution policy
(caching, TTL, failover, resolver choice) belongs to the caller; baking a
strategy in here would force the wrong behaviour on someone.

```rust
use sdre_stubborn_io::StubbornTcpStream;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;

let addr: SocketAddr = "127.0.0.1:8080".parse()?;
let mut tcp = StubbornTcpStream::connect(addr).await?;
tcp.write_all(b"hello").await?;
```

If you need DNS re-resolution on every reconnect, write your own
`UnderlyingIo` impl whose `Context` carries the host string plus a resolver
handle. See the skeleton at the end of this file.

## Configuration

```rust
use sdre_stubborn_io::{ReconnectOptions, StubbornTcpStream};
use sdre_stubborn_io::config::{ReconnectEvent, WriteFailurePolicy};
use std::time::Duration;

let opts = ReconnectOptions::new()
    .with_connection_name("my-feed")
    .with_connect_timeout(Some(Duration::from_secs(15)))
    .with_write_failure_policy(WriteFailurePolicy::Backpressure)
    .with_event_callback(|ev| match ev {
        ReconnectEvent::Connected { attempt }            => log::info!("connected (attempt {attempt})"),
        ReconnectEvent::Disconnected                     => log::warn!("dropped"),
        ReconnectEvent::ConnectFailed { error, attempt } => log::warn!("attempt {attempt}: {error}"),
        ReconnectEvent::ReconnectScheduled { attempt, delay } => log::info!("retry {attempt} in {delay:?}"),
        ReconnectEvent::WriteWhileDisconnected { bytes_dropped } => log::error!("dropped {bytes_dropped} bytes"),
        ReconnectEvent::Exhausted                        => log::error!("giving up"),
        _ => {}
    });

let tcp = StubbornTcpStream::connect_with_options(addr, opts).await?;
```

All configuration goes through builder methods; the `ReconnectOptions` fields
are crate-private.

## API surface

### Trait

```rust
pub trait UnderlyingIo: Sized + Unpin {
    type Context: Clone + Send + Unpin + 'static;
    fn establish(ctx: Self::Context) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>>;
    fn is_disconnect_error(&self, err: &io::Error) -> bool { /* sensible default */ }
    fn is_final_read(&self, bytes_read: usize) -> bool { bytes_read == 0 }
}
```

`establish` is the canonical hook for **post-connect configuration that must
survive reconnects** — TCP keepalive, `TCP_NODELAY`, socket buffer sizes, TLS
handshake, application-level handshakes. Configuration applied externally
through `Deref<Target = T>` on a `StubbornIo<T>` is silently lost the next
time the connection drops.

### Event enum

`ReconnectEvent<'a>` is `#[non_exhaustive]`:

| Variant                  | Fields                                   |
| ------------------------ | ---------------------------------------- |
| `Connected`              | `attempt: usize` (0 = initial)           |
| `Disconnected`           | —                                        |
| `ConnectFailed`          | `error: &'a io::Error`, `attempt: usize` |
| `ReconnectScheduled`     | `attempt: usize`, `delay: Duration`      |
| `WriteWhileDisconnected` | `bytes_dropped: usize`                   |
| `Exhausted`              | —                                        |

Borrowed payloads are scoped to the callback invocation; clone if you need to
retain them.

### Write failure policy

```rust
pub enum WriteFailurePolicy {
    Backpressure,   // default: return Pending; wake when reconnected
    DropAndNotify,  // return Ready(Ok(buf.len())); emit WriteWhileDisconnected
}
```

`Backpressure` preserves caller-side framing. `DropAndNotify` is for
fire-and-forget transports where back-pressure is unacceptable.

### Terminal state

`AsyncWrite::poll_shutdown` transitions the stream into a terminal `Closed`
state. After that, every read/write/shutdown returns
`io::ErrorKind::NotConnected`; no reconnect is attempted. Check via
`StubbornIo::is_closed()` / `is_terminated()`; `is_connected()` reports the
live state.

### Defaults that changed in 0.7.0

| Knob                          | 0.6.x                                       | 0.7.x                                                                                      |
| ----------------------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `exit_if_first_connect_fails` | `true`                                      | `false` (retry from the start)                                                             |
| Write while disconnected      | silent drop (with truncated vectored count) | `Backpressure`                                                                             |
| Connect timeout               | none                                        | optional via `with_connect_timeout`                                                        |
| Disconnect-error set          | narrow                                      | full union of plausible disconnect kinds; `TcpStream` overrides to exclude `UnexpectedEof` |

## Custom `UnderlyingIo` (file example)

```rust
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use sdre_stubborn_io::tokio::{StubbornIo, UnderlyingIo};
use tokio::fs::File;

struct MyFile(File);

impl UnderlyingIo for MyFile {
    type Context = PathBuf;

    fn establish(path: PathBuf) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>> {
        Box::pin(async move { Ok(MyFile(File::open(path).await?)) })
    }
}

type StubbornFile = StubbornIo<MyFile>;
// let f = StubbornFile::connect(PathBuf::from("./input.log")).await?;
```

## Custom `UnderlyingIo` (TCP with DNS re-resolution + keepalive)

The shape consumers reach for when they need both DNS that survives a host's
IP change and per-connection socket tuning that survives reconnects:

```rust,ignore
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use sdre_stubborn_io::tokio::{StubbornIo, UnderlyingIo};
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;

#[derive(Clone)]
pub struct ResolvedTcpCtx {
    pub host: Arc<str>,
    pub port: u16,
    pub resolver: Arc<dyn Resolver + Send + Sync>,
}

pub trait Resolver {
    fn resolve(&self, host: &str) -> Pin<Box<dyn Future<Output = io::Result<std::net::IpAddr>> + Send + '_>>;
}

pub struct ResolvedTcp(pub TcpStream);

impl UnderlyingIo for ResolvedTcp {
    type Context = ResolvedTcpCtx;

    fn establish(ctx: ResolvedTcpCtx) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>> {
        Box::pin(async move {
            // Fresh DNS lookup on every (re)connect — picks up IP changes.
            let ip = ctx.resolver.resolve(&ctx.host).await?;
            let stream = TcpStream::connect((ip, ctx.port)).await?;

            // Keepalive applied here survives every reconnect. Applying it
            // through Deref on the StubbornIo wrapper would be silently lost.
            let ka = TcpKeepalive::new()
                .with_time(Duration::from_secs(60))
                .with_interval(Duration::from_secs(10));
            SockRef::from(&stream).set_tcp_keepalive(&ka)?;

            Ok(ResolvedTcp(stream))
        })
    }
}

pub type StubbornResolvedTcp = StubbornIo<ResolvedTcp>;
```

`ResolvedTcp` also needs `impl AsyncRead + AsyncWrite for ResolvedTcp` — delegate
through the inner `TcpStream`.

## Documentation

API docs: <https://docs.rs/sdre-stubborn-io>.
