//! Replay mock for the Fly.io provider **control plane** — a tiny no-dependency
//! HTTP server seeded with the documented `api.machines.dev/v1` + `api.fly.io`
//! GraphQL shapes. Exercises `FlyProvider` create (app-exists → allocate a
//! dedicated IPv4 → create the sshd machine) → list (ledger) → destroy (delete
//! the app), request encoding, and the leak-safety ledger. Mirrors `vps_mock`.
//!
//! Scope: control plane only (skip_ready_wait bypasses the ssh reachability
//! wait). The ssh data path is proven by `fly_live` against a real org.
//!
//! Run: `cargo test -p superzej-svc --test fly_mock`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use superzej_svc::fly::{FlyProvider, FlySpec, IrohInject};
use superzej_svc::provider::RemoteProvider;
use superzej_svc::vps::registry;

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    body: String,
    auth: String,
}

fn start_mock() -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = recorded.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let rec = rec.clone();
            thread::spawn(move || handle(stream, rec));
        }
    });
    (format!("http://127.0.0.1:{port}"), recorded)
}

fn handle(stream: TcpStream, rec: Arc<Mutex<Vec<Recorded>>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut auth = String::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            auth = h
                .split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    rec.lock().unwrap().push(Recorded {
        method: method.clone(),
        path: path.clone(),
        body: body.clone(),
        auth,
    });

    let managed = r#"{"managed-by":"superzej","sz-host":"h1"}"#;
    let resp = if path == "/graphql" {
        // Two GraphQL calls: app_ips (no v4 yet) then allocate (returns the IP).
        if body.contains("allocateIpAddress") {
            r#"{"data":{"allocateIpAddress":{"ipAddress":{"id":"ip_x","address":"137.0.0.99","type":"v4"}}}}"#.to_string()
        } else {
            r#"{"data":{"app":{"ipAddresses":{"nodes":[]}}}}"#.to_string()
        }
    } else if method == "GET" && path.starts_with("/v1/apps/") && !path.contains("/machines") {
        // ensure_app: the app already exists.
        r#"{"name":"sz-app","status":"deployed"}"#.to_string()
    } else if method == "POST" && path.ends_with("/machines") {
        format!(
            r#"{{"id":"90810","name":"sz-fly-1","state":"created","config":{{"metadata":{managed}}}}}"#
        )
    } else if method == "DELETE" && path.starts_with("/v1/apps/") {
        r#"{"ok":true}"#.to_string()
    } else {
        let _ = writer.write_all(
            b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}",
        );
        return;
    };
    let _ = writer.write_all(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{resp}",
            resp.len()
        )
        .as_bytes(),
    );
}

