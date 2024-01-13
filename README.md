# tokio_smux

TODO test badge TODO version badge

tokio_smux is an implementation of [smux](https://github.com/xtaci/smux/) in Rust, which is a stream multiplexing library in Golang.

tokio_smux is originally written to work with tokio [TcpStream](https://docs.rs/tokio/latest/tokio/net/struct.TcpStream.html) and [KcpStream](https://docs.rs/tokio_kcp/latest/tokio_kcp/struct.KcpStream.html). It can also be used with any streams implement tokio `AsyncRead` and `AsyncWrite`.

## Smux Protocol Implementation Status

Smux protocl version 2 is not supported yet.

## Docs

[https://docs.rs/tokio_smux](https://docs.rs/tokio_smux)

## Usage Example

Client side:
```rust
use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tokio_smux::{Session, SmuxConfig, Stream};

async fn client() -> Result<()> {
  let client = TcpStream::connect("127.0.0.1:3100").await?;

  let mut session = Session::new_client(client, SmuxConfig::default())?;

  let stream = session.open_stream().await?;

  let data: Vec<u8> = vec![0; 65535];
  stream.send_message(data).await?;

  let msg = stream.recv_message().await?;
}
```

Server side:
```rust
use tokio::net::{TcpListener, TcpStream};
use tokio_smux::{Session, SmuxConfig, Stream};

async fn server() -> Result<()> {
  let listener = TcpListener::bind("0.0.0.0:3100").await?;
  loop {
    let (client, _) = listener.accept().await?;
    let session = Session::new_server(client, SmuxConfig::default())?;

    tokio::spawn(async move {
      loop {
        let mut stream = session.accept_stream().await.unwrap();
        println!("[server] accept stream {}", stream.sid());

        tokio::spawn(async move {
          let data = stream.recv_message().await.unwrap();
          if data.is_none() {
            println!("[server] stream fin {}", stream.sid());
            return
          }

          println!("[serveri] receive client data len {}", data.unwrap().len())
        });
      }
    });
  }
}

```
