use std::io::{Cursor, Read, Write};

use crate::error::{Result, TokioSmuxError};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

#[derive(Clone, Copy)]
pub enum Cmd {
  // stream open
  Sync,
  // stream close, a.k.a EOF mark
  Fin,
  // data push
  Psh,
  // no operation
  Nop,
  // protocol version 2 extra commands
  // notify bytes consumed by remote peer-end
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
  pub sid: u32,
  // Extra data, whos length should equal to length.
  pub data: Option<Vec<u8>>,
}

impl Frame {
  pub fn new(ver: u8, cmd: Cmd, sid: u32) -> Self {
    Self {
      ver,
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
      data: None, // TODO when should we read data?
    }))
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
