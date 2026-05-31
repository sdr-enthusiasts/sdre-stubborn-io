//! Provides functionality related to asynchronous IO.
//!
//! Includes concrete ready to use structs such as [`StubbornTcpStream`] as well as
//! the [`UnderlyingIo`] trait and [`StubbornIo`] struct
//! needed to create custom stubborn io types yourself.

mod io;
mod tcp;

pub use self::io::{StubbornIo, UnderlyingIo};

pub use self::tcp::StubbornTcpStream;
