# Changelog

All notable changes to this crate are documented in this file.

The format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate currently ships only the single release described below since being
forked from `stubborn-io` and renamed to `sdre-stubborn-io`.

## [0.7.1] — 2026-05-31

Post-release polish. No functional bug fixes; one technically-breaking API
narrowing (field visibility) justified by the single-consumer policy.

### Breaking (single-consumer)

- `ReconnectOptions` fields (`retries_to_attempt_fn`, `exit_if_first_connect_fails`,
  `event_callback`, `connection_name`, `write_failure_policy`, `connect_timeout`)
  demoted from `pub` to `pub(crate)`. The builder methods (`with_*`) are the
  only supported configuration surface; no router code reaches these fields
  directly. If you were constructing `ReconnectOptions` with the struct literal
  syntax, switch to `ReconnectOptions::new().with_*(…)`.

### Changed in 0.7.1

- `ReconnectOptions::with_connection_name` now accepts `impl Into<Arc<str>>`
  (was `impl AsRef<str>`). `&str` and `String` continue to work via the
  existing blanket impls; callers holding `Arc<str>` get a refcount bump
  instead of a fresh allocation.

### Docs

- `README.md` rewritten for the 0.7.x API: `StubbornTcpStream`'s
  `SocketAddr`-only context with rationale, single `with_event_callback` +
  `ReconnectEvent` variant table, `WriteFailurePolicy`, flipped
  `exit_if_first_connect_fails` default, `with_connect_timeout`,
  `Status::Closed` terminal state, and two custom `UnderlyingIo` skeletons
  (file open + DNS-resolving TCP with keepalive that survives reconnects).
- Removed a duplicated two-line paragraph at the head of the
  `UnderlyingIo::establish` rustdoc.

### Internal in 0.7.1

- Dropped `#[allow(clippy::needless_pass_by_ref_mut)]` on `on_disconnect`;
  the parameter is now `&Context<'_>` since the body only needs `wake_by_ref`.
  All call sites are unaffected (`&mut Context` coerces to `&Context`).
- `flake.nix`: reordered `buildInputs` so the unified Rust toolchain from
  `precommit.passthru.devPackages` appears before the nixpkgs cargo helpers
  (`cargo-deny`/`cargo-machete`/`cargo-make`). The previous order let
  nixpkgs' transitively-pulled cargo+clippy at one stable version shadow the
  unified toolchain's rustc at another, producing `E0514` on every
  `cargo clippy` after a `cargo build`.

### Router migration (`acars_router`) addenda

These notes supplement the 0.7.0 migration block below. They are the items a
fresh migration agent will reach for first.

#### Required Cargo.toml changes

- `sdre-stubborn-io = "0.7"` (was `0.6.x`).
- `acars_router` itself must have `rust-version = "1.85"` (or higher) in its
  workspace `Cargo.toml`; otherwise resolver picks the wrong edition for
  the crate and produces unhelpful errors.

#### Before/after at every call site

```diff
- StubbornTcpStream::connect_with_options(host.to_string(), opts)   // 0.6.x: impl ToSocketAddrs
+ let addr: SocketAddr = /* router-side resolve */;
+ StubbornTcpStream::connect_with_options(addr, opts)               // 0.7.x: SocketAddr only

- ReconnectOptions::new()
-     .with_on_connect_callback(|| …)
-     .with_on_disconnect_callback(|| …)
-     .with_on_connect_fail_callback(|| …)
+ ReconnectOptions::new()
+     .with_event_callback(|ev| match ev {
+         ReconnectEvent::Connected { attempt } => …,
+         ReconnectEvent::Disconnected => …,
+         ReconnectEvent::ConnectFailed { error, attempt } => …,
+         _ => {}
+     })

- block_on_write_failures: false               // struct-literal config (broken in 0.7.1)
+ .with_write_failure_policy(WriteFailurePolicy::DropAndNotify)

- .with_exit_if_first_connect_fails(false)     // redundant in 0.7.x (now the default)
```

#### State inspection

The internal `Status` enum is **not** public. Use accessors:

