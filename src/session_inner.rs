use std::borrow::BorrowMut;
use std::future::{self, Future};
use std::time;

use crate::error::{Result, TokioSmuxError};
use crate::frame::HEADER_SIZE;
use crate::{Cmd, Frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};

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
  write_rx: mpsc::Receiver<WriteRequest>,

  // Receive close request and stop the connection.
  close_rx: broadcast::Receiver<()>,

  // Send new frames to outside.
  recv_tx: mpsc::Sender<ReadRequest>,

  read_buf: Vec<u8>,

  read_finished: bool,
}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> SessionInner<T> {
  // TODO receive renders from outside
  pub fn new(
    conn: T,
    write_rx: mpsc::Receiver<WriteRequest>,
    close_rx: broadcast::Receiver<()>,
  ) -> (Self, mpsc::Receiver<ReadRequest>) {
    let buffer: usize = 1024;
    let (recv_tx, recv_rx) = mpsc::channel(buffer);

    let inner = Self {
      conn,
      write_rx,
      close_rx,
      recv_tx,
      read_buf: vec![],
      read_finished: false,
    };

    (inner, recv_rx)
  }

  pub(crate) fn conn_ref(&self) -> &T {
    &self.conn
  }

  pub async fn run(&mut self) {
    // TODO add way to tell session the connection is broken
    self.run_inner().await.unwrap();
  }

  async fn read_with(conn: &mut T, data: &mut [u8], read_finished: bool) -> Result<usize> {
    if read_finished {
      future::pending::<()>().await;
      return Ok(0);
    }

    let size = conn.read(data).await?;
    Ok(size)
  }

  // TODO add keep-alive looping
  // Should return Err only when the error is not recoverable.
  async fn run_inner(&mut self) -> Result<()> {
    let mut data: Vec<u8> = vec![0; 65535];

    // NOTE: Always ensure the cancel safety.
    // TODO hot path, is using select! correctly here?
    loop {
      let conn = self.conn.borrow_mut();
      let read_finished = self.read_finished;

      tokio::select! {
        req = self.write_rx.recv() => {
          if req.is_none() {
            // Write_rx is closed, means the session is removed.
            break;
          }
          self.handle_write_req(req.unwrap()).await?;
        }
        size = SessionInner::read_with(conn, &mut data, read_finished) => {
          let size = size?;
          if size == 0 {
            // remote stops writing
            self.read_finished = true;
            continue;
          }
          self.handle_read_data(&data[0..size])?;
        }
        _ = self.close_rx.recv() => {
          self.handle_before_stop()?;
          break; // don't loop anymore when closed
        }
      }
    }

    Ok(())
  }

  async fn handle_write_req(&mut self, mut req: WriteRequest) -> Result<()> {
    let data = req.frame.get_buf()?;

    // TODO is using write() different?
    self.conn.write_all(&data).await?;

    if req.finish_tx.is_none() {
      return Ok(());
    }

    let finish_tx = req.finish_tx.take().unwrap();
    let res = finish_tx.send(());
    if res.is_err() {
      // TODO closed? what should we do now?
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

#[cfg(test)]
mod test {
  use crate::session::test::MockAsyncStream;
  use crate::session_inner::SessionInner;
  use crate::session_inner::WriteRequest;
  use crate::{Cmd, Frame};
  use tokio::sync::oneshot;
  use tokio::sync::{broadcast, mpsc};

  #[tokio::test]
  async fn test_session_inner() {
    let stream = MockAsyncStream::new();
    let (write_tx, write_rx) = mpsc::channel(1024);
    let (close_tx, close_rx) = broadcast::channel(1024);

    let (mut inner, read_rx) = SessionInner::new(stream, write_rx, close_rx);

    let join = tokio::spawn(async move {
      inner.run().await;

      inner
    });

    // test write

    let (finish_tx, finish_rx) = oneshot::channel();

    let frame = Frame::new_v1(Cmd::Sync, 1);
    let req = WriteRequest {
      frame,
      finish_tx: Some(finish_tx),
    };
    println!("a");
    write_tx.send(req).await.unwrap();
    // wait for writting to finish
    println!("b");
    finish_rx.await.unwrap();
    println!("c");

    // stop inner
    close_tx.send(()).unwrap();

    println!("d");

    let inner = join.await.unwrap();
    assert!(inner.conn_ref().write_data.len() > 0);
  }
}
