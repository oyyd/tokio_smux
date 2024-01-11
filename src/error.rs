use thiserror::Error;

pub type Result<T> = std::result::Result<T, TokioSmuxError>;

#[derive(Debug, Error, Clone)]
pub enum TokioSmuxError {
  #[error("receive invalid cmd: {cmd}")]
  FrameParseCmdError { cmd: u8 },

  #[error("failed to create frame from data")]
  FrameCreateError,

  #[error("receive invalid config: {msg}")]
  InvalidConfig { msg: String },

  #[error("session closed")]
  SessionClosed,

  #[error("stream id overflows, should start a new connection")]
  SessionGoAway,

  #[error("stream closed")]
  StreamClosed,

  #[error("stream receives unexpected cmd: {cmd_value}")]
  StreamReceiveUnexpectedCmd { cmd_value: u8 },

  #[error("tokio recv error: {inner}")]
  TokioRecvError { inner: String },

  #[error("tokio send error: {inner}")]
  TokioSendError { inner: String },

  #[error("receive inner stream err: {msg}")]
  InnerStreamError { msg: String },

  #[error("{msg}")]
  Default { msg: String },
}

impl From<std::io::Error> for TokioSmuxError {
  fn from(value: std::io::Error) -> Self {
    TokioSmuxError::Default {
      msg: value.to_string(),
    }
  }
}

impl From<tokio::sync::oneshot::error::RecvError> for TokioSmuxError {
  fn from(value: tokio::sync::oneshot::error::RecvError) -> Self {
    Self::TokioRecvError {
      inner: value.to_string(),
    }
  }
}

impl From<tokio::sync::broadcast::error::RecvError> for TokioSmuxError {
  fn from(value: tokio::sync::broadcast::error::RecvError) -> Self {
    Self::TokioRecvError {
      inner: value.to_string(),
    }
  }
}

impl<T> From<tokio::sync::broadcast::error::SendError<T>> for TokioSmuxError {
  fn from(value: tokio::sync::broadcast::error::SendError<T>) -> Self {
    Self::TokioSendError {
      inner: value.to_string(),
    }
  }
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for TokioSmuxError {
  fn from(value: tokio::sync::mpsc::error::SendError<T>) -> Self {
    Self::TokioRecvError {
      inner: value.to_string(),
    }
  }
}