| Question                                  | Method                    |
| ----------------------------------------- | ------------------------- |
| Currently usable?                         | `is_connected() -> bool`  |
| Permanently dead (exhausted or closed)?   | `is_terminated() -> bool` |
| Explicitly shut down via `poll_shutdown`? | `is_closed() -> bool`     |

#### Worked skeleton: DNS-resolving TCP with keepalive

The router has two requirements that no built-in shape covers: re-resolve DNS
on every reconnect (instead of caching `SocketAddr` at first connect), and
re-apply TCP keepalive on every reconnect (currently lost at
`tcp_services.rs:209-212` because the post-connect tuning is done on a
`StubbornIo`'s `Deref`-target, which the fresh `TcpStream` from the next
`establish` doesn't inherit). The two requirements merge into a single
custom `UnderlyingIo` impl:

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
    fn resolve(&self, host: &str)
        -> Pin<Box<dyn Future<Output = io::Result<std::net::IpAddr>> + Send + '_>>;
}

pub struct ResolvedTcp(pub TcpStream);

impl UnderlyingIo for ResolvedTcp {
    type Context = ResolvedTcpCtx;

    fn establish(ctx: ResolvedTcpCtx)
        -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>>
    {
        Box::pin(async move {
            // (1) Fresh DNS on every (re)connect.
            let ip = ctx.resolver.resolve(&ctx.host).await?;
            let stream = TcpStream::connect((ip, ctx.port)).await?;

            // (2) Keepalive re-applied on every (re)connect.
            let ka = TcpKeepalive::new()
                .with_time(Duration::from_secs(60))
                .with_interval(Duration::from_secs(10));
            SockRef::from(&stream).set_tcp_keepalive(&ka)?;

            Ok(ResolvedTcp(stream))
        })
    }
}

// AsyncRead / AsyncWrite for ResolvedTcp: forward each method to self.0
// (mechanical; the Pin<&mut Self> projections all delegate via Pin::new(&mut self.0)).

pub type StubbornResolvedTcp = StubbornIo<ResolvedTcp>;
```

The router then replaces `service_init.rs:662` with
`StubbornResolvedTcp::connect_with_options(ctx, opts)` and deletes the
`socket2::SockRef` block at `tcp_services.rs:209-212`.

#### Inventory of removed / renamed / added public items

Removed:

- `ReconnectOptions::with_on_connect_callback`
- `ReconnectOptions::with_on_disconnect_callback`
- `ReconnectOptions::with_on_connect_fail_callback`
- `ReconnectOptions::block_on_write_failures` (field)
- `FormatName` type
- Blanket `impl<A: ToSocketAddrs> UnderlyingIo<A> for TcpStream`
- All six `ReconnectOptions` fields as `pub` (now `pub(crate)`)

Added:

- `ReconnectOptions::with_event_callback`
- `ReconnectOptions::with_write_failure_policy`
- `ReconnectOptions::with_connect_timeout`
- `ReconnectOptions::default()`
- `ReconnectEvent<'_>` enum
- `WriteFailurePolicy` enum
- `EventCallback` type alias
- `StubbornIo::is_connected`
- `StubbornIo::is_terminated`
- `StubbornIo::is_closed`
- `StubbornIo::get_write_failure_policy`
- Terminal `Status::Closed` (observable via `is_closed`, not directly matchable)

Changed signature:

- `UnderlyingIo`: gains `type Context`; `establish(ctx: Self::Context)` (no
  generic `C`).
- `StubbornTcpStream::connect[_with_options]`: takes `SocketAddr` (was
  `impl ToSocketAddrs`).
- `ReconnectOptions::with_connection_name`: takes `impl Into<Arc<str>>`
  (was `impl AsRef<str>` in 0.7.0, `impl Into<String>` in 0.6.x).

## [0.7.0] — 2026-05-31

This release is a deliberate, breaking re-cut of the public API following the
internal audit in `sdre_stubborn_io_audit.md`. The single downstream consumer
(`acars_router`) is migrated in lock-step; see "Router migration" below.

### Breaking

