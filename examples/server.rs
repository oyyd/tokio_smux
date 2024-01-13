use tokio::net::TcpListener;
use tokio_smux::{Session, SmuxConfig};

#[tokio::main]
async fn main() {
  let listener = TcpListener::bind("0.0.0.0:3100").await.unwrap();
  loop {
    let (client, _) = listener.accept().await.unwrap();
    let mut session = Session::server(client, SmuxConfig::default()).unwrap();

    tokio::spawn(async move {
      loop {
        let mut stream = session.accept_stream().await.unwrap();
        println!("[server] accept stream {}", stream.sid());

        tokio::spawn(async move {
          let data = stream.recv_message().await.unwrap();
          if data.is_none() {
            println!("[server] stream fin {}", stream.sid());
            return;
          }

          println!("[serveri] receive client data len {}", data.unwrap().len())
        });
      }
    });
  }
}
