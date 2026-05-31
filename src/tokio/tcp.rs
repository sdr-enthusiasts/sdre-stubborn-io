use super::io::{StubbornIo, UnderlyingIo};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::net::TcpStream;

impl UnderlyingIo for TcpStream {
    type Context = SocketAddr;

    fn establish(addr: SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<Self>> + Send>> {
        Box::pin(Self::connect(addr))
    }
}

/// A drop in replacement for tokio's [`TcpStream`](tokio::net::TcpStream), with the
/// distinction that it will automatically attempt to reconnect in the face of connectivity failures.
///
/// Resolution of host strings (DNS) is intentionally **not** performed by this crate;
/// callers must hand in an already-resolved [`SocketAddr`]. Wrapping a name+port into a
/// custom [`UnderlyingIo`] implementation gives the caller control over caching and
/// re-resolution policy.
///
/// ```
/// use sdre_stubborn_io::StubbornTcpStream;
/// use std::net::SocketAddr;
/// use tokio::io::AsyncWriteExt;
///
/// let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
///
/// async {
///     let mut tcp_stream = StubbornTcpStream::connect(addr).await.unwrap();
///     tcp_stream.write_all(b"hello world!").await.unwrap();
/// };
/// ```
pub type StubbornTcpStream = StubbornIo<TcpStream>;
