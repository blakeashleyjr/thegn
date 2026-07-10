//! Control-plane domain model: auth scopes, token/pairing-URL formats, and
//! relay-lease timing math.
//!
//! Everything here is **pure** (no I/O, no clock, no crypto) so the security
//! decisions are exhaustively unit-tested: hashing happens in `superzej-svc`
//! (which has the CSPRNG + hasher), the store persists opaque strings
//! ([`crate::store::ControlStore`]), and every time-dependent function takes an
//! injected `now_ms`.

use serde::{Deserialize, Serialize};

use crate::store::LeaseRow;

// --- scopes -----------------------------------------------------------------

/// One capability a control-API token can hold.
///
/// `Git` deliberately does **not** imply `Write` (a phone that can commit must
/// not be able to type into a terminal) and vice versa; both imply `Read`.
/// `Admin` implies everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// List sessions/worktrees/leases, snapshots, the event feed.
    Read,
    /// Send terminal input, open worktrees, drive the preview browser.
    Write,
    /// Stage/commit through the GitBackend seam.
    Git,
    /// Pairing management, daemon shutdown.
    Admin,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Git => "git",
            Scope::Admin => "admin",
        }
    }

    fn bit(self) -> u8 {
        match self {
            Scope::Read => 1,
            Scope::Write => 2,
            Scope::Git => 4,
            Scope::Admin => 8,
        }
    }
}

/// A set of scopes, stored in the DB as csv (`"read,git"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeSet(u8);

impl ScopeSet {
    pub fn empty() -> Self {
        ScopeSet(0)
    }

    pub fn of(scopes: &[Scope]) -> Self {
        let mut s = ScopeSet(0);
        for sc in scopes {
            s.insert(*sc);
        }
        s
    }

    pub fn insert(&mut self, scope: Scope) {
        self.0 |= scope.bit();
    }

    pub fn contains(&self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// Parse the csv storage form. Unknown names are ignored (never an escalation:
    /// dropping a name can only *narrow* the grant), so old builds reading a newer
    /// DB degrade safely.
    pub fn parse(csv: &str) -> ScopeSet {
        let mut s = ScopeSet(0);
        for part in csv.split(',') {
            match part.trim() {
                "read" => s.insert(Scope::Read),
                "write" => s.insert(Scope::Write),
                "git" => s.insert(Scope::Git),
                "admin" => s.insert(Scope::Admin),
                _ => {}
            }
        }
        s
    }

    /// The csv storage form, in canonical order.
    pub fn to_csv(&self) -> String {
        let mut out = Vec::new();
        for sc in [Scope::Read, Scope::Write, Scope::Git, Scope::Admin] {
            if self.contains(sc) {
                out.push(sc.as_str());
            }
        }
        out.join(",")
    }

    /// Does this grant satisfy a verb needing `need`? The implication lattice:
    /// `Admin` ⊇ all; `Write` ⊇ `Read`; `Git` ⊇ `Read`; `Git` and `Write` are
    /// mutually independent.
    pub fn allows(&self, need: Scope) -> bool {
        if self.contains(Scope::Admin) {
            return true;
        }
        match need {
            Scope::Read => {
                self.0 & (Scope::Read.bit() | Scope::Write.bit() | Scope::Git.bit()) != 0
            }
            Scope::Write => self.contains(Scope::Write),
            Scope::Git => self.contains(Scope::Git),
            Scope::Admin => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

/// Every control-API verb, for the verb→scope table. Adapters (HTTP handlers,
/// gRPC methods, CLI) MUST route their scope checks through [`required_scope`]
/// so the policy lives in exactly one tested place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    ListSessions,
    ListWorktrees,
    OpenSession,
    Attach,
    Detach,
    SendInput,
    Resize,
    Snapshot,
    KillSession,
    OpenWorktree,
    DriveBrowser,
    GitStatus,
    GitStage,
    GitCommit,
    MergeList,
    MergeAdd,
    MergeClear,
    Events,
    LeaseStatus,
    Me,
    IssuePairing,
    ListPairings,
    RevokePairing,
    ApprovePairing,
    Shutdown,
}

/// The single verb→scope policy table.
pub fn required_scope(verb: Verb) -> Scope {
    match verb {
        Verb::ListSessions
        | Verb::ListWorktrees
        | Verb::Snapshot
        | Verb::Events
        | Verb::LeaseStatus
        | Verb::GitStatus
        | Verb::MergeList
        | Verb::Me => Scope::Read,
        // Attaching streams pane output (read) but registers a client that
        // holds the session and can resize it — that is a write-side effect.
        Verb::OpenSession
        | Verb::Attach
        | Verb::Detach
        | Verb::SendInput
        | Verb::Resize
        | Verb::KillSession
        | Verb::OpenWorktree
        | Verb::DriveBrowser => Scope::Write,
        Verb::GitStage | Verb::GitCommit | Verb::MergeAdd | Verb::MergeClear => Scope::Git,
        Verb::IssuePairing
        | Verb::ListPairings
        | Verb::RevokePairing
        | Verb::ApprovePairing
        | Verb::Shutdown => Scope::Admin,
    }
}

// --- token formats ----------------------------------------------------------

/// Which credential family a presented string belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `szc1_…` — a long-lived scoped bearer token (minted by redeeming a code).
    Control,
    /// `szp1_…` — a single-use pairing code embedded in a pairing URL.
    PairingCode,
}

impl TokenKind {
    fn prefix(self) -> &'static str {
        match self {
            TokenKind::Control => "szc1",
            TokenKind::PairingCode => "szp1",
        }
    }
}

/// The two halves of a parsed credential: the public lookup `id` (safe to log,
/// the `pairings.pairing_id` key) and the `secret` whose sha-256 the store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenParts {
    pub id: String,
    pub secret: String,
}

