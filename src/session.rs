use crate::config::SmuxConfig;
use crate::error::{Result, TokioSmuxError};
use crate::frame::{Cmd, Frame, Sid};
use crate::read_frame_grouper::ReadFrameGrouper;
use crate::session_inner::{SessionInner, WriteRequest};
use crate::stream::Stream;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Mutex};

const MAX_READ_REQ: usize = 4096;
const MAX_IN_QUEUE_SYNC_FRAMES: usize = 4096;

/// Session is used to manage the underlying stream and provide multiplexing abilites.
pub struct Session {
  config: SmuxConfig,

  // current stream id
  sid: Sid,

  write_tx: mpsc::Sender<WriteRequest>,

  go_away: bool,

  sync_rx: mpsc::Receiver<Frame>,
  sid_tx_map: Arc<Mutex<HashMap<Sid, Arc<Mutex<mpsc::Sender<Frame>>>>>>,
  sid_rx_map: Arc<Mutex<HashMap<Sid, mpsc::Receiver<Frame>>>>,
  sid_close_tx_map: Arc<Mutex<HashMap<Sid, oneshot::Sender<()>>>>,
  sid_drop_tx: mpsc::Sender<Sid>,

  inner_err: Arc<Mutex<Option<TokioSmuxError>>>,
}

impl Drop for Session {
  fn drop(&mut self) {
    // close all streams
    let sid_close_tx_map = self.sid_close_tx_map.clone();
    tokio::spawn(async move {
      let mut keys: Vec<Sid> = vec![];
      for (kv, _) in sid_close_tx_map.lock().await.iter() {
        keys.push(*kv);
      }
      for id in keys {
        let item = sid_close_tx_map.lock().await.remove(&id);
        let tx = item.unwrap();
        let _ = tx.send(());
      }
    });
  }
}

