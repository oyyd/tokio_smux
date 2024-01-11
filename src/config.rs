use crate::error::{Result, TokioSmuxError};
use core::time;
use std::time::Duration;

pub struct SmuxConfig {
  // SMUX Protocol version, support 1,2
  pub version: u8,
  // Disabled keepalive
  pub keep_alive_disable: bool,
  // KeepAliveInterval is how often to send a NOP command to the remote
  pub keep_alive_interval: Duration,
  // KeepAliveTimeout is how long the session
  // will be closed if no data has arrived
  pub keep_alive_timeout: Duration,
  // MaxFrameSize is used to control the maximum
  // frame size to sent to the remote
  pub max_frame_size: i32,
  // MaxReceiveBuffer is used to control the maximum
  // number of data in the buffer pool
  pub max_receive_buffer: i32,
  // MaxStreamBuffer is used to control the maximum
  // number of data per stream
  pub max_stream_buffer: i32,
}

impl Default for SmuxConfig {
  fn default() -> Self {
    Self {
      version: 1,
      keep_alive_interval: time::Duration::from_secs(10),
      keep_alive_timeout: time::Duration::from_secs(30),
      max_frame_size: 32768,
      max_receive_buffer: 4194304,
      max_stream_buffer: 65536,
      keep_alive_disable: false,
    }
  }
}

impl SmuxConfig {
  pub fn verify_config(&self) -> Result<()> {
    if self.version != 1 {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "unsupported protocol version".to_string(),
      });
    }

    if !self.keep_alive_disable {
      if self.keep_alive_interval == time::Duration::from_secs(0) {
        return Err(TokioSmuxError::InvalidConfig {
          msg: "keep-alive interval must be positive".to_string(),
        });
      }
      if self.keep_alive_timeout < self.keep_alive_interval {
        return Err(TokioSmuxError::InvalidConfig {
          msg: "keep-alive timeout must be larger than keep-alive interval".to_string(),
        });
      }
    }

    if self.max_frame_size < 0 {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "max frame size must be positive".to_string(),
      });
    }

    if self.max_frame_size > 65535 {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "max frame size must not be larger than 65535".to_string(),
      });
    }

    if self.max_receive_buffer <= 0 {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "max receive buffer must be positive".to_string(),
      });
    }

    if self.max_stream_buffer <= 0 {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "max stream buffer must be positive".to_string(),
      });
    }

    if self.max_stream_buffer > self.max_receive_buffer {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "max stream buffer must not be larger than max receive buffer".to_string(),
      });
    }

    if self.max_stream_buffer > std::i32::MAX {
      return Err(TokioSmuxError::InvalidConfig {
        msg: "max stream buffer cannot be larger than 2147483647".to_string(),
      });
    }

    Ok(())
  }
}

#[test]
fn test_config() {
  let mut config = SmuxConfig::default();

  assert!(config.verify_config().is_ok());

  config.version = 3;

  assert!(config.verify_config().is_err());
}
