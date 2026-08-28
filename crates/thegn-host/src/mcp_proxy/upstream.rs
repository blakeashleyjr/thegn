//! One upstream MCP server: a child process spoken to over newline-delimited
//! JSON-RPC on stdio (the MCP stdio contract), with a per-request timeout and a
//! circuit breaker.
//!
//! A background reader thread parses the child's stdout into JSON messages and
//! feeds them over a channel, so a request can wait with a deadline rather than
//! block forever on a wedged upstream — the timeout is what feeds the breaker.
//! Secret resolution happens *before* spawn (the hub resolves each env ref);
//! this type only ever sees concrete env values.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use thegn_core::mcp::proxy::breaker::{Breaker, BreakerConfig, BreakerState};
use thegn_core::mcp::proxy::route;

/// A protocol-level round-trip failure.
enum RpcFail {
    /// Timed out, or the child's pipe closed — a *transport* failure that feeds
    /// the breaker.
    Transport(String),
    /// The upstream answered with a JSON-RPC error — it is alive (does not feed
    /// the breaker), but the call did not succeed.
    Rpc(i32, String),
}

/// A running upstream MCP server.
pub struct Upstream {
    pub name: String,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
    breaker: Breaker,
    timeout: Duration,
    /// Cached `tools/list` result value (`{ "tools": [ … ] }`).
    tools: Value,
    /// The `proxy.tools` glob list (exposure policy) for this instance.
    pub exposure: Vec<String>,
    health_checked_at: Option<Instant>,
}

