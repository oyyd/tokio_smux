use crate::error::{Result, TokioSmuxError};
use crate::frame::{Cmd, Frame};
use crate::session::WriteRequest;
use tokio::sync::{mpsc, oneshot};

// Three possible closing scenarioes:
// 1. close by remote fin message (should not send fin to remote)
// 2. close by session (should send fin to remote)
// 3. close by stream (should send fin to remote)
pub struct Stream {
  sid: u32,

  // frame_rx gets closed means the session is closed.
  frame_rx: mpsc::Receiver<Frame>,
  write_tx: Option<mpsc::Sender<WriteRequest>>,
  // Disallow write operations after receiving from close_rx.
  close_rx: oneshot::Receiver<()>,

  receive_remote_fin: bool,
  closed: bool,
}

impl Drop for Stream {
  fn drop(&mut self) {
    // Don't send fin if it has received fin.
    if self.receive_remote_fin {
      return;
    }

    let write_tx = self.write_tx.take();
    if write_tx.is_none() {
      return;
    }
    let sid = self.sid;
    let write_tx = write_tx.unwrap();
    tokio::spawn(async move {
      // allow failure
      let _ = Stream::send_fin(sid, write_tx).await;
    });
  }
}

impl Stream {
  // Let session pass these parameters therefore we could control channel capabilities from outside.
  pub fn new(
    sid: u32,
    frame_rx: mpsc::Receiver<Frame>,
    write_tx: mpsc::Sender<WriteRequest>,
    close_rx: oneshot::Receiver<()>,
  ) -> Self {
    Self {
      sid,
      frame_rx,
      write_tx: Some(write_tx),
      close_rx,
      receive_remote_fin: false,
      closed: false,
    }
  }

  pub async fn send_message(&mut self, data: Vec<u8>) -> Result<()> {
    if self.closed || self.receive_remote_fin {
      return Err(TokioSmuxError::StreamClosed);
    }

    if self.is_closed_by_session() {
      return Err(TokioSmuxError::StreamClosed);
    }

    let mut frame = Frame::new_v1(Cmd::Psh, self.sid);
    frame.with_data(data);

    let (finish_tx, finish_rx) = oneshot::channel();
    if self.write_tx.is_some() {
      self
        .write_tx
        .as_ref()
        .unwrap()
        .send(WriteRequest {
          frame,
          finish_tx: Some(finish_tx),
        })
        .await?;
    }

    finish_rx.await?;

    Ok(())
  }

  pub async fn recv_message(&mut self) -> Result<Vec<u8>> {
    // We don't check close_rx here because we still allow the stream to consume
    // rest data.
    // And outside sessions should also close the frame_rx.
    let msg = self.frame_rx.recv().await;
    if msg.is_none() {
      // closed
      return Err(TokioSmuxError::StreamClosed);
    }

    let msg = msg.unwrap();

    match msg.cmd {
      Cmd::Fin => {
        self.receive_remote_fin = true;
        return Err(TokioSmuxError::StreamClosed);
      }
      Cmd::Psh => {
        if msg.data.is_none() {
          return Err(TokioSmuxError::Default {
            msg: "receive unexpected none data".to_string(),
          });
        }
        let data = msg.data.unwrap();
        return Ok(data);
      }
      _ => {
        return Err(TokioSmuxError::StreamReceiveUnexpectedCmd {
          cmd_value: msg.cmd.into(),
        })
      }
    }
  }

  fn is_closed_by_session(&mut self) -> bool {
    // If already closed by stream, return true directly.
    if self.closed {
      return true;
    }

    let res = self.close_rx.try_recv();
    if res.is_ok() {
      self.closed = true;
      return true;
    }

    match res.err().unwrap() {
      oneshot::error::TryRecvError::Closed => {
        // session closed
        self.closed = true;
        return true;
      }
      oneshot::error::TryRecvError::Empty => return false,
    }
  }

  async fn send_fin(sid: u32, write_tx: mpsc::Sender<WriteRequest>) -> Result<()> {
    let frame = Frame::new_v1(Cmd::Fin, sid);

    write_tx
      .send(WriteRequest {
        frame,
        finish_tx: None,
      })
      .await?;
    Ok(())
  }
}

#[cfg(test)]
mod test {
  use crate::frame::{Cmd, Frame};
  use crate::stream::Stream;
  use crate::TokioSmuxError;
  use tokio::sync::mpsc;
  use tokio::sync::oneshot;

