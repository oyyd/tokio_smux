use crate::error::{Result, TokioSmuxError};
use core::time;
use std::time::Duration;

pub struct SmuxConfig {
  // SMUX Protocol version, support 1
  pub version: u8,
  // Disable keepalive
  pub keep_alive_disable: bool,
  // keep_alive_interval is how often to send a NOP command to the remote
  pub keep_alive_interval: Duration,

  // **NOTE:** Not yet supported.
  // KeepAliveTimeout is how long the session
  // will be closed if no data has arrived
  pub keep_alive_timeout: Duration,

  // Max number of pending writing frames in queue.
  // More writing frames operations will be blocked. Default: 4096.
  pub writing_frame_channel_capacity: usize,

  // Max number of pending reading frames in queue for each stream.
  // More reading frames will be blocked until the frames in queue get consumed.
  // Default: 1024
  pub stream_reading_frame_channel_capacity: usize,
}

impl Default for SmuxConfig {
  fn default() -> Self {
    Self {
      version: 1,
      keep_alive_interval: time::Duration::from_secs(10),
      keep_alive_timeout: time::Duration::from_secs(30),
      keep_alive_disable: false,
      writing_frame_channel_capacity: 4096,
      stream_reading_frame_channel_capacity: 1024,
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
