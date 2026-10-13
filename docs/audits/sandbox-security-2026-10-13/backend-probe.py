#!/usr/bin/env python3
"""Offline audit probe: extracted production helpers; UDP uses loopback only.
No container runtime or external resolver is contacted. Run from any directory.
"""
from pathlib import Path
import subprocess
import tempfile

repo = Path(__file__).resolve().parents[3]
sandbox = (repo / "crates/thegn-core/src/sandbox.rs").read_text()
reuse = sandbox[sandbox.index("fn mounts_match("):sandbox.index("/// `(running, mounts_ok)` parsed")]
dns = (repo / "crates/thegn-core/src/dns_filter.rs").read_text()
dns = dns[dns.index("use std::net"):dns.index("#[cfg(test)]")]
# Only replace the tracing macro to compile this source excerpt without Cargo.
dns = dns.replace('tracing::warn!(target: "thegn::dns", error = %e, "dns-filter listener failed to start");', 'eprintln!("dns-filter listener failed to start: {e}");')
main = r'''
fn packet(name: &str) -> Vec<u8> {
    let mut q=vec![0x12,0x34,1,0,0,1,0,0,0,0,0,0];
    for label in name.split('.') { q.push(label.len() as u8); q.extend(label.bytes()); }
    q.extend([0,0,1,0,1]); q
}
fn query(port:u16,name:&str)->u8 {
    let sock=std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    sock.send_to(&packet(name),("127.0.0.1",port)).unwrap();
    let mut buf=[0u8;512]; let (n,_)=sock.recv_from(&mut buf).unwrap(); assert!(n>=12); buf[3]&15
}
fn main() {
    let source=std::collections::HashSet::from(["/synthetic/worktree"]);
    assert_eq!(judge_inspect("RUNNING\n0\n/synthetic/worktree\n",&source,false,false),(true,true));
    println!("reuse: NAT->none and rw->ro share same inspected input, reusable=true");
    let extra=std::collections::HashSet::from(["/synthetic/worktree","/synthetic/old-secret"]);
    assert!(mounts_match(&source,&extra,false,false));
    println!("nonsealed removed mount: extra old bind accepted=true");
    let upstream=std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let upstream_addr=upstream.local_addr().unwrap();
    std::thread::spawn(move|| { let mut buf=[0u8;512]; loop {let(n,src)=upstream.recv_from(&mut buf).unwrap();buf[2]=0x81;buf[3]=0;upstream.send_to(&buf[..n],src).unwrap();}});
    let first=get_or_start(DnsPolicy{allow:vec!["allowed.test".into()],block:vec!["blocked.test".into()],upstream:Some(upstream_addr)}).unwrap();
    assert_eq!(query(first,"blocked.test"),3);
    let second=get_or_start(DnsPolicy{allow:vec![],block:vec!["*".into()],upstream:Some(upstream_addr)}).unwrap();
    assert_eq!(first,second); assert_eq!(query(second,"allowed.test"),0);
    println!("singleton: second deny-all policy uses first port and forwards allowed.test, rcode=0");
    let block_only=DnsPolicy{allow:vec![],block:vec!["blocked.test".into()],upstream:None};
    assert!(!block_only.allows("blocked.test"));assert!(block_only.allows("BLOCKED.TEST"));
    println!("block-only case mismatch: blocked.test denied, BLOCKED.TEST allowed=true");
}
'''
with tempfile.TemporaryDirectory(prefix="thegn-backend-audit-") as directory:
    scratch = Path(directory)
    (scratch / "probe.rs").write_text(reuse + "\n" + dns + "\n" + main)
    subprocess.run(["rustc", "--edition=2024", str(scratch / "probe.rs"), "-o", str(scratch / "probe")], check=True)
    subprocess.run([str(scratch / "probe")], check=True)
