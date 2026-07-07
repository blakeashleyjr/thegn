//! The sandbox-side serving loop: once connected home, accept one iroh bi-stream
//! per exec request and bridge it to a real PTY.
//!
//! Each exec stream: the compositor sends [`Wire::Exec`] first, then streams
//! `Stdin`/`Resize`/`Close`; we spawn the command under a PTY and stream
//! `Stdout`/`Exit` back. PTY I/O is blocking, so it runs on dedicated OS threads
//! bridged to the async pump via channels (mirrors the host's pane relay).

use anyhow::{Context, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use superzej_core::iroh_wire::{ALPN, Hello, Wire, WireDecoder, encode};
use tokio::sync::mpsc;

/// Dial the compositor's home node, authenticate, and serve exec streams until
/// the connection drops.
pub async fn dial_and_serve(
    endpoint: &iroh::Endpoint,
    home: impl Into<iroh::EndpointAddr>,
    hello: Hello,
) -> Result<()> {
    let conn = endpoint
        .connect(home.into(), ALPN)
        .await
        .context("dial home")?;
    // Handshake: open a stream, send Hello, and finish our send side so the
    // compositor sees a clean end-of-hello.
    let (mut hs_send, _hs_recv) = conn.open_bi().await.context("open handshake stream")?;
    hs_send
        .write_all(&encode(&Wire::Hello(hello)))
        .await
        .context("send hello")?;
    hs_send.finish().context("finish hello")?;

    serve_exec_streams(conn).await
}

/// Accept compositor-initiated exec bi-streams and serve each on its own task.
async fn serve_exec_streams(conn: Connection) -> Result<()> {
    loop {
        let (send, recv) = match conn.accept_bi().await {
            Ok(pair) => pair,
            // The connection closed (compositor gone / sandbox torn down).
            Err(_) => return Ok(()),
        };
        tokio::spawn(async move {
            if let Err(e) = serve_one_exec(send, recv).await {
                tracing::debug!("exec stream ended: {e:#}");
            }
        });
    }
}

/// Read the leading [`Wire::Exec`], spawn it under a PTY, and pump both directions.
async fn serve_one_exec(mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    let mut dec = WireDecoder::new();
    let req = match read_frame(&mut recv, &mut dec).await? {
        Some(Wire::Exec(r)) => r,
        _ => return Ok(()), // no request → nothing to serve
    };
    anyhow::ensure!(!req.argv.is_empty(), "empty argv");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: req.rows.max(1),
            cols: req.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(&req.argv[0]);
    cmd.args(&req.argv[1..]);
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &req.cwd {
        cmd.cwd(cwd);
    }
    let mut child = pair.slave.spawn_command(cmd).context("spawn command")?;
    drop(pair.slave); // parent closes its handle to the slave side

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let mut writer = pair.master.take_writer().context("take pty writer")?;
    let master = pair.master;

    // PTY output → async pump (blocking read on a thread; unbounded_send is sync).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Stdin → PTY writer (blocking) on a thread.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Write;
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    // Child reaper → exit code.
    let (exit_tx, mut exit_rx) = mpsc::channel::<i32>(1);
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
        let _ = exit_tx.blocking_send(code);
    });

    loop {
        tokio::select! {
            out = out_rx.recv() => {
                if let Some(bytes) = out {
                    write_frame(&mut send, &Wire::Stdout(bytes)).await?;
                }
            }
            code = exit_rx.recv() => {
                if let Some(c) = code {
                    // Flush whatever PTY output is still buffered, then the exit.
                    while let Ok(bytes) = out_rx.try_recv() {
                        write_frame(&mut send, &Wire::Stdout(bytes)).await?;
                    }
                    write_frame(&mut send, &Wire::Exit(c)).await?;
                    let _ = send.finish();
                    return Ok(());
                }
            }
            inbound = read_frame(&mut recv, &mut dec) => {
                match inbound? {
                    Some(Wire::Stdin(b)) => { let _ = in_tx.send(b); }
                    Some(Wire::Resize { cols, rows }) => {
                        let _ = master.resize(PtySize {
                            rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0,
                        });
                    }
                    // Client closed its half or asked to close: stop reading input,
                    // but keep pumping output until the child exits.
                    Some(Wire::Close) | None => { /* fall through; exit branch ends us */ }
                    _ => {}
                }
            }
        }
    }
}

/// Read the next [`Wire`] frame from an iroh recv stream, or `None` at end-of-stream.
async fn read_frame(recv: &mut RecvStream, dec: &mut WireDecoder) -> Result<Option<Wire>> {
    loop {
        if let Some(w) = dec.next_frame()? {
            return Ok(Some(w));
        }
        let mut buf = [0u8; 16 * 1024];
        match recv.read(&mut buf).await.context("read frame")? {
            None => return Ok(dec.next_frame()?), // stream finished
            Some(0) => continue,
            Some(n) => dec.push(&buf[..n]),
        }
    }
}

/// Write one [`Wire`] frame to an iroh send stream.
async fn write_frame(send: &mut SendStream, w: &Wire) -> Result<()> {
    send.write_all(&encode(w)).await.context("write frame")?;
    Ok(())
}
