//! Throwaway echo sidecar used by `vance-ipc`'s integration tests.
//!
//! Reads `EchoReq` frames on stdin, writes `EchoResp` frames on stdout,
//! exits when stdin closes. With `--blobs` it serves through
//! `serve_with_blobs` and also echoes / generates raw blobs; without it,
//! plain `serve` (blob requests then fail).

use serde::{Deserialize, Serialize};
use vance_ipc::{serve, serve_with_blobs, Reply};

#[derive(Debug, Serialize, Deserialize)]
enum EchoReq {
    Ping,
    Echo(String),
    /// A blob of `len` bytes follows; reply `Blob { len }` + the same bytes.
    EchoBlob {
        len: u64,
    },
    /// Reply `Blob { len }` + `len` bytes of `i % 253`.
    MakeBlob {
        len: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum EchoResp {
    Pong,
    Echoed(String),
    Blob { len: u64 },
    Failed(String),
}

fn main() -> vance_ipc::Result<()> {
    if std::env::args().any(|a| a == "--blobs") {
        return serve_with_blobs::<EchoReq, EchoResp, _>(|req, blobs| match req {
            EchoReq::Ping => EchoResp::Pong.into(),
            EchoReq::Echo(s) => EchoResp::Echoed(s).into(),
            EchoReq::EchoBlob { len } => match blobs.read_blob(len) {
                Ok(bytes) => Reply {
                    msg: EchoResp::Blob { len },
                    blob: Some(bytes),
                },
                Err(e) => EchoResp::Failed(e.to_string()).into(),
            },
            EchoReq::MakeBlob { len } => Reply {
                msg: EchoResp::Blob { len },
                blob: Some((0..len).map(|i| (i % 253) as u8).collect()),
            },
        });
    }
    serve::<EchoReq, EchoResp, _>(|req| match req {
        EchoReq::Ping => EchoResp::Pong,
        EchoReq::Echo(s) => EchoResp::Echoed(s),
        other => EchoResp::Failed(format!("{other:?} needs --blobs")),
    })
}