- `UnderlyingIo` no longer takes a generic constructor argument. The trait is now
  `pub trait UnderlyingIo: Sized + Unpin { type Context: Clone + Send + Unpin + 'static; fn establish(ctx: Self::Context) -> …; }`.
  Each implementer declares its own `Context`; the prior blanket
  `impl<A: ToSocketAddrs> UnderlyingIo<A> for TcpStream` is removed.
  `StubbornTcpStream` is now exactly `StubbornIo<TcpStream>` with `Context = SocketAddr` —
  DNS resolution is intentionally pushed out of this crate and onto the caller, who
  can either pre-resolve once or wrap their own `UnderlyingIo` impl whose `Context`
  carries the resolver.
- `ReconnectOptions` collapses the three callback setters
  (`with_on_connect_callback`, `with_on_disconnect_callback`,
  `with_on_connect_fail_callback`) into a single
  `with_event_callback(impl Fn(ReconnectEvent<'_>))`. The callback is stored
  as `Arc<dyn Fn(ReconnectEvent<'_>) + Send + Sync>`.
- `ReconnectEvent<'a>` is a new `#[non_exhaustive]` enum with variants
  `Connected { attempt }`, `Disconnected`, `ConnectFailed { error, attempt }`,
  `ReconnectScheduled { attempt, delay }`, `WriteWhileDisconnected { bytes_dropped }`,
  and `Exhausted`.
- `ReconnectOptions::block_on_write_failures: bool` is replaced by
  `write_failure_policy: WriteFailurePolicy` (`#[non_exhaustive]` enum:
  `Backpressure` (default), `DropAndNotify`). Vectored writes under
  `DropAndNotify` now correctly report the sum of all input slice lengths as
  the written count.
- `exit_if_first_connect_fails` default flipped from `true` to `false`. Callers
  who want the prior fail-fast behaviour must opt in explicitly via
  `with_exit_if_first_connect_fails(true)`.
- `Status` gains a terminal `Closed` variant. A successful (or errored)
  `poll_shutdown` transitions to `Closed`, after which no further reconnects
  are attempted and subsequent read/write/shutdown calls return
  `io::ErrorKind::NotConnected`. A `Disconnected` stream that is then asked
  to shut down also transitions to `Closed`.
- `connection_name` is now `Arc<str>` (was `String`). `with_connection_name`
  takes `impl AsRef<str>` and interns into an `Arc<str>`.
- `FormatName` is deleted; the formatted log prefix is cached internally and
  surfaced via `get_connection_name()` only.
- Crate now requires Rust edition 2024 and MSRV `1.85`.

### Added

- `ReconnectOptions::with_connect_timeout(Option<Duration>)`: per-attempt
  timeout wrapped around every `UnderlyingIo::establish` invocation. Elapsed
  timeouts surface as `io::ErrorKind::TimedOut` to the reconnect machinery,
  so the next backoff step runs as if the establish itself had failed.
- `StubbornIo::is_connected()`, `is_terminated()`, and `is_closed()` accessors.
- `StubbornIo::get_write_failure_policy()` accessor.
- `ReconnectOptions::default()` (delegates to `new()`).
- `proptest` dev-dep and a property suite covering the state machine,
  vectored `DropAndNotify` length accounting, and `connect_timeout` bounds.
- `tests/common/mod.rs`: shared scriptable in-memory `UnderlyingIo` shim
  (connect outcomes, read/write scripts, establish counter) consumed by
  both `tests/state_machine.rs` and `tests/property_tests.rs`.

### Changed

- Default `UnderlyingIo::is_disconnect_error` set widened to the full union
  of plausible-disconnect kinds: `ConnectionRefused`, `ConnectionReset`,
  `ConnectionAborted`, `NotConnected`, `AddrInUse`, `AddrNotAvailable`,
  `BrokenPipe`, `TimedOut`, `UnexpectedEof`, `HostUnreachable`,
  `NetworkUnreachable`, `NetworkDown`. `TcpStream` overrides to drop
  `UnexpectedEof` (which a raw `TcpStream` poll cannot directly surface;
  EOF is handled by `is_final_read`).
- Retry-path messages "Initial connection failed" and
  "Connection attempt #N failed" demoted from `error!` to `warn!`. The
  terminal `error!` lines (initial-fail bail and retry-exhausted) remain.