impl Session {
  pub fn client<T: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    conn: T,
    config: SmuxConfig,
  ) -> Result<Self> {
    return Session::new(conn, config, true);
  }

  pub fn server<T: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    conn: T,
    config: SmuxConfig,
  ) -> Result<Self> {
    return Session::new(conn, config, false);
  }

  fn new<T: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    conn: T,
    config: SmuxConfig,
    is_client: bool,
  ) -> Result<Self> {
    config.verify_config()?;

    // init SessionInner
    let (write_tx, write_rx) = mpsc::channel(config.writing_frame_channel_capacity);
    let (read_tx, new_frame_rx) = mpsc::channel(MAX_READ_REQ);
    let inner_err = Arc::new(Mutex::new(None));
    let mut inner = SessionInner::new(conn, write_rx, read_tx);
    if !config.keep_alive_disable {
      inner.with_keep_alive_interval(config.keep_alive_interval)
    }

    let inner_err_handle = inner_err.clone();

    tokio::spawn(async move {
      let res = inner.run().await;
      if res.is_err() {
        let mut inner_err = inner_err_handle.lock().await;
        let _ = inner_err.insert(res.err().unwrap());
      }
    });

    // init ReadFrameGrouper
    let (sync_tx, sync_rx) = mpsc::channel(MAX_IN_QUEUE_SYNC_FRAMES);
    let sid_tx_map = Arc::new(Mutex::new(HashMap::new()));
    let sid_rx_map = Arc::new(Mutex::new(HashMap::new()));
    let mut spliter = ReadFrameGrouper {
      new_frame_rx,
      sync_tx,
      sid_tx_map: sid_tx_map.clone(),
      sid_rx_map: sid_rx_map.clone(),
      sid_frame_buffer_size: config.stream_reading_frame_channel_capacity,
    };
    tokio::spawn(async move {
      spliter.run().await;
    });

    // init self
    let (sid_drop_tx, sid_drop_rx) = mpsc::channel(1024);

    let session = Self {
      config,
      sid: match is_client {
        true => 1,
        false => 0,
      },

      write_tx,
      go_away: false,

      sid_tx_map,
      sid_rx_map,
      sid_close_tx_map: Arc::new(Mutex::new(HashMap::new())),
      sid_drop_tx,

      sync_rx,
      inner_err,
    };

    let mut cleaner = SessionCleaner::from_session(&session, sid_drop_rx);
    tokio::spawn(async move { cleaner.run().await });

    Ok(session)
  }

  /// Get the underlying stream error, e.g. tcp connection failures.
  pub async fn get_inner_err(&mut self) -> Option<TokioSmuxError> {
    let inner_err = self.inner_err.lock().await;
    if inner_err.is_some() {
      return inner_err.clone();
    }

    return None;
  }

  async fn inner_ok(&mut self) -> Result<()> {
    let err = self.get_inner_err().await;
    if err.is_none() {
      return Ok(());
    }
    return Err(err.unwrap());
  }

  /// Create a new `Stream` by sending a sync frame to the remote.
  pub async fn open_stream(&mut self) -> Result<Stream> {
    // Check if stream id overflows.
    if self.go_away {
      return Err(TokioSmuxError::SessionGoAway);
    }

    // Check inner error.
    self.inner_ok().await?;

    self.sid += 2;
    if self.sid == self.sid % 2 {
      self.go_away = true;
      return Err(TokioSmuxError::SessionGoAway);
    }
    let sid = self.sid;

    // New stream and write sync cmd.
    let frame = Frame::new(self.config.version, Cmd::Sync, sid);
    let res = self.write_frame(frame).await;
    // When the operation fails, firstly check and return the inner error because
    // we want to expose the inner error to users, which is more useful than
    // "session closed".
    if res.is_err() {
      self.inner_ok().await?;
      // If no inner error, return the write_frame error.
      res?;
    }

    // Update sid_tx_map and sid_rx_map when open_stream.
    {
      let contained = { self.sid_tx_map.lock().await.contains_key(&sid) };
      if !contained {
        let (tx, rx) = mpsc::channel(self.config.stream_reading_frame_channel_capacity);
        self
          .sid_tx_map
          .lock()
          .await
          .insert(sid, Arc::new(Mutex::new(tx)));
        self.sid_rx_map.lock().await.insert(sid, rx);
      }
    }
    let stream = self.new_stream(sid).await;
    if stream.is_err() {
      // not likely
      self.sid_tx_map.lock().await.remove(&sid);
      self.sid_rx_map.lock().await.remove(&sid);
    }
    Ok(stream.unwrap())
  }

  /// Create a new `Stream` by receiving a sync frame from the remote.
  pub async fn accept_stream(&mut self) -> Result<Stream> {
    // Check inner error.
    self.inner_ok().await?;

    let frame = self.sync_rx.recv().await;
    if frame.is_none() {
      return Err(TokioSmuxError::SessionClosed);
    }

    let frame = frame.unwrap();
    let sid = frame.sid;

    let stream = self.new_stream(sid).await?;

    Ok(stream)
  }

  async fn new_stream(&mut self, sid: Sid) -> Result<Stream> {
    let frame_rx = {
      let rx = self.sid_rx_map.lock().await.remove(&sid);
      if rx.is_none() {
        return Err(TokioSmuxError::Default {
          msg: "unexpected empty sid in sid_rx_map".to_string(),
        });
      }
      rx.unwrap()
    };

    let (close_tx, close_rx) = oneshot::channel();
    self.sid_close_tx_map.lock().await.insert(sid, close_tx);

    let mut stream = Stream::new(sid, frame_rx, self.write_tx.clone(), close_rx);
    stream.with_drop_tx(Some(self.sid_drop_tx.clone()));
    Ok(stream)
  }

  async fn write_frame(&mut self, frame: Frame) -> Result<()> {
    let (tx, rx) = oneshot::channel::<()>();

    // Append to the write queue.
    self
      .write_tx
      .send(WriteRequest {
        frame,
        finish_tx: Some(tx),
      })
      .await?;

    rx.await?;
    Ok(())
  }
}

// Clean stream data.
struct SessionCleaner {
  sid_drop_rx: mpsc::Receiver<Sid>,

  sid_tx_map: Arc<Mutex<HashMap<Sid, Arc<Mutex<mpsc::Sender<Frame>>>>>>,
  sid_rx_map: Arc<Mutex<HashMap<Sid, mpsc::Receiver<Frame>>>>,
  sid_close_tx_map: Arc<Mutex<HashMap<Sid, oneshot::Sender<()>>>>,
}

impl SessionCleaner {
  pub fn from_session(session: &Session, sid_drop_rx: mpsc::Receiver<Sid>) -> Self {
    Self {
      sid_drop_rx: sid_drop_rx,
      sid_tx_map: session.sid_tx_map.clone(),
      sid_rx_map: session.sid_rx_map.clone(),
      sid_close_tx_map: session.sid_close_tx_map.clone(),
    }
  }