impl Upstream {
    /// Spawn `argv` (already secret-resolved `env`) as an MCP stdio child and
    /// wait for the handshake (`initialize` + `tools/list`). The argv is wrapped
    /// through the shared `thegn.slice` background cap so a greedy upstream is
    /// bounded like everything else thegn starts.
    pub fn spawn(
        name: &str,
        argv: &[String],
        env: &BTreeMap<String, String>,
        exposure: Vec<String>,
        breaker_cfg: BreakerConfig,
        timeout: Duration,
    ) -> Result<Upstream, String> {
        let wrapped = thegn_core::sandbox_cpucap::wrap_background_argv(argv.to_vec());
        let Some((prog, rest)) = wrapped.split_first() else {
            return Err(format!("upstream `{name}` has no launch command"));
        };
        let mut cmd = Command::new(prog);
        cmd.args(rest);
        // Minimal base env + only the declared (resolved) env — a compromised
        // upstream must not inherit the daemon's whole environment.
        cmd.env_clear();
        for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "SystemRoot"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn upstream `{name}` ({prog}): {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

        // Reader thread: one parsed JSON message per line onto the channel.
        let (tx, rx) = channel::<Value>();
        std::thread::Builder::new()
            .name(format!("mcp-up-{name}"))
            .spawn(move || {
                // Utility: this pump's lag lands on deadline-enforced MCP tool calls (a
                // miss is a breaker-feeding transport failure), not on background scrapes.
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let t = line.trim();
                    if t.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(t) {
                        // best-effort: the receiver is gone once the upstream is dropped.
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        let mut up = Upstream {
            name: name.to_string(),
            child,
            stdin,
            rx,
            next_id: 0,
            breaker: Breaker::new(breaker_cfg),
            timeout,
            tools: json!({ "tools": [] }),
            exposure,
            health_checked_at: None,
        };
        up.handshake()?;
        Ok(up)
    }

    /// The cached `tools/list` result (post-handshake / post-refresh).
    pub fn tools(&self) -> &Value {
        &self.tools
    }

    /// The breaker's state as of `now_ms` (for status/doctor).
    pub fn breaker_state(&self, now_ms: i64) -> BreakerState {
        self.breaker.state(now_ms)
    }

    /// Milliseconds since the last health check, if any.
    pub fn health_age_ms(&self) -> Option<i64> {
        self.health_checked_at
            .map(|t| t.elapsed().as_millis() as i64)
    }

    fn handshake(&mut self) -> Result<(), String> {
        self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "thegn-mcp-proxy", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .map_err(|f| match f {
            RpcFail::Transport(e) => format!("`{}` initialize: {e}", self.name),
            RpcFail::Rpc(c, m) => format!("`{}` initialize rejected ({c}): {m}", self.name),
        })?;
        // MCP requires this notification after a successful initialize.
        self.notify("notifications/initialized", json!({}))?;
        self.refresh_tools()?;
        Ok(())
    }

    /// Re-fetch and cache the upstream's tool list. Returns whether it changed.
    pub fn refresh_tools(&mut self) -> Result<bool, String> {
        let list = self.rpc("tools/list", json!({})).map_err(|f| match f {
            RpcFail::Transport(e) => format!("`{}` tools/list: {e}", self.name),
            RpcFail::Rpc(c, m) => format!("`{}` tools/list error ({c}): {m}", self.name),
        })?;
        let changed = list != self.tools;
        self.tools = list;
        Ok(changed)
    }

    /// A cheap liveness probe for the health tick: a `tools/list` round-trip,
    /// driving the breaker. `now_ms` is the injected clock.
    #[expect(
        dead_code,
        reason = "reserved: unwired mcp-proxy capability, wire-or-remove tracked in THE-16 follow-up"
    )]
    pub fn health_check(&mut self, now_ms: i64) {
        self.health_checked_at = Some(Instant::now());
        match self.rpc("tools/list", json!({})) {
            Ok(list) => {
                self.tools = list;
                self.breaker.on_success();
            }
            Err(RpcFail::Rpc(..)) => self.breaker.on_success(), // answered ⇒ alive
            Err(RpcFail::Transport(_)) => self.breaker.on_failure(now_ms),
        }
    }

    /// Forward a `tools/call` to the upstream, honoring the breaker. Returns the
    /// upstream's `result` on success, or a JSON-RPC `(code, message)` for a
    /// breaker-open, transport, or upstream error.
    pub fn call_tool(
        &mut self,
        tool: &str,
        args: &Value,
        now_ms: i64,
    ) -> Result<Value, (i32, String)> {
        if !self.breaker.allow(now_ms) {
            return Err(route::breaker_open_error(&self.name));
        }
        match self.rpc("tools/call", json!({ "name": tool, "arguments": args })) {
            Ok(result) => {
                self.breaker.on_success();
                Ok(result)
            }
            Err(RpcFail::Rpc(code, msg)) => {
                // The upstream answered — alive, but the call errored. Forward it.
                self.breaker.on_success();
                Err((code, msg))
            }
            Err(RpcFail::Transport(e)) => {
                self.breaker.on_failure(now_ms);
                Err((-32000, format!("upstream `{}` failed: {e}", self.name)))
            }
        }
    }

    /// One request/response round-trip. `Ok` is the `result` value; `Err`
    /// distinguishes a transport failure from an upstream JSON-RPC error.
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, RpcFail> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{req}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| RpcFail::Transport(e.to_string()))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RpcFail::Transport("timed out".into()));
            }
            match self.rx.recv_timeout(remaining) {
                Ok(msg) => {
                    // Only the reply carrying our id is ours; drop notifications
                    // / log lines / out-of-order ids the upstream may emit.
                    if msg.get("id") == Some(&json!(id)) {
                        if let Some(err) = msg.get("error") {
                            let code =
                                err.get("code").and_then(Value::as_i64).unwrap_or(-32000) as i32;
                            let m = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("upstream error")
                                .to_string();
                            return Err(RpcFail::Rpc(code, m));
                        }
                        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(RpcFail::Transport("timed out".into()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RpcFail::Transport("upstream exited".into()));
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        writeln!(self.stdin, "{msg}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| e.to_string())
    }
}

impl Drop for Upstream {
    #[expect(
        clippy::disallowed_methods,
        reason = "reaping an already-killed child in the shim/CLI process — off-loop, and returns immediately"
    )]
    fn drop(&mut self) {
        // best-effort: kill the child; the reader thread then sees EOF and exits.
        let _ = self.child.kill();
        let _ = self.child.wait(); // best-effort: teardown: the child may already have exited or been reaped
    }
}