  #[tokio::test]
  async fn test_stream() {
    let sid = 1;
    let size = 1024;
    let (frame_tx, frame_rx) = mpsc::channel(size);
    let (write_tx, mut write_rx) = mpsc::channel(size);
    let (_close_tx, close_rx) = oneshot::channel();

    let mut stream = Stream::new(sid, frame_rx, write_tx, close_rx);

    // push some frames to read
    tokio::spawn(async move {
      let mut push_frame = Frame::new_v1(Cmd::Psh, sid);
      push_frame.with_data(vec![0; 10]);
      let _ = frame_tx.send(push_frame).await;

      let fin_frame = Frame::new_v1(Cmd::Fin, sid);
      let _ = frame_tx.send(fin_frame).await;
    });

    // receive all messages
    tokio::spawn(async move {
      loop {
        let res = write_rx.recv().await;
        if res.is_none() {
          break;
        }

        let req = res.unwrap();
        assert_eq!(req.frame.sid, sid);
        // should not receive stream closed msg
        if matches!(req.frame.cmd, Cmd::Fin) {
          panic!("unexpected fin message");
        }

        if req.finish_tx.is_some() {
          let tx = req.finish_tx.unwrap();
          let _ = tx.send(());
        }
      }
    });

    {
      stream.send_message(vec![0; 10]).await.unwrap();
      let msg = stream.recv_message().await.unwrap();
      assert!(msg.len() > 0);

      // test receive fin
      let msg = stream.recv_message().await;
      assert!(msg.is_err());
      let is_stream_close_err = matches!(msg.err().unwrap(), TokioSmuxError::StreamClosed);
      assert!(is_stream_close_err);
      // do not write anymore
      let write_res = stream.send_message(vec![0; 10]).await;
      assert!(write_res.is_err());

      assert!(stream.receive_remote_fin);
    };
  }

  #[tokio::test]
  async fn test_stream_actively_closing() {
    let sid = 1;
    let size = 1024;
    let (_frame_tx, frame_rx) = mpsc::channel(size);
    let (write_tx, mut write_rx) = mpsc::channel(size);
    let (_close_tx, close_rx) = oneshot::channel();

    let stream = Stream::new(sid, frame_rx, write_tx, close_rx);
    let (recv_fin_tx, recv_fin_rx) = oneshot::channel::<()>();

    // receive all messages
    tokio::spawn(async move {
      let mut tx = Some(recv_fin_tx);
      loop {
        let res = write_rx.recv().await;
        if res.is_none() {
          break;
        }

        let req = res.unwrap();
        // notify outside that have received fin
        if matches!(req.frame.cmd, Cmd::Fin) {
          let item = tx.take();
          if item.is_some() {
            item.unwrap().send(()).unwrap();
          }
        }

        if req.finish_tx.is_some() {
          let tx = req.finish_tx.unwrap();
          let _ = tx.send(());
        }
      }
    });

    assert!(!stream.receive_remote_fin);
    drop(stream);
    recv_fin_rx.await.unwrap();
  }

  #[tokio::test]
  async fn test_stream_closed_by_session() {
    let sid = 1;
    let size = 1024;
    let (frame_tx, frame_rx) = mpsc::channel(size);
    let (write_tx, mut write_rx) = mpsc::channel(size);
    let (close_tx, close_rx) = oneshot::channel();

    let mut stream = Stream::new(sid, frame_rx, write_tx, close_rx);
    let (recv_fin_tx, recv_fin_rx) = oneshot::channel::<()>();

    // receive all messages
    tokio::spawn(async move {
      let mut tx = Some(recv_fin_tx);
      loop {
        let res = write_rx.recv().await;
        if res.is_none() {
          break;
        }

        let req = res.unwrap();
        // notify outside that have received fin
        if matches!(req.frame.cmd, Cmd::Fin) {
          let item = tx.take();
          if item.is_some() {
            item.unwrap().send(()).unwrap();
          }
        }

        if req.finish_tx.is_some() {
          let tx = req.finish_tx.unwrap();
          let _ = tx.send(());
        }
      }
    });

    drop(frame_tx);
    close_tx.send(()).unwrap();

    // then, any read, write operations should failed
    let res = stream.send_message(vec![0; 0]).await;
    assert!(res.is_err());
    matches!(res.err().unwrap(), TokioSmuxError::StreamClosed);

    let res = stream.recv_message().await;
    assert!(res.is_err());
    matches!(res.err().unwrap(), TokioSmuxError::StreamClosed);

    // the remote should receive fin
    drop(stream);
    recv_fin_rx.await.unwrap();
  }
}