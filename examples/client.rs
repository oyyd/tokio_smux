use tokio::net::TcpStream;
use tokio_smux::{Session, SmuxConfig};

#[tokio::main]
async fn main() {
  let client = TcpStream::connect("127.0.0.1:3100").await.unwrap();

  let mut session = Session::client(client, SmuxConfig::default()).unwrap();

  let mut stream = session.open_stream().await.unwrap();

  let data: Vec<u8> = vec![0; 65535];
  stream.send_message(data).await.unwrap();

  let msg = stream.recv_message().await.unwrap();

  if msg.is_none() {
    println!("stream fin");
    return;
  }

  println!("receive {} bytes", msg.unwrap().len())
}
