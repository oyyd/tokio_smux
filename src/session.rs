use crate::config::SmuxConfig;
use crate::error::{Result, TokioSmuxError};
use crate::frame::{self, HEADER_SIZE};
use crate::{Cmd, Frame};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::RwLock;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

// TODO implement Stream apis
pub struct Stream {
  sid: u32,
}

impl Drop for Stream {
  fn drop(&mut self) {
    // TODO should inform Session something?
  }
}

// TODO implement some tokio async read/write traits?
impl Stream {
  pub fn new(sid: u32) -> Self {
    Self { sid }
  }

  pub async fn write(&self) {
    //
  }

  pub async fn read(&self) {
    //
  }
}

const MAX_STREAMS: usize = 65535;
const MAX_WRITE_REQ: usize = 1024;

struct WriteRequest {
  frame: Frame,
  finish_tx: oneshot::Sender<()>,
}

struct ReadRequest {
  frame: Frame,
}

// Hold the connection and handle low-level operations, like frames reading/writing.
struct SessionInner<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> {
  conn: T,

  // Receive write requests and handle writing.
  write_rx: mpsc::Receiver<WriteRequest>,

  // Receive close request and stop the connection.
  closed_rx: broadcast::Receiver<()>,

  // Send new frames to outside.
  recv_tx: mpsc::Sender<ReadRequest>,

  read_buf: Vec<u8>,
}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> SessionInner<T> {
  pub fn new(
    conn: T,
    write_rx: mpsc::Receiver<WriteRequest>,
    closed_rx: broadcast::Receiver<()>,
  ) -> (Self, mpsc::Receiver<ReadRequest>) {
    // TODO
    let buffer: usize = 1024;
    let (recv_tx, recv_rx) = mpsc::channel(buffer);

    let inner = Self {
      conn,
      write_rx,
      closed_rx,
      recv_tx,
      read_buf: vec![],
    };

    (inner, recv_rx)
  }

  pub async fn run(&mut self) {
    // TODO add way to tell session the connection is broken
    self.run_inner().await.unwrap();
  }

  // TODO add keep-alive looping
  // Should return Err only when the error is not recoverable.
  async fn run_inner(&mut self) -> Result<()> {
    let mut data: Vec<u8> = vec![0; 65535];

    // NOTE: Always ensure the cancel safety.
    // TODO hot path, is using select! correctly here?
    loop {
      tokio::select! {
        req = self.write_rx.recv() => {
          if req.is_none() {
            // Write_rx is closed, means the session is removed.
            break;
          }
          self.handle_write_req(req.unwrap()).await?;
        }
        size = self.conn.read(&mut data) => {
          if size.is_err() {
            return Err(TokioSmuxError::Default { msg: format!("read data failed") })
          }
          let size = size.unwrap();
          self.handle_read_data(&data[0..size])?;
        }
        _ = self.closed_rx.recv() => {
          self.handle_before_stop()?;
          break; // don't loop anymore when closed
        }
      }
    }

    Ok(())
  }

  async fn handle_write_req(&mut self, req: WriteRequest) -> Result<()> {
    let data = req.frame.get_buf()?;

    // TODO is using write() different?
    self.conn.write_all(&data).await?;

    let finish_res: std::prelude::v1::Result<(), ()> = req.finish_tx.send(());
    if finish_res.is_err() {
      return Err(TokioSmuxError::Default {
        msg: "failed to finish_tx.send()".to_string(),
      });
    }

    Ok(())
  }

  fn handle_before_stop(&mut self) -> Result<()> {
    Ok(())
  }

  fn handle_read_data(&mut self, data: &[u8]) -> Result<()> {
    if data.len() == 0 {
      // Remote write side closed, no more data.
      return Ok(());
    }
    // REFACTOR: refactor allocation
    self.read_buf.append(&mut data.to_vec());

    // REFACTOR: or use cursor to refactor
    loop {
      if self.read_buf.len() < HEADER_SIZE {
        break;
      }

      let frame = Frame::from_buf(&self.read_buf[0..HEADER_SIZE])?;
      // Not enough header data. Though we have ensured the data size is enough.
      if frame.is_none() {
        break;
      }
      let mut frame = frame.unwrap();
      let frame_length = frame.length;
      // check if all data ready
      if (frame_length + HEADER_SIZE as u16) > (self.read_buf.len() as u16) {
        // not enough data
        break;
      }

      // pop data
      let mut frame_data: Vec<u8> = vec![0; frame_length as usize];
      frame_data
        .clone_from_slice(&self.read_buf[HEADER_SIZE..HEADER_SIZE + (frame_length as usize)]);
      self.read_buf = self.read_buf[HEADER_SIZE + (frame_length as usize)..].to_vec();
      frame.with_data(frame_data);
      let read_req = ReadRequest { frame };

      // output frame
      // TODO consider should we block here
      let recv_tx = self.recv_tx.clone();
      tokio::spawn(async move {
        // TODO when will errors happen?
        // Will block if the tx capability is empty.
        recv_tx.send(read_req).await.unwrap();
      });

      // continue
    }

    Ok(())
  }
}

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

// TODO
struct StreamController {}

pub struct Session {
  config: SmuxConfig,

  // current stream id
  sid: u32,
  streams_controllers: DashMap<u32, Arc<RwLock<StreamController>>>,

  closed_tx: broadcast::Sender<()>,
  closed_rx: broadcast::Receiver<()>,
  closed: bool,

  write_tx: mpsc::Sender<WriteRequest>,

  go_away: bool,

  sync_rx: mpsc::Receiver<Frame>,
  sid_frames_rx_map: Arc<DashMap<u32, mpsc::Receiver<Frame>>>,
}

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
    let (mut inner, new_frame_rx) = SessionInner::new(conn, write_rx, closed_rx.resubscribe());
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
      streams_controllers: DashMap::new(),
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
    let stream = Stream::new(sid);
    // TODO
    let controller = Arc::new(RwLock::new(StreamController {}));
    self.streams_controllers.insert(sid, controller);

    stream
  }

  fn remove_stream_controller(&mut self, sid: u32) {
    if self.streams_controllers.contains_key(&sid) {
      self.streams_controllers.remove(&sid);
    }
  }

  async fn write_frame(&mut self, frame: Frame) -> Result<()> {
    let (tx, rx) = oneshot::channel::<()>();

    // Append to the write queue.
    self
      .write_tx
      .send(WriteRequest {
        frame,
        finish_tx: tx,
      })
      .await?;

    rx.await?;
    Ok(())
  }
}

#[cfg(test)]
mod test {
  use crate::{session::Session, SmuxConfig};
  use tokio::io::{AsyncRead, AsyncWrite};

  struct MockAsyncStream {}

  impl AsyncRead for MockAsyncStream {
    fn poll_read(
      self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
      buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
      std::task::Poll::Ready(Ok(()))
    }
  }

  impl AsyncWrite for MockAsyncStream {
    fn is_write_vectored(&self) -> bool {
      return false;
    }

    fn poll_flush(
      self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
      self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_write(
      self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
      std::task::Poll::Ready(Ok(0))
    }

    fn poll_write_vectored(
      self: std::pin::Pin<&mut Self>,
      cx: &mut std::task::Context<'_>,
      bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
      std::task::Poll::Ready(Ok(0))
    }
  }

  #[tokio::test]
  async fn test_session() {
    let stream = MockAsyncStream {};
    // let stream = tokio::net::TcpStream::connect("127.0.0.1:1234")
    //   .await
    //   .unwrap();
    let session = Session::new(stream, SmuxConfig::default(), true);
  }
}