  pub async fn run(&mut self) {
    loop {
      let sid = self.sid_drop_rx.recv().await;
      if sid.is_none() {
        break;
      }

      let sid = sid.unwrap();
      self.sid_tx_map.lock().await.remove(&sid);
      self.sid_rx_map.lock().await.remove(&sid);
      self.sid_close_tx_map.lock().await.remove(&sid);
    }
  }
}

#[cfg(test)]
pub mod test {
  use crate::{frame::Sid, session::Session, SmuxConfig, TokioSmuxError};
  use tokio::io::{AsyncRead, AsyncWrite};
  use tokio::sync::mpsc;

  pub struct MockAsyncStream {
    pub read_data: Vec<u8>,
    pub write_data: Vec<u8>,
    write_error: Option<String>,
  }

  impl MockAsyncStream {
    pub fn new() -> Self {
      MockAsyncStream {
        read_data: vec![],
        write_data: vec![],
        write_error: None,
      }
    }

    pub fn with_write_error(&mut self, error: String) {
      self.write_error = Some(error);
    }

    pub fn with_read_data(&mut self, data: Vec<u8>) {
      self.read_data = data;
    }

    #[allow(dead_code)]
    pub fn with_write_data(&mut self, data: Vec<u8>) {
      self.write_data = data;
    }
  }

  pub struct MockMpscStream {
    // don't support large data
    read_rx: mpsc::Receiver<Vec<u8>>,
    write_tx: mpsc::Sender<Vec<u8>>,
  }

  impl AsyncRead for MockMpscStream {
    fn poll_read(
      mut self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
      buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
      let data = self.read_rx.try_recv();

      if data.is_err() {
        let err = data.err().unwrap();
        match err {
          mpsc::error::TryRecvError::Disconnected => {
            // finsiehd
            return std::task::Poll::Ready(Ok(()));
          }
          mpsc::error::TryRecvError::Empty => {
            let waker = cx.waker().clone();
            tokio::spawn(async move {
              tokio::time::sleep(std::time::Duration::from_millis(10)).await;
              waker.wake_by_ref();
            });

            return std::task::Poll::Pending;
          }
        }
      }

      let read_data = data.unwrap();
      let remainning = buf.remaining();
      if remainning < read_data.len() {
        buf.put_slice(&read_data[0..remainning]);
      } else {
        buf.put_slice(&read_data);
      }
      std::task::Poll::Ready(Ok(()))
    }
  }

  impl AsyncWrite for MockMpscStream {
    fn is_write_vectored(&self) -> bool {
      return false;
    }

    fn poll_flush(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_write(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
      let res = self.write_tx.try_send(buf.to_vec());
      if res.is_err() {
        // closed
        return std::task::Poll::Ready(Err(std::io::Error::new(
          std::io::ErrorKind::AddrInUse,
          "mock error",
        )));
      }

      std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_write_vectored(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      _bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
      std::task::Poll::Ready(Ok(0))
    }
  }

  impl AsyncRead for MockAsyncStream {
    fn poll_read(
      mut self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
      let remainning = buf.remaining();
      if remainning < self.read_data.len() {
        buf.put_slice(&self.read_data[0..remainning]);
        self.read_data = self.read_data[remainning..].to_vec();
      } else {
        buf.put_slice(&self.read_data);
        self.read_data = vec![];
      }

      std::task::Poll::Ready(Ok(()))
    }
  }

  impl AsyncWrite for MockAsyncStream {
    fn is_write_vectored(&self) -> bool {
      return false;
    }

    fn poll_flush(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_write(
      mut self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
      if self.write_error.is_some() {
        let err = self.write_error.take().unwrap();
        return std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::NotFound, err)));
      }
      self.write_data.append(&mut buf.to_vec());
      std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_write_vectored(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      _bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
      std::task::Poll::Ready(Ok(0))
    }
  }

  use crate::frame::{Cmd, Frame, HEADER_SIZE};

  // disable the ping
  pub fn test_smux_config() -> SmuxConfig {
    let mut config = SmuxConfig::default();
    config.keep_alive_disable = true;
    config
  }

