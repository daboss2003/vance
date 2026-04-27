//! Throwaway echo sidecar used by `vance-ipc`'s integration tests.
//!
//! Reads `EchoReq` frames on stdin, writes `EchoResp` frames on stdout,
//! exits when stdin closes.

use serde::{Deserialize, Serialize};
use vance_ipc::serve;

#[derive(Debug, Serialize, Deserialize)]
enum EchoReq {
    Ping,
    Echo(String),
}

#[derive(Debug, Serialize, Deserialize)]
enum EchoResp {
    Pong,
    Echoed(String),
}

fn main() -> vance_ipc::Result<()> {
    serve::<EchoReq, EchoResp, _>(|req| match req {
        EchoReq::Ping => EchoResp::Pong,
        EchoReq::Echo(s) => EchoResp::Echoed(s),
    })
}