/// Expected hex length of the id half (4 random bytes).
pub const TOKEN_ID_HEX: usize = 8;
/// Expected hex length of the secret half (32 random bytes ⇒ 256-bit entropy —
/// why a fast sha-256 hash, not argon2, is the right stored form).
pub const TOKEN_SECRET_HEX: usize = 64;

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Format a credential from its raw random halves (caller generates the bytes).
pub fn format_token(kind: TokenKind, id: &str, secret: &str) -> String {
    format!("{}_{}_{}", kind.prefix(), id, secret)
}

/// Parse a presented credential. Returns `None` for anything malformed —
/// callers treat that identically to a failed hash match (one rejection path).
pub fn parse_token(s: &str) -> Option<(TokenKind, TokenParts)> {
    let mut it = s.splitn(3, '_');
    let (prefix, id, secret) = (it.next()?, it.next()?, it.next()?);
    let kind = match prefix {
        "szc1" => TokenKind::Control,
        "szp1" => TokenKind::PairingCode,
        _ => return None,
    };
    if !is_lower_hex(id, TOKEN_ID_HEX) || !is_lower_hex(secret, TOKEN_SECRET_HEX) {
        return None;
    }
    Some((
        kind,
        TokenParts {
            id: id.to_string(),
            secret: secret.to_string(),
        },
    ))
}

// --- pairing URL ------------------------------------------------------------

/// A pairing URL: everything a thin client needs to redeem a code against a
/// `szhost serve` instance. `fp` is reserved for a TLS certificate fingerprint
/// (v1 serves plaintext behind a trusted network; the slot keeps v2 pinning an
/// additive change, not a format break).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingUrl {
    pub host: String,
    pub port: u16,
    /// The full `szp1_…` pairing code.
    pub code: String,
    pub fp: Option<String>,
}

impl PairingUrl {
    /// The app-scheme form: `superzej://pair?host=H&port=P&t=szp1_…[&fp=…]`.
    pub fn encode(&self) -> String {
        let mut s = format!(
            "superzej://pair?host={}&port={}&t={}",
            self.host, self.port, self.code
        );
        if let Some(fp) = &self.fp {
            s.push_str("&fp=");
            s.push_str(fp);
        }
        s
    }

    /// The web-redeem form: `http://H:P/pair#t=szp1_…`. The code rides in the
    /// fragment so it never appears in server request logs.
    pub fn web_form(&self) -> String {
        format!("http://{}:{}/pair#t={}", self.host, self.port, self.code)
    }

    /// Parse the app-scheme form. Hosts are restricted to URL-safe chars by
    /// construction (hostname / IP / tailnet name); anything else fails parse.
    pub fn parse(s: &str) -> Option<PairingUrl> {
        let rest = s.strip_prefix("superzej://pair?")?;
        let mut host = None;
        let mut port = None;
        let mut code = None;
        let mut fp = None;
        for kv in rest.split('&') {
            let (k, v) = kv.split_once('=')?;
            match k {
                "host" if !v.is_empty() => host = Some(v.to_string()),
                "port" => port = Some(v.parse::<u16>().ok()?),
                "t" => {
                    // Must be a well-formed pairing code, not a control token.
                    let (kind, _) = parse_token(v)?;
                    if kind != TokenKind::PairingCode {
                        return None;
                    }
                    code = Some(v.to_string());
                }
                "fp" if !v.is_empty() => fp = Some(v.to_string()),
                _ => return None,
            }
        }
        Some(PairingUrl {
            host: host?,
            port: port?,
            code: code?,
            fp,
        })
    }
}