  #[tokio::test]
  async fn test_session_client() {
    let sid: Sid = 3;
    let (read_tx, read_rx) = mpsc::channel(1024);
    let (write_tx, mut write_rx) = mpsc::channel(1024);
    let conn = MockMpscStream { read_rx, write_tx };
    let mut client = Session::new(conn, test_smux_config(), true).unwrap();
    let mut stream = client.open_stream().await.unwrap();
    assert_eq!(sid, stream.sid());

    // opening stream writes some data to the remote
    let data = write_rx.recv().await;
    assert!(data.is_some());
    let data = data.unwrap();
    let frame = Frame::from_buf(&data).unwrap().unwrap();
    assert!(matches!(frame.cmd, Cmd::Sync));

    // mock remote sending data to the stream
    let mut frame = Frame::new_v1(Cmd::Psh, sid);
    frame.with_data("hello!".as_bytes().to_vec());
    read_tx.send(frame.get_buf().unwrap()).await.unwrap();

    let msg = stream.recv_message().await.unwrap().unwrap();
    assert_eq!(String::from_utf8_lossy(&msg).as_ref(), "hello!");

    // write some data
    stream
      .send_message("world!".as_bytes().to_vec())
      .await
      .unwrap();

    let data = write_rx.recv().await.unwrap();
    let frame = Frame::from_buf(&data).unwrap().unwrap();
    assert!(matches!(frame.cmd, Cmd::Psh));
    let frame_data = &data[(HEADER_SIZE as usize)..HEADER_SIZE + (frame.length as usize)];
    assert_eq!(String::from_utf8_lossy(frame_data).as_ref(), "world!");

    // write again
    stream
      .send_message("world!!".as_bytes().to_vec())
      .await
      .unwrap();
    let data = write_rx.recv().await.unwrap();
    let frame = Frame::from_buf(&data).unwrap().unwrap();
    assert!(matches!(frame.cmd, Cmd::Psh));
    let frame_data = &data[(HEADER_SIZE as usize)..HEADER_SIZE + (frame.length as usize)];
    assert_eq!(String::from_utf8_lossy(frame_data).as_ref(), "world!!");

    // close the stream should also cleanup its frame data
    drop(stream);

    // clean up after a while
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(client.sid_tx_map.lock().await.get(&sid).is_none());
    assert!(client.sid_rx_map.lock().await.get(&sid).is_none());
    assert!(client.sid_close_tx_map.lock().await.get(&sid).is_none());

    // the remote should also receive the fin
    let data = write_rx.recv().await.unwrap();
    let frame = Frame::from_buf(&data).unwrap().unwrap();
    assert!(matches!(frame.cmd, Cmd::Fin));
  }

  #[tokio::test]
  async fn test_session_server() {
    let (read_tx, read_rx) = mpsc::channel(1024);
    let (write_tx, mut write_rx) = mpsc::channel(1024);
    let conn = MockMpscStream { read_rx, write_tx };
    let mut server = Session::new(conn, test_smux_config(), false).unwrap();

    let sid = 3;
    // send nop frame
    let frame = Frame::new_v1(Cmd::Nop, 0);
    read_tx.send(frame.get_buf().unwrap()).await.unwrap();
    // mock sync frame
    let frame = Frame::new_v1(Cmd::Sync, sid);
    read_tx.send(frame.get_buf().unwrap()).await.unwrap();

    let mut stream = server.accept_stream().await.unwrap();
    assert_eq!(stream.sid(), sid);

    // ok to write some data
    stream
      .send_message("Hello from server".as_bytes().to_vec())
      .await
      .unwrap();

    let data = write_rx.recv().await.unwrap();
    let frame = Frame::from_buf_with_data(&data).unwrap().unwrap();
    assert_eq!(
      String::from_utf8_lossy(&frame.data.unwrap()),
      "Hello from server"
    );

    // drop server to make stream stop
    drop(server);

    // clean up after a while
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let res = stream.send_message(vec![0; 10]).await;
    assert!(res.is_err());
    assert!(matches!(res.err().unwrap(), TokioSmuxError::StreamClosed));
  }

  #[tokio::test]
  async fn test_session_error() {
    let (_read_tx, read_rx) = mpsc::channel(1024);
    let (write_tx, write_rx) = mpsc::channel(1024);
    let conn = MockMpscStream { read_rx, write_tx };
    let mut client = Session::new(conn, test_smux_config(), true).unwrap();

    // close the write channel to mock error
    drop(write_rx);

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // write some data to pretend inner write operation
    let stream = client.open_stream().await;
    assert!(stream.is_err());
    assert_eq!(stream.err().unwrap().to_string(), "mock error");

    // expect inner error
    let stream = client.open_stream().await;
    assert!(stream.is_err());
    assert_eq!(stream.err().unwrap().to_string(), "mock error");
    // still get same error
    let stream = client.open_stream().await;
    assert!(stream.is_err());
    assert_eq!(stream.err().unwrap().to_string(), "mock error");

    assert!(client.get_inner_err().await.is_some());
  }
}