fn spec(base: &str, tmp: &std::path::Path) -> FlySpec {
    FlySpec {
        api_base: format!("{base}/v1"),
        graphql_url: format!("{base}/graphql"),
        token: "mock-token".into(),
        org_slug: String::new(),
        name: "sz-fly-1".into(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 0,
        max_lifetime_secs: 0,
        key_path: tmp.join("key"),
        pubkey: "ssh-ed25519 MOCKKEY superzej".into(),
        iroh: None,
        skip_ready_wait: true,
    }
}

#[test]
fn create_list_destroy_with_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("SUPERZEJ_DIR", tmp.path()) };
    let (base, recorded) = start_mock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let p = FlyProvider::new(spec(&base, tmp.path()));

    // --- create: app-exists → allocate IPv4 → create machine; ledger finalized.
    let handle = rt.block_on(p.create()).expect("create");
    assert_eq!(handle.id, "sz-fly-1");
    // The exec handle points at the allocated dedicated IPv4 on port 22.
    match handle.exec {
        superzej_svc::provider::ExecKind::Ssh(t) => {
            assert_eq!(t.host, "137.0.0.99");
            assert_eq!(t.port, 22);
        }
        other => panic!("expected Ssh exec, got {other:?}"),
    }
    let reqs = recorded.lock().unwrap().clone();
    // A GraphQL allocate call was made (bearer auth), and a machine POST carrying
    // scoped metadata + the sshd wiring.
    let alloc = reqs
        .iter()
        .find(|r| r.path == "/graphql" && r.body.contains("allocateIpAddress"));
    assert!(alloc.is_some(), "allocated an IPv4");
    let create = reqs
        .iter()
        .find(|r| r.method == "POST" && r.path.ends_with("/machines"))
        .expect("machine create POST");
    assert_eq!(create.auth, "Bearer mock-token");
    let cbody: serde_json::Value = serde_json::from_str(&create.body).unwrap();
    assert_eq!(cbody["config"]["metadata"]["managed-by"], "superzej");
    assert!(
        cbody["config"]["init"]["exec"][2]
            .as_str()
            .unwrap()
            .contains("sshd")
    );
    assert!(cbody["config"]["services"][0]["ports"][0]["port"] == 22);
    // No iroh injection (spec `iroh: None`) ⇒ no `env` object: today's behavior.
    assert!(
        cbody["config"].get("env").is_none(),
        "absent iroh injection leaves config free of an env object"
    );

    // Ledger finalized (provider=fly, machine id + ip recorded).
    let rec = registry::read("sz-fly-1").expect("ledger record");
    assert_eq!(rec.state, "ready");
    assert_eq!(rec.provider, "fly");
    assert_eq!(rec.instance_id, "90810");
    assert_eq!(rec.ip, "137.0.0.99");

    // --- list: ledger-based.
    assert_eq!(rt.block_on(p.list()).expect("list"), vec!["sz-fly-1"]);

    // --- destroy: deletes the app (cascades machine + IP); ledger cleared.
    rt.block_on(p.destroy("sz-fly-1")).expect("destroy");
    let last = recorded.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.method, "DELETE");
    assert!(
        last.path.starts_with("/v1/apps/"),
        "delete app: {}",
        last.path
    );
    assert!(
        registry::read("sz-fly-1").is_none(),
        "ledger cleared on destroy"
    );

    // Idempotent destroy.
    rt.block_on(p.destroy("never-existed"))
        .expect("idempotent destroy");

    // --- iroh injection (present case): rebuild the provider on the SAME
    // process-wide `SUPERZEJ_DIR` (this file keeps a single #[test] because the
    // registry lives under that env var, set process-wide — a second parallel
    // #[test] would race it). The machine-create POST must now carry a
    // `config.env` with the three `SUPERZEJ_*` call-home keys. The exhaustive
    // body-builder coverage lives in `machines::tests`.
    let mut s = spec(&base, tmp.path());
    s.name = "sz-fly-iroh".into();
    s.iroh = Some(IrohInject {
        home_node: "home-endpoint-id".into(),
        sandbox_auth: "auth-token-xyz".into(),
        sandbox_id: "sz-fly-iroh".into(),
    });
    let pi = FlyProvider::new(s);
    rt.block_on(pi.create()).expect("create with iroh");
    let create = recorded
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|r| r.method == "POST" && r.path.ends_with("/machines"))
        .cloned()
        .expect("machine create POST (iroh)");
    let ibody: serde_json::Value = serde_json::from_str(&create.body).unwrap();
    let env = &ibody["config"]["env"];
    assert_eq!(env["SUPERZEJ_HOME_NODE"], "home-endpoint-id");
    assert_eq!(env["SUPERZEJ_SANDBOX_AUTH"], "auth-token-xyz");
    assert_eq!(env["SUPERZEJ_SANDBOX_ID"], "sz-fly-iroh");
    // Additive: the ssh key + service wiring is still present alongside iroh.
    assert_eq!(
        ibody["config"]["files"][0]["guest_path"],
        "/root/.ssh/authorized_keys"
    );
    assert!(ibody["config"]["services"][0]["ports"][0]["port"] == 22);
}