// --- relay-lease math -------------------------------------------------------

/// What the daemon's lease supervisor should do right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LeasePlan {
    /// Lease ids whose grace period has ended — reap their PTYs.
    pub reap: Vec<i64>,
    /// When the next un-expired relay lease ends (the supervisor's next wake);
    /// `None` when no timed lease is pending (sleep until notified).
    pub next_wake_at: Option<i64>,
}

/// Pure supervisor decision: given the daemon's leases and now, which relay
/// leases to reap and when to wake next. `attached` leases (no expiry) never
/// reap; only `kind == "relay"` rows with an expiry participate.
pub fn plan_leases(leases: &[LeaseRow], now_ms: i64) -> LeasePlan {
    let mut plan = LeasePlan::default();
    for l in leases {
        if l.kind != "relay" {
            continue;
        }
        let Some(exp) = l.expires_at else { continue };
        if exp <= now_ms {
            plan.reap.push(l.lease_id);
        } else {
            plan.next_wake_at = Some(match plan.next_wake_at {
                Some(cur) => cur.min(exp),
                None => exp,
            });
        }
    }
    plan
}

/// Expiry instant for a fresh relay lease opened at `now_ms` with the
/// configured grace period.
pub fn relay_expiry(now_ms: i64, grace_ms: i64) -> i64 {
    now_ms.saturating_add(grace_ms.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: i64, kind: &str, expires_at: Option<i64>) -> LeaseRow {
        LeaseRow {
            lease_id: id,
            session_id: format!("s{id}"),
            daemon_id: "d".into(),
            client_id: None,
            kind: kind.into(),
            created_at: 0,
            expires_at,
        }
    }

    #[test]
    fn scope_set_parse_csv_round_trip() {
        for csv in ["read", "read,write", "read,git", "read,write,git,admin", ""] {
            assert_eq!(ScopeSet::parse(csv).to_csv(), csv);
        }
        // Whitespace and unknown names are tolerated; unknowns only narrow.
        let s = ScopeSet::parse(" read , FUTURE_SCOPE ,git ");
        assert_eq!(s.to_csv(), "read,git");
        assert!(ScopeSet::parse("bogus").is_empty());
    }

    #[test]
    fn scope_lattice() {
        let read = ScopeSet::of(&[Scope::Read]);
        let write = ScopeSet::of(&[Scope::Write]);
        let git = ScopeSet::of(&[Scope::Git]);
        let admin = ScopeSet::of(&[Scope::Admin]);

        // Read is implied by every non-empty grant.
        for s in [read, write, git, admin] {
            assert!(s.allows(Scope::Read), "{s:?} should allow read");
        }
        assert!(!ScopeSet::empty().allows(Scope::Read));

        // Write and Git are independent silos: a git-scoped phone must not be
        // able to type into a terminal, and a write token can't commit.
        assert!(write.allows(Scope::Write) && !write.allows(Scope::Git));
        assert!(git.allows(Scope::Git) && !git.allows(Scope::Write));

        // Admin implies everything; nothing else implies admin.
        for need in [Scope::Read, Scope::Write, Scope::Git, Scope::Admin] {
            assert!(admin.allows(need));
        }
        for s in [read, write, git] {
            assert!(!s.allows(Scope::Admin));
        }
    }

    #[test]
    fn verb_scope_table_is_exhaustive_and_least_privilege() {
        use Verb::*;
        let read = [
            ListSessions,
            ListWorktrees,
            Snapshot,
            Events,
            LeaseStatus,
            GitStatus,
            MergeList,
            Me,
        ];
        let write = [
            OpenSession,
            Attach,
            Detach,
            SendInput,
            Resize,
            KillSession,
            OpenWorktree,
            DriveBrowser,
        ];
        let git = [GitStage, GitCommit, MergeAdd, MergeClear];
        let admin = [
            IssuePairing,
            ListPairings,
            RevokePairing,
            ApprovePairing,
            Shutdown,
        ];
        for v in read {
            assert_eq!(required_scope(v), Scope::Read, "{v:?}");
        }
        for v in write {
            assert_eq!(required_scope(v), Scope::Write, "{v:?}");
        }
        for v in git {
            assert_eq!(required_scope(v), Scope::Git, "{v:?}");
        }
        for v in admin {
            assert_eq!(required_scope(v), Scope::Admin, "{v:?}");
        }
        // The spec scenario: a read-only view set requires only Read.
        let read_only = ScopeSet::of(&[Scope::Read]);
        for v in read {
            assert!(read_only.allows(required_scope(v)));
        }
        for v in write.iter().chain(&git).chain(&admin) {
            assert!(
                !read_only.allows(required_scope(*v)),
                "{v:?} leaked to read"
            );
        }
    }

    const ID: &str = "0123abcd";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn token_format_parse_round_trip() {
        for kind in [TokenKind::Control, TokenKind::PairingCode] {
            let s = format_token(kind, ID, SECRET);
            let (k, parts) = parse_token(&s).unwrap();
            assert_eq!(k, kind);
            assert_eq!(parts.id, ID);
            assert_eq!(parts.secret, SECRET);
        }
        assert!(format_token(TokenKind::Control, ID, SECRET).starts_with("szc1_"));
        assert!(format_token(TokenKind::PairingCode, ID, SECRET).starts_with("szp1_"));
    }

    #[test]
    fn malformed_tokens_reject() {
        let good = format_token(TokenKind::Control, ID, SECRET);
        assert!(parse_token(&good).is_some());
        for bad in [
            "",
            "szc1",
            "szc1__",
            "notaprefix_0123abcd_deadbeef",
            &format!("szx1_{ID}_{SECRET}"),          // unknown prefix
            &format!("szc1_{ID}"),                   // missing secret
            &format!("szc1_short_{SECRET}"),         // id wrong length
            &format!("szc1_{ID}_{}", &SECRET[..60]), // secret wrong length
            &format!("szc1_{ID}_{}", SECRET.to_uppercase()), // not lower hex
            &format!("szc1_{}_{SECRET}", "0123ABCD"),
            &good[..good.len() - 1],
        ] {
            assert!(parse_token(bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn pairing_url_round_trip() {
        let code = format_token(TokenKind::PairingCode, ID, SECRET);
        for fp in [None, Some("aabbcc".to_string())] {
            let u = PairingUrl {
                host: "studio.tail1234.ts.net".into(),
                port: 5380,
                code: code.clone(),
                fp: fp.clone(),
            };
            let parsed = PairingUrl::parse(&u.encode()).unwrap();
            assert_eq!(parsed, u);
        }
        // The web form carries the code in the fragment (never in access logs).
        let u = PairingUrl {
            host: "10.0.0.5".into(),
            port: 80,
            code: code.clone(),
            fp: None,
        };
        assert_eq!(u.web_form(), format!("http://10.0.0.5:80/pair#t={code}"));
    }

    #[test]
    fn pairing_url_rejects_malformed() {
        let code = format_token(TokenKind::PairingCode, ID, SECRET);
        let control = format_token(TokenKind::Control, ID, SECRET);
        for bad in [
            "".to_string(),
            "https://pair?host=h&port=1&t=x".to_string(),
            format!("superzej://pair?port=1&t={code}"), // no host
            format!("superzej://pair?host=h&t={code}"), // no port
            "superzej://pair?host=h&port=1".to_string(), // no code
            format!("superzej://pair?host=h&port=notaport&t={code}"),
            format!("superzej://pair?host=h&port=1&t={control}"), // control token, not a code
            format!("superzej://pair?host=h&port=1&t={code}&evil=1"), // unknown param
        ] {
            assert!(PairingUrl::parse(&bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn plan_leases_reap_boundary_and_next_wake() {
        let leases = vec![
            lease(1, "attached", None),     // never reaps
            lease(2, "relay", Some(5_000)), // expired at 5_000
            lease(3, "relay", Some(9_000)), // pending
            lease(4, "relay", Some(7_000)), // pending, earlier
            lease(5, "relay", None),        // malformed (no expiry): ignored
        ];
        // Strictly before the boundary nothing reaps.
        let p = plan_leases(&leases, 4_999);
        assert!(p.reap.is_empty());
        assert_eq!(p.next_wake_at, Some(5_000));
        // At the boundary the expired lease reaps; wake = earliest survivor.
        let p = plan_leases(&leases, 5_000);
        assert_eq!(p.reap, vec![2]);
        assert_eq!(p.next_wake_at, Some(7_000));
        // Past everything: all timed leases reap, nothing to wake for.
        let p = plan_leases(&leases, 10_000);
        assert_eq!(p.reap, vec![2, 3, 4]);
        assert_eq!(p.next_wake_at, None);
        // Empty input → idle plan.
        assert_eq!(plan_leases(&[], 0), LeasePlan::default());
    }

    #[test]
    fn relay_expiry_saturates() {
        assert_eq!(relay_expiry(1_000, 60_000), 61_000);
        assert_eq!(relay_expiry(1_000, -5), 1_000); // negative grace clamps
        assert_eq!(relay_expiry(i64::MAX - 1, 100), i64::MAX);
    }
}