- `UnderlyingIo::establish` is now explicitly documented as the canonical
  hook for post-connect configuration that must survive reconnects (TCP
  keepalive, `TCP_NODELAY`, socket buffer sizes, TLS handshake,
  application-level handshakes). Configuration applied externally through
  `Deref` is silently lost on every reconnect.
- `EventCallback` is exposed as a public type alias
  (`Arc<dyn for<'a> Fn(ReconnectEvent<'a>) + Send + Sync>`).
- `ExpBackoffIter::next` clamps the computed delay to a finite, non-negative
  `Duration` via `try_from_secs_f64`, returning `Duration::MAX` on overflow
  or non-finite jitter rather than panicking.
- Reconnect-future field on `ReconnectStatus` is now `Option<Pin<Box<…>>>`,
  removing the placeholder no-op future that was never polled.

### Fixed

- Vectored writes under the old `block_on_write_failures = false` path
  returned `Ok(bufs[0].len())` instead of the total length, silently
  truncating the caller's framing cursor. The replacement
  `WriteFailurePolicy::DropAndNotify` returns the sum of all input slice
  lengths and emits a `WriteWhileDisconnected { bytes_dropped }` event.
- Duplicate disconnect callback invocation on the reconnect-after-failure
  path is gone; events are emitted exactly once per transition at the call
  site where the relevant data is in scope.

### Internal

- Edition 2024, MSRV 1.85; `rust-toolchain.toml` pins stable + rustfmt + clippy.
- Canonical `[lints]` table: `clippy::pedantic + nursery + cargo`,
  `rust_2018_idioms`, `unsafe_code = forbid`, `missing_docs = warn`,
  `unreachable_pub = warn`. CI enforces `-D warnings` over
  `--all-targets --all-features`. Crate is lint-clean.
- Patch-pinned dependency versions (no `^`/`~` ranges).
- Legacy `tests/dummy_tests.rs` replaced by `tests/state_machine.rs` and
  `tests/property_tests.rs`; `tests/integration_tests.rs` retained as the
  real-socket shutdown smoke test.

### Router migration (`acars_router`)

Bump `sdre-stubborn-io = "0.6.x"` → `"0.7.0"` in
`acars_router/Cargo.toml`, then:

- `rust/libraries/acars_connection_manager/src/lib.rs:102-107`
  (`reconnect_options`):
  - The three `with_on_*_callback` setters no longer exist. Router was not
    invoking any of them; deletion is a no-op. To gain per-destination
    metrics, install a single `.with_event_callback(|ev| …)` and match on
    `ReconnectEvent`.
  - `with_exit_if_first_connect_fails(false)` is now the default and can be
    deleted.
  - Optionally add `.with_connect_timeout(Some(Duration::from_secs(15)))` to
    bound stuck `connect()` calls.
  - Default `WriteFailurePolicy::Backpressure` matches what the router
    actually wants; no change needed.
- `rust/libraries/acars_connection_manager/src/service_init.rs:662`
  (`StubbornTcpStream::connect_with_options(host.to_string(), …)`): this is
  the DNS-hammering site. `StubbornTcpStream` now takes a `SocketAddr` only.
  The router must own DNS — either pre-resolve once and pass the
  `SocketAddr`, or define its own `UnderlyingIo` impl whose `Context`
  carries an `Arc<str>` host plus a cached resolver and re-resolves on each
  `establish` call. See the new `UnderlyingIo::establish` rustdoc for the
  canonical pattern.
- `rust/libraries/acars_connection_manager/src/service_init.rs:723` (the
  dead `SocketListenerServer` path the router audit flags for deletion):
  same change as `:662` if retained; deletion preferred.
- `rust/libraries/acars_connection_manager/src/tcp_services.rs:188`
  (`StubbornTcpStream::connect_with_options(addr, …)`): unchanged — this
  site already passes a `SocketAddr`.
- `rust/libraries/acars_connection_manager/src/tcp_services.rs:209-212`
  (post-connect keepalive setup via `socket2::SockRef::from(&*stream)`):
  keepalive applied here is silently lost on every reconnect because the
  fresh `TcpStream` returned by `establish` has OS-default keepalive. Move
  the keepalive setup into a custom `impl UnderlyingIo for …` (inside the
  `establish` body, applied to the freshly-built `TcpStream` before
  returning) so it survives reconnects.
