use std::io::{Cursor, Write};

use crate::error::{Result, TokioSmuxError};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

pub type Sid = u32;

/// Frame commands of smux protocal.
#[derive(Clone, Copy, Debug)]
pub enum Cmd {
  /// Stream open.
  Sync,
  /// Stream close, a.k.a EOF mark.
  Fin,
  /// Data push.
  Psh,
  /// No operation.
  Nop,
  /// Protocol version 2 extra commands.
  /// Notify bytes consumed by remote peer-end.
  Udp,
}

impl TryFrom<u8> for Cmd {
  type Error = TokioSmuxError;

  fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
    let val = match value {
      0 => Cmd::Sync,
      1 => Cmd::Fin,
      2 => Cmd::Psh,
      3 => Cmd::Nop,
      4 => Cmd::Udp,
      _ => return Err(TokioSmuxError::FrameParseCmdError { cmd: value }),
    };

    Ok(val)
  }
}

impl Into<u8> for Cmd {
  fn into(self) -> u8 {
    match self {
      Cmd::Sync => 0,
      Cmd::Fin => 1,
      Cmd::Psh => 2,
      Cmd::Nop => 3,
      Cmd::Udp => 4,
    }
  }
}

// sizeOfVer    = 1
// sizeOfCmd    = 1
// sizeOfLength = 2
// sizeOfSid    = 4
pub const HEADER_SIZE: usize = 8;

pub struct Frame {
  pub ver: u8,
  pub cmd: Cmd,
  // TODO remove length
  pub length: u16,
  pub sid: Sid,
  // Extra data, whos length should equal to length.
  pub data: Option<Vec<u8>>,
}

impl std::fmt::Debug for Frame {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Frame")
      .field("sid", &self.sid)
      .field("cmd", &self.cmd)
      .field("len", &self.length)
      .finish()
  }
}

impl Frame {
  pub fn new(ver: u8, cmd: Cmd, sid: Sid) -> Self {
    Self {
      ver,
      cmd,
      sid,
      length: 0,
      data: None,
    }
  }

  pub fn new_v1(cmd: Cmd, sid: Sid) -> Self {
    Self {
      ver: 1,
      cmd,
      sid,
      length: 0,
      data: None,
    }
  }

  pub fn with_data(&mut self, data: Vec<u8>) {
    self.length = data.len() as u16;
    self.data = Some(data)
  }

  pub fn get_buf(&self) -> Result<Vec<u8>> {
    let mut real_data_len = 0;
    if self.data.is_some() {
      real_data_len = self.data.as_ref().unwrap().len();
    }
    if (real_data_len as u16) != self.length {
      return Err(TokioSmuxError::Default {
        msg: "unmatch data length".to_string(),
      });
    }

    let buf: Vec<u8> = vec![0; HEADER_SIZE + self.length as usize];
    let mut cur = Cursor::new(buf);
    cur.write_u8(self.ver)?;
    let cmd_val: u8 = self.cmd.into();
    cur.write_u8(cmd_val)?;
    cur.write_u16::<LittleEndian>(self.length)?;
    cur.write_u32::<LittleEndian>(self.sid)?;

    if self.data.is_some() {
      let data = self.data.as_ref().unwrap();
      cur.write(&data)?;
    }

    Ok(cur.into_inner())
  }

  pub fn from_buf(data: &[u8]) -> Result<Option<Self>> {
    if data.len() < HEADER_SIZE {
      return Ok(None);
    }

    let mut cursor = Cursor::new(data);

    let ver = cursor.read_u8()?;
    let cmd_raw = cursor.read_u8()?;
    let cmd: Cmd = cmd_raw.try_into()?;
    let length = cursor.read_u16::<LittleEndian>()?;
    let sid = cursor.read_u32::<LittleEndian>()?;

    Ok(Some(Frame {
      ver,
      length,
      cmd,
      sid,
      data: None,
    }))
  }

  #[allow(dead_code)]
  pub fn from_buf_with_data(data: &[u8]) -> Result<Option<Self>> {
    let frame = Frame::from_buf(data);
    if frame.is_err() {
      return Err(frame.err().unwrap());
    }

    let frame = frame.unwrap();
    if frame.is_none() {
      return Ok(None);
    }

    // not enough data
    let mut frame = frame.unwrap();
    if data.len() < HEADER_SIZE + (frame.length as usize) {
      return Ok(None);
    }

    let frame_data = data[HEADER_SIZE..HEADER_SIZE + (frame.length as usize)].to_vec();
    frame.data = Some(frame_data);
    return Ok(Some(frame));
  }
}

#[test]
fn test_frame() {
  let data: Vec<u8> = vec![
    0, // version
    0, // cmd
    1, 0, // length
    1, 0, 0, 0, // sid
  ];

  let frame: Frame = Frame::from_buf(&data).unwrap().unwrap();

  assert_eq!(frame.ver, 0);
  let cmd_val: u8 = frame.cmd.into();
  assert_eq!(cmd_val, 0);
  assert_eq!(frame.length, 1);
  assert_eq!(frame.sid, 1);

  let data: Vec<u8> = vec![0];

  let frame = Frame::from_buf(&data).unwrap(); // not enough length
  assert!(frame.is_none());
}

#[test]
fn test_frame_data() {
  //
}
