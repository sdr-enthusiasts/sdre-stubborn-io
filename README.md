# sdre_stubborn-io

This crate provides io traits/structs that automatically recover from potential disconnections/interruptions.

To use with your project, add the following to your Cargo.toml:

```toml
stubborn-io = { git = "https://github.com/sdr-enthusiasts/sdre-stubborn-io.git" }
```

## Thanks and purpose

This project has been forked from [stubborn-io](https://github.com/craftytrickster/stubborn-io) and modified to add the ability to name connections. Much thanks to [craftytrickster](https://github.com/craftytrickster) for the original project.

## Documentation

API Documentation, examples and motivations can be found here -
(Rust Docs)<https://docs.rs/stubborn-io> .

Only change to the documentation in this fork will be the addition of the `ReconnectionOptions` struct, which adds `with_connection_name(name: &str)` as a method to the `StubbornTcpStream` struct. This allows for the naming of the connection, which is useful for logging purposes.

If you generate the struct manually, the field name is `connection_name`.

## Usage Example

In this example, we will see a drop in replacement for tokio's TcpStream, with the
distinction that it will automatically attempt to reconnect in the face of connectivity failures.

```rust
use sdre_stubborn_io::StubbornTcpStream;
use tokio::io::AsyncWriteExt;

let addr = "localhost:8080";

// we are connecting to the TcpStream using the default built in options.
// these can also be customized (for example, the amount of reconnect attempts,
// wait duration, etc) using the connect_with_options method.
let mut tcp_stream = StubbornTcpStream::connect(addr).await?;
// once we acquire the wrapped IO, in this case, a TcpStream, we can
// call all of the regular methods on it, as seen below
tcp_stream.write_all(b"hello world!").await?;
```
