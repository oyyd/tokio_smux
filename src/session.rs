use crate::config::SmuxConfig;
use crate::error::{Result, TokioSmuxError};
use crate::session_inner::{ReadRequest, SessionInner, WriteRequest};
use crate::stream::Stream;
use crate::{Cmd, Frame};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};

const MAX_STREAMS: usize = 65535;
const MAX_WRITE_REQ: usize = 1024;

// Consume reading frames and split them into:
// - Sync frames
// - Fin and Psh frames by sid (group by sid)
// Nop frames are discarded.
//
// When receive sync frames, we also need to create a sid sender of sid_tx_map.
// When receive fin and psh frames, discard them if the sid sender doesn't exist.
struct FrameSpliter {
  new_frame_rx: mpsc::Receiver<ReadRequest>,

  sync_tx: mpsc::Sender<Frame>,

  sid_tx_map: DashMap<u32, mpsc::Sender<Frame>>,

  sid_rx_map: Arc<DashMap<u32, mpsc::Receiver<Frame>>>,
}

impl FrameSpliter {
  pub async fn run(&mut self) {
    let result = self.run_inner().await;
    // TODO handle error
  }

  async fn run_inner(&mut self) -> Result<()> {
    loop {
      let read_req = self.new_frame_rx.recv().await;
      if read_req.is_none() {
        break;
      }

      let read_req = read_req.unwrap();
      let sid = read_req.frame.sid;

      match read_req.frame.cmd {
        Cmd::Sync => {
          self.sync_tx.send(read_req.frame).await?;
          if !self.sid_tx_map.contains_key(&sid) {
            // TODO
            let buffer = 1024;
            let (tx, rx) = mpsc::channel(buffer);
            self.sid_tx_map.insert(sid, tx);
            self.sid_rx_map.insert(sid, rx);
          }
        }
        Cmd::Fin | Cmd::Psh => {
          if !self.sid_tx_map.contains_key(&sid) {
            continue; // discard
          }

          let tx = self.sid_tx_map.get(&sid).unwrap();
          tx.send(read_req.frame).await?;
        }
        Cmd::Udp => {
          // discard since we don't support
        }
        Cmd::Nop => {
          // discard
        }
      }
    } // loop end

    Ok(())
  }
}

pub struct Session {
  config: SmuxConfig,

  // current stream id
  sid: u32,
  // streams_controllers: DashMap<u32, Arc<RwLock<StreamController>>>,
  closed_tx: broadcast::Sender<()>,
  closed_rx: broadcast::Receiver<()>,
  closed: bool,

  write_tx: mpsc::Sender<WriteRequest>,

  go_away: bool,

  sync_rx: mpsc::Receiver<Frame>,
  sid_frames_rx_map: Arc<DashMap<u32, mpsc::Receiver<Frame>>>,
}

// TODO impl drop for cleaning
impl Session {
  pub fn new<T: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    conn: T,
    config: SmuxConfig,
    is_client: bool,
  ) -> Result<Self> {
    config.verify_config()?;

    // init SessionInner
    let (closed_tx, closed_rx) = broadcast::channel::<()>(MAX_STREAMS);
    let (write_tx, write_rx) = mpsc::channel(MAX_WRITE_REQ);
    let (mut inner, new_frame_rx, _) = SessionInner::new(conn, write_rx, closed_rx.resubscribe());
    tokio::spawn(async move {
      inner.run().await;
    });

    // init FrameSpliter
    let buffer = 1024;
    let (sync_tx, sync_rx) = mpsc::channel(buffer);
    let sid_rx_map = Arc::new(DashMap::new());
    let sid_frames_rx_map = {
      let map = sid_rx_map.clone();
      let mut spliter = FrameSpliter {
        new_frame_rx,
        sync_tx,
        sid_tx_map: DashMap::new(),
        sid_rx_map,
      };
      tokio::spawn(async move {
        spliter.run().await;
      });
      map
    };

    let session = Self {
      config,
      sid: match is_client {
        true => 1,
        false => 0,
      },
      // streams_controllers: DashMap::new(),
      closed_tx,
      closed_rx,
      closed: false,

      write_tx,
      go_away: false,

      sid_frames_rx_map,
      sync_rx,
    };

    Ok(session)
  }

  pub async fn close(&mut self) -> Result<()> {
    // only once
    if self.closed {
      return Ok(());
    }
    self.closed_tx.send(())?;
    self.closed = true;
    Ok(())
  }

  pub async fn open_stream(&mut self) -> Result<Stream> {
    if self.closed {
      return Err(TokioSmuxError::SessionClosed);
    }

    // Check if stream id overflows.
    if self.go_away {
      return Err(TokioSmuxError::SessionGoAway);
    }

    self.sid += 2;
    if self.sid == self.sid % 2 {
      self.go_away = true;
      return Err(TokioSmuxError::SessionGoAway);
    }
    let sid = self.sid;

    // New stream and write sync cmd.
    let frame = Frame::new(self.config.version, Cmd::Sync, sid);
    self.write_frame(frame).await?;
    let stream = self.new_stream(sid);
    Ok(stream)
  }

  // TODO don't give lock to users
  pub async fn accept_stream(&mut self) -> Result<Stream> {
    let frame = self.sync_rx.recv().await;

    if frame.is_none() {
      return Err(TokioSmuxError::SessionClosed);
    }

    let frame = frame.unwrap();
    let sid = frame.sid;

    let stream = self.new_stream(sid);

    Ok(stream)
  }

  // 1. New a stream.
  // 2. Create stream controller and push it.
  fn new_stream(&mut self, sid: u32) -> Stream {
    // TODO
    let (frame_tx, frame_rx) = mpsc::channel(1024);
    let (write_tx, write_rx) = mpsc::channel(1024);
    let (close_tx, close_rx) = oneshot::channel();
    let stream = Stream::new(sid, frame_rx, write_tx, close_rx);
    // TODO
    // let controller = StreamController {};
    // let controller = Arc::new(RwLock::new(controller));
    // self.streams_controllers.insert(sid, controller);

    stream
  }

  fn remove_stream_controller(&mut self, sid: u32) {
    // if self.streams_controllers.contains_key(&sid) {
    //   self.streams_controllers.remove(&sid);
    // }
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

#[cfg(test)]
pub mod test {
  use crate::{session::Session, SmuxConfig, TokioSmuxError};
  use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

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

    pub fn with_write_data(&mut self, data: Vec<u8>) {
      self.write_data = data;
    }
  }

  impl AsyncRead for MockAsyncStream {
    fn poll_read(
      mut self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
      // TODO check this
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

  #[tokio::test]
  async fn test_session() {
    let stream = MockAsyncStream::new();
    // let stream = tokio::net::TcpStream::connect("127.0.0.1:1234")
    //   .await
    //   .unwrap();

    let session = Session::new(stream, SmuxConfig::default(), true);
  }
}
