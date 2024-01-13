use crate::frame::{Cmd, Frame, Sid};
use crate::session_inner::ReadRequest;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

// Consume reading frames and split them into:
// - Sync frames
// - Fin and Psh frames by sid (group by sid)
// Nop frames are discarded.
//
// When receive sync frames, we also need to create a sid sender of sid_tx_map.
// When receive fin and psh frames, discard them if the sid sender doesn't exist.
pub(crate) struct ReadFrameGrouper {
  // Close this channel to stop the running.
  pub new_frame_rx: mpsc::Receiver<ReadRequest>,

  pub sync_tx: mpsc::Sender<Frame>,

  // Session could also operate `sid_tx_map` and `sid_rx_map`.
  pub sid_tx_map: Arc<DashMap<Sid, mpsc::Sender<Frame>>>,

  // `sid_rx_map` is shared with session.
  // Items of `sid_rx_map` will be taken away by the session when accepting new streams.
  pub sid_rx_map: Arc<DashMap<Sid, mpsc::Receiver<Frame>>>,

  pub sid_frame_buffer_size: usize,
}

impl ReadFrameGrouper {
  pub async fn run(&mut self) {
    loop {
      let read_req = self.new_frame_rx.recv().await;
      if read_req.is_none() {
        // Connection closed, exit.
        break;
      }

      let read_req = read_req.unwrap();

      match read_req.frame.cmd {
        Cmd::Sync => {
          self.handle_sync(read_req).await;
        }
        Cmd::Fin | Cmd::Psh => {
          self.handle_fin_push(read_req).await;
        }
        Cmd::Udp => {
          // discard since we don't support
        }
        Cmd::Nop => {
          // discard
        }
      }
    } // loop end
  }

  async fn handle_sync(&mut self, read_req: ReadRequest) {
    let sid = read_req.frame.sid;
    if !self.sid_tx_map.contains_key(&sid) {
      let (tx, rx) = mpsc::channel(self.sid_frame_buffer_size);
      self.sid_tx_map.insert(sid, tx);
      self.sid_rx_map.insert(sid, rx);
    }
    let send_sync_tx_res = self.sync_tx.send(read_req.frame).await;
    if send_sync_tx_res.is_err() {
      // session closed
      return;
    }
  }

  async fn handle_fin_push(&mut self, read_req: ReadRequest) {
    let sid = read_req.frame.sid;
    if !self.sid_tx_map.contains_key(&sid) {
      // unexpected, ignore the frame
      log::warn!("[grouper] receive unexecpted frame, sid: {}", sid,);
      return;
    }
    let tx = self.sid_tx_map.get(&sid).unwrap();
    let _ = tx.send(read_req.frame).await;
  }
}

#[cfg(test)]
mod test {
  use crate::frame::{Cmd, Frame};
  use crate::read_frame_grouper::ReadFrameGrouper;
  use crate::session_inner::ReadRequest;
  use dashmap::DashMap;
  use std::sync::Arc;
  use tokio::sync::mpsc;

  #[tokio::test]
  async fn test_grouper() {
    let (new_frame_tx, new_frame_rx) = mpsc::channel(1024);
    let (sync_tx, mut sync_rx) = mpsc::channel(1024);
    let sid_rx_map = Arc::new(DashMap::new());
    let mut grouper = ReadFrameGrouper {
      new_frame_rx,
      sid_frame_buffer_size: 1024,
      sync_tx,
      sid_rx_map: sid_rx_map.clone(),
      sid_tx_map: Arc::new(DashMap::new()),
    };

    // Should create correspond sid_tx_map when receive sync frames.
    let sid = 1;
    new_frame_tx
      .send(ReadRequest {
        frame: Frame::new_v1(Cmd::Sync, sid),
      })
      .await
      .unwrap();
    let mut frame = Frame::new_v1(Cmd::Psh, sid);
    frame.with_data(vec![0; 10]);
    new_frame_tx.send(ReadRequest { frame }).await.unwrap();

    let join = tokio::spawn(async move {
      grouper.run().await;
    });

    let frame = sync_rx.recv().await.unwrap();
    assert!(matches!(frame.cmd, Cmd::Sync));
    let item = sid_rx_map.remove(&sid);
    assert!(item.is_some());
    let (id, mut item_frame_rx) = item.unwrap();
    assert_eq!(id, sid);
    let frame = item_frame_rx.recv().await.unwrap();
    assert_eq!(frame.sid, sid);

    // It's okay to push some nop frames.
    new_frame_tx
      .send(ReadRequest {
        frame: Frame::new_v1(Cmd::Nop, sid),
      })
      .await
      .unwrap();

    // Input new push frame won't create another key-value in sid_rx_map.
    let mut frame = Frame::new_v1(Cmd::Psh, sid);
    frame.with_data(vec![0; 10]);
    new_frame_tx.send(ReadRequest { frame }).await.unwrap();
    item_frame_rx.recv().await.unwrap();
    assert!(sid_rx_map.remove(&sid).is_none());

    // Cloud new_frame_rx should
    drop(new_frame_tx);
    join.await.unwrap();
  }
}
