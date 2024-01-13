# tokio_smux

[![Build And Test](https://github.com/oyyd/tokio_smux/actions/workflows/build_and_test.yml/badge.svg)](https://github.com/oyyd/tokio_smux/actions) [![crates.io](https://img.shields.io/crates/v/tokio_smux.svg)](https://crates.io/crates/tokio_smux)

Tokio_smux is an implementation of [smux](https://github.com/xtaci/smux/) in Rust, which is a stream multiplexing library in Golang.

Tokio_smux can work with tokio [TcpStream](https://docs.rs/tokio/latest/tokio/net/struct.TcpStream.html) and [KcpStream](https://docs.rs/tokio_kcp/latest/tokio_kcp/struct.KcpStream.html). It can also be used with any streams that implement tokio's `AsyncRead` and `AsyncWrite` traits.

## Docs

[https://docs.rs/tokio_smux](https://docs.rs/tokio_smux)

## Usage Example

See [examples](./examples)

## Smux Protocol Implementation

The smux protocl version 2 is not yet supported.

**Why doesn't `Stream` of tokio_smux impl `AsyncRead` and `AysncWrite` itself?**

Becuase the smux protocol uses frames, means all user data transfered is wrapped in frames of fixed lengths. Similar to the websocket protocol.

Using frames has its benifits: it maintains message boundaries.
For example, you can send many one byte data messages to the remote, and receive them one by one in the remote.

It's still feasible to wrap the current APIs of `Stream` to provide `AsyncRead` and `AsyncWrite`. However, this approach introduces additional overhead and loses message boundaries.
