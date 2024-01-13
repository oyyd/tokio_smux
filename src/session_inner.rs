use std::borrow::BorrowMut;
use std::future;

use crate::error::Result;
use crate::frame::HEADER_SIZE;
use crate::frame::{Cmd, Frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time;

pub(crate) struct WriteRequest {
  pub frame: Frame,
  pub finish_tx: Option<oneshot::Sender<()>>,
}

pub(crate) struct ReadRequest {
  pub frame: Frame,
}

// Hold the connection and handle low-level operations, like frames reading/writing.
pub(crate) struct SessionInner<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> {
  conn: T,

  // Receive write requests and handle writing.
  // Cloes write channel to close the SessionInner.
  write_rx: mpsc::Receiver<WriteRequest>,

  // Send new frames to outside.
  recv_tx: mpsc::Sender<ReadRequest>,

  // Buffer read data.
  read_buf: Vec<u8>,

  read_finished: bool,

  keep_alive_interval: Option<time::Duration>,
  // FEATURE: support keep_alive_timeout
}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> SessionInner<T> {
  pub fn new(
    conn: T,
    write_rx: mpsc::Receiver<WriteRequest>,
    recv_tx: mpsc::Sender<ReadRequest>,
  ) -> Self {
    let inner = Self {
      conn,
      write_rx,
      recv_tx,
      read_buf: vec![],
      read_finished: false,
      keep_alive_interval: None,
    };

    inner
  }

  pub fn with_keep_alive_interval(&mut self, duration: time::Duration) {
    self.keep_alive_interval = Some(duration);
  }

  pub async fn run(&mut self) -> Result<()> {
    self.run_inner().await?;
    Ok(())
  }

  async fn read_or_pending_if_finished(
    conn: &mut T,
    data: &mut [u8],
    read_finished: bool,
  ) -> Result<usize> {
    if read_finished {
      future::pending::<()>().await;
      return Ok(0);
    }

    let size = conn.read(data).await?;
    Ok(size)
  }

  async fn keep_alive_tick(interval: Option<&mut time::Interval>) {
    if interval.is_none() {
      future::pending::<()>().await;
      return;
    }

    let i = interval.unwrap();
    i.tick().await;
  }

  // Should return Err only when the error is not recoverable.
  async fn run_inner(&mut self) -> Result<()> {
    let mut data: Vec<u8> = vec![0; 65535];
    let mut interval = self.keep_alive_interval.map(|t| {
      let mut interval = time::interval(t);
      interval.set_missed_tick_behavior(time::MissedTickBehavior::Burst);
      interval
    });

    // REFACTOR Hot path.
    // tokio::TcpStream has apis, like writable(), readable(), that
    // should provide better performance when writing and readding concurrently.
    // But they are not included in AsyncRead/AsyncWrite traits.
    // NOTE: Always ensure the cancelling safety.
    loop {
      let conn = self.conn.borrow_mut();
      let read_finished = self.read_finished;

      tokio::select! {
        // read
        size = SessionInner::read_or_pending_if_finished(conn, &mut data, read_finished) => {
          let size = size?;
          if size == 0 {
            // remote stops writing
            self.read_finished = true;
            continue;
          }
          self.handle_read_data(&data[0..size])?;
        }
        // write
        req = self.write_rx.recv() => {
          if req.is_none() {
            // Session closed, stop running.
            break;
          }
          self.handle_write_req(req.unwrap()).await?;
        }
        // keep alive
        _ = SessionInner::<T>::keep_alive_tick(interval.as_mut()) => {
          self.handle_keep_alive_interval_tick().await?;
        }
      }
    }

    Ok(())
  }

  async fn handle_keep_alive_interval_tick(&mut self) -> Result<()> {
    let frame = Frame::new_v1(Cmd::Nop, 0);
    let buf = frame.get_buf()?;
    self.conn.write_all(&buf).await?;

    Ok(())
  }

  async fn handle_write_req(&mut self, mut req: WriteRequest) -> Result<()> {
    let data = req.frame.get_buf()?;

    self.conn.write_all(&data).await?;

    if req.finish_tx.is_none() {
      return Ok(());
    }

    let finish_tx = req.finish_tx.take().unwrap();
    // ignore stream closed error
    let _ = finish_tx.send(());

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
      if (frame_length as u32 + HEADER_SIZE as u32) > (self.read_buf.len() as u32) {
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
      let recv_tx = self.recv_tx.clone();
      tokio::spawn(async move {
        // Will block if the tx capability is empty.
        // is_err() means the session is closed, therefore ignore the error.
        let _ = recv_tx.send(read_req).await;
      });

      // continue
    }

    Ok(())
  }
}

#[cfg(test)]
mod test {
  use crate::frame::HEADER_SIZE;
  use crate::frame::{Cmd, Frame};
  use crate::session::test::MockAsyncStream;
  use crate::session_inner::SessionInner;
  use crate::session_inner::WriteRequest;
  use tokio::sync::mpsc;
  use tokio::sync::oneshot;

  #[tokio::test]
  async fn test_session_inner() {
    let sid = 3;
    let test_data_size = 65535;
    let mut frame = Frame::new_v1(Cmd::Psh, 3);
    frame.with_data(vec![0; test_data_size]);
    let data_to_read = frame.get_buf().unwrap();

    let mut stream = MockAsyncStream::new();
    stream.with_read_data(data_to_read);

    let buffer = 1024;
    let (recv_tx, mut recv_rx) = mpsc::channel(buffer);
    let (write_tx, write_rx) = mpsc::channel(1024);
    let mut inner = SessionInner::new(stream, write_rx, recv_tx);

    let join = tokio::spawn(async move {
      let err = inner.run().await;

      (inner, err)
    });

    // test read and write
    let (finish_tx, finish_rx) = oneshot::channel();
    let frame = Frame::new_v1(Cmd::Sync, sid);
    let req = WriteRequest {
      frame,
      finish_tx: Some(finish_tx),
    };
    write_tx.send(req).await.unwrap();
    // wait for writting to finish
    finish_rx.await.unwrap();

    let read_req = recv_rx.recv().await;
    assert!(read_req.is_some());
    let read_req = read_req.unwrap();
    assert!(matches!(read_req.frame.cmd, Cmd::Psh));
    assert_eq!(read_req.frame.sid, sid);
    let data = read_req.frame.data.unwrap();
    assert_eq!(data.len(), test_data_size);

    // stop inner to check write data
    drop(write_tx);
    let (inner, _err) = join.await.unwrap();
    assert_eq!(inner.conn.write_data.len(), HEADER_SIZE);
  }

  #[tokio::test]
  async fn test_session_inner_error() {
    let mut stream = MockAsyncStream::new();
    let err_msg = "some failure".to_string();
    stream.with_write_error(err_msg.clone());

    let (write_tx, write_rx) = mpsc::channel(1024);
    let (recv_tx, _recv_rx) = mpsc::channel(1024);
    let mut inner = SessionInner::new(stream, write_rx, recv_tx);

    let join = tokio::spawn(async move { inner.run().await });

    // write something and receive error
    let frame = Frame::new_v1(Cmd::Psh, 3);
    let res = write_tx
      .send(WriteRequest {
        frame,
        finish_tx: None,
      })
      .await;
    assert!(!res.is_err());

    let res = join.await.unwrap();
    assert!(res.is_err());
    println!("err {}", res.err().unwrap().to_string());
  }

  #[tokio::test]
  async fn test_session_inner_keep_alive_interval() {
    let stream = MockAsyncStream::new();

    let (write_tx, write_rx) = mpsc::channel(1024);
    let (recv_tx, _recv_rx) = mpsc::channel(1024);
    let mut inner = SessionInner::new(stream, write_rx, recv_tx);

    let duration = std::time::Duration::from_millis(10);
    inner.with_keep_alive_interval(duration);

    let join = tokio::spawn(async move {
      let res = inner.run().await;
      (inner, res)
    });

    // wait duration
    tokio::time::sleep(std::time::Duration::from_millis(
      (duration.as_millis() * 2).try_into().unwrap(),
    ))
    .await;

    drop(write_tx);

    let (inner, _res) = join.await.unwrap();
    assert!(inner.conn.write_data.len() >= 8);
  }
}
