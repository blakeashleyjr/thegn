//! `model_credentials = "upload"`: copy the host's claude/codex login files
//! into the sandbox's tmpfs (after narrowing egress), and remove them again.
//!
//! The order is the contract (THE-2233):
//!
//! 1. the sandbox must already be running (`Wake::Never`; the bridge resumed
//!    it), so this never buys compute by itself;
//! 2. the narrowed sandbox-level egress policy is applied, and a failure
//!    returns before any upload;
//! 3. `/dev/shm` must be a tmpfs (fail closed, never a disk fallback);
//! 4. a fresh `/dev/shm/thegn-login-<16 hex>` dir receives one gzipped ustar
//!    of `<agent>/<file>` entries at mode 0600;
//! 5. the modes are forced with `chmod` and read back with `stat`; any
//!    mismatch removes the dir and refuses.
//!
//! The payload bytes and the plain tar buffer are zeroized on drop. The
//! gzipped body handed to the HTTP client is not (the client owns it).

use std::collections::BTreeSet;
use std::fmt;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use futures_util::future::BoxFuture;
use thegn_core::vercel::creds;
use thegn_core::vercel::ustar::{TarEntry, ustar};

use super::{STOPPED_TEXT, VercelProvider, Wake};
use crate::provider::ProviderEgress as _;

/// The prefix every login dir (and every login env value) starts with.
pub(crate) const LOGIN_DIR_PREFIX: &str = "/dev/shm/thegn-login-";

/// The ustar mode of every uploaded login file.
const FILE_MODE: u32 = 0o600;

/// The fixed chmod + stat script. `$1` is how many of the following paths are
/// directories (chmod 700); the rest are files (chmod 600). Paths are argv,
/// never interpolated. Prints `<octal mode> <path>` per path.
const CHMOD_STAT_SCRIPT: &str = r#"n=$1; shift; i=0
for p in "$@"; do
  i=$((i+1))
  if [ "$i" -le "$n" ]; then chmod 700 -- "$p" || exit 1; else chmod 600 -- "$p" || exit 1; fi
done
stat -c '%a %n' -- "$@"
"#;

/// Whether `(key, value)` is one of the two login env exceptions to the
/// sandbox env allowlist: a [`creds::LOGIN_ENV_KEYS`] key whose value is a
/// path under [`LOGIN_DIR_PREFIX`] with no `..` and no NUL.
pub(crate) fn login_env_allowed(key: &str, value: &str) -> bool {
    creds::LOGIN_ENV_KEYS.contains(&key)
        && value.starts_with(LOGIN_DIR_PREFIX)
        && !value.contains("..")
        && !value.contains('\0')
}

/// One agent's login files, held in zeroizing buffers. Redacted `Debug`.
pub struct LoginPayload {
    pub(crate) agent: String,
    pub(crate) files: Vec<(String, zeroize::Zeroizing<Vec<u8>>)>,
}

impl LoginPayload {
    /// Wrap `files` (`(vm_name, bytes)`) for `agent`.
    pub fn new(agent: &str, files: Vec<(String, Vec<u8>)>) -> Result<Self> {
        let files: Vec<(String, zeroize::Zeroizing<Vec<u8>>)> = files
            .into_iter()
            .map(|(name, bytes)| (name, zeroize::Zeroizing::new(bytes)))
            .collect();
        for (name, _) in &files {
            ensure!(
                super::files::valid_remote_component(name),
                "vercel: login file name is not a single path component"
            );
        }
        Ok(LoginPayload {
            agent: agent.to_string(),
            files,
        })
    }

    /// The agent this payload logs in.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// The file names (never the bytes).
    pub fn file_names(&self) -> Vec<&str> {
        self.files.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// The byte length of file `name`, if present (never the bytes).
    pub fn file_len(&self, name: &str) -> Option<usize> {
        self.files
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, data)| data.len())
    }
}

impl fmt::Debug for LoginPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.files.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("LoginPayload")
            .field("agent", &self.agent)
            .field("files", &names)
            .finish()
    }
}

/// Where an upload landed and the env pairs (only `LOGIN_ENV_KEYS`) that
/// point the agents at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginHandle {
    pub dir: String,
    pub env: Vec<(String, String)>,
}

/// The sandbox calls an upload makes. [`VercelProvider`] is the production
/// implementation; unit tests substitute a recording fake.
trait LoginOps: Send + Sync {
    /// Ok only when the sandbox is running now (never resumes).
    fn running<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Apply the sandbox-level allow-list egress policy.
    fn apply_policy<'a>(&'a self, name: &'a str, allow: &'a [String]) -> BoxFuture<'a, Result<()>>;
    /// A one-shot command with no env (never resumes).
    fn exec<'a>(&'a self, name: &'a str, argv: Vec<String>)
    -> BoxFuture<'a, Result<(i32, String)>>;
    fn mkdir<'a>(&'a self, name: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn write_targz<'a>(
        &'a self,
        name: &'a str,
        cwd: &'a str,
        targz: Vec<u8>,
    ) -> BoxFuture<'a, Result<()>>;
}

impl LoginOps for VercelProvider {
    fn running<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.session_for(name, Wake::Never).await.map(drop) })
    }

    fn apply_policy<'a>(&'a self, name: &'a str, allow: &'a [String]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.set_network_policy(name, allow, &[]).await })
    }

    fn exec<'a>(
        &'a self,
        name: &'a str,
        argv: Vec<String>,
    ) -> BoxFuture<'a, Result<(i32, String)>> {
        Box::pin(async move { self.run_exec(name, &argv, None, &[], Wake::Never).await })
    }

    fn mkdir<'a>(&'a self, name: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let session = self.session_for(name, Wake::Never).await?;
            self.client.fs_mkdir(&session, path).await
        })
    }

    fn write_targz<'a>(
        &'a self,
        name: &'a str,
        cwd: &'a str,
        targz: Vec<u8>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let session = self.session_for(name, Wake::Never).await?;
            self.client.fs_write_targz(&session, cwd, targz).await
        })
    }
}

/// A fresh 16-lowercase-hex nonce from the OS CSPRNG.
fn random_nonce() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow!("vercel: no randomness for the login dir: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The subdir of `dir` that [`creds::agent_home_env`] points `agent` at.
fn agent_subdir(agent: &str, dir: &str) -> Result<String> {
    let (_, home) = creds::agent_home_env(agent, dir)
        .ok_or_else(|| anyhow!("vercel: model_credentials = upload supports claude and codex"))?;
    let sub = home
        .strip_prefix(dir)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|sub| super::files::valid_remote_component(sub))
        .ok_or_else(|| anyhow!("vercel: the login home for {agent} is not a child of the dir"))?;
    Ok(sub.to_string())
}

/// The planned upload: `(subdir, payload)` per agent, refusing an unsupported
/// agent, an empty payload, or two agents sharing one subdir.
fn upload_plan<'p>(
    payloads: &'p [LoginPayload],
    dir: &str,
) -> Result<Vec<(String, &'p LoginPayload)>> {
    ensure!(!payloads.is_empty(), "vercel: no login files to upload");
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for payload in payloads {
        ensure!(
            !payload.files.is_empty(),
            "vercel: the {} login has no files",
            payload.agent
        );
        let sub = agent_subdir(&payload.agent, dir)?;
        ensure!(
            seen.insert(sub.clone()),
            "vercel: two agents share the {sub} login dir"
        );
        out.push((sub, payload));
    }
    Ok(out)
}

/// The zeroizing plain tar of every planned `<subdir>/<file>` at mode 0600.
fn login_tar(plan: &[(String, &LoginPayload)]) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    let paths: Vec<String> = plan
        .iter()
        .flat_map(|(sub, payload)| {
            payload
                .files
                .iter()
                .map(move |(name, _)| format!("{sub}/{name}"))
        })
        .collect();
    let datas = plan
        .iter()
        .flat_map(|(_, payload)| payload.files.iter().map(|(_, data)| data.as_slice()));
    let entries: Vec<TarEntry<'_>> = paths
        .iter()
        .zip(datas)
        .map(|(path, data)| TarEntry {
            path,
            data,
            mode: FILE_MODE,
        })
        .collect();
    ustar(&entries)
        .map(zeroize::Zeroizing::new)
        .map_err(|e| anyhow!("vercel: build login archive: {e}"))
}

/// `(dirs, files)` the chmod/stat step covers, as absolute paths.
fn expected_paths(dir: &str, plan: &[(String, &LoginPayload)]) -> (Vec<String>, Vec<String>) {
    let mut dirs = vec![dir.to_string()];
    let mut files = Vec::new();
    for (sub, payload) in plan {
        dirs.push(format!("{dir}/{sub}"));
        for (name, _) in &payload.files {
            files.push(format!("{dir}/{sub}/{name}"));
        }
    }
    (dirs, files)
}

/// Check `stat -c '%a %n'` output: every dir is `700`, every file `600`, and
/// every expected path is reported exactly once.
fn verify_modes(out: &str, dirs: &[String], files: &[String]) -> Result<()> {
    let mut reported = std::collections::BTreeMap::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let (mode, path) = line
            .split_once(' ')
            .ok_or_else(|| anyhow!("vercel: malformed login mode readback"))?;
        ensure!(
            reported
                .insert(path.to_string(), mode.to_string())
                .is_none(),
            "vercel: login mode readback repeated a path"
        );
    }
    let want = dirs
        .iter()
        .map(|d| (d, "700"))
        .chain(files.iter().map(|f| (f, "600")));
    let mut count = 0usize;
    for (path, mode) in want {
        count += 1;
        match reported.get(path.as_str()) {
            Some(got) if got == mode => {}
            Some(_) => bail!("vercel: a login path has the wrong mode"),
            None => bail!("vercel: a login path is missing from the mode readback"),
        }
    }
    ensure!(
        reported.len() == count,
        "vercel: login mode readback has unexpected paths"
    );
    Ok(())
}

/// `rm -rf -- <dir>`; best-effort (the dir is tmpfs and dies with the session).
async fn discard(ops: &dyn LoginOps, name: &str, dir: &str) {
    let argv = ["rm", "-rf", "--", dir].map(String::from).to_vec();
    match ops.exec(name, argv).await {
        Ok((0, _)) => {}
        // best-effort: the dir lives in tmpfs, so a session stop destroys it anyway.
        Ok((code, _)) => {
            tracing::warn!(sandbox = %name, code, "vercel: removing a refused login dir failed");
        }
        Err(e) => {
            tracing::warn!(sandbox = %name, error = %e, "vercel: removing a refused login dir failed");
        }
    }
}

async fn ensure_login_with(
    ops: &dyn LoginOps,
    name: &str,
    payloads: &[LoginPayload],
    egress_user_domains: &[String],
    install_agents: bool,
    nonce: String,
) -> Result<LoginHandle> {
    let dir = creds::login_dir(&nonce).map_err(|e| anyhow!("vercel: {}: {}", e.field, e.reason))?;
    let plan = upload_plan(payloads, &dir)?;

    // 1. Running already (the bridge resumed it); never resume here.
    ops.running(name)
        .await
        .context("vercel: the sandbox must be running to receive a login")?;

    // 2. Narrow egress before the first credential byte leaves the host.
    let agents: Vec<String> = payloads.iter().map(|p| p.agent.clone()).collect();
    let allow = creds::narrowed_egress(&agents, egress_user_domains, install_agents);
    ops.apply_policy(name, &allow)
        .await
        .context("vercel: narrowing egress before the login upload failed; nothing uploaded")?;

    // 3. tmpfs or nothing.
    let argv = ["stat", "-f", "-c", "%T", creds::LOGIN_TMPFS_ROOT]
        .map(String::from)
        .to_vec();
    let (code, out) = ops.exec(name, argv).await?;
    if code != 0 || out.trim() != "tmpfs" {
        bail!("vercel: /dev/shm is not a tmpfs; refusing to write a login to disk");
    }

    // 4. The fresh dir.
    ops.mkdir(name, &dir).await?;

    // 5. One gzipped ustar; the plain tar is zeroized when it drops.
    let targz = {
        let tar = login_tar(&plan)?;
        super::files::gzip(&tar)?
    };
    if let Err(e) = ops.write_targz(name, &dir, targz).await {
        discard(ops, name, &dir).await;
        return Err(e.context("vercel: uploading the login failed"));
    }

    // 6. Force and verify the modes.
    let (dirs, files) = expected_paths(&dir, &plan);
    let mut argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        CHMOD_STAT_SCRIPT.to_string(),
        "sh".to_string(),
        dirs.len().to_string(),
    ];
    argv.extend(dirs.iter().cloned());
    argv.extend(files.iter().cloned());
    let checked = match ops.exec(name, argv).await {
        Ok((0, out)) => verify_modes(&out, &dirs, &files),
        Ok((code, _)) => Err(anyhow!("vercel: chmod/stat of the login exited {code}")),
        Err(e) => Err(e),
    };
    if let Err(e) = checked {
        discard(ops, name, &dir).await;
        return Err(e.context("vercel: the login dir modes could not be verified; removed it"));
    }

    // 7. The env pointing the agents at it.
    let mut env: Vec<(String, String)> = Vec::new();
    for payload in payloads {
        if let Some((key, value)) = creds::agent_home_env(&payload.agent, &dir)
            && !env.iter().any(|(k, _)| k == key)
        {
            env.push((key.to_string(), value));
        }
    }
    Ok(LoginHandle { dir, env })
}

/// Whether `dir` has exactly the [`creds::login_dir`] shape.
fn is_login_dir(dir: &str) -> bool {
    dir.strip_prefix(LOGIN_DIR_PREFIX)
        .and_then(|nonce| creds::login_dir(nonce).ok())
        .is_some_and(|shaped| shaped == dir)
}

/// A stopped or missing sandbox: its tmpfs is already gone.
fn already_gone(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains(STOPPED_TEXT) || text.contains("not found") || text.contains("404")
}

async fn remove_login_with(ops: &dyn LoginOps, name: &str, handle: &LoginHandle) -> Result<()> {
    ensure!(
        is_login_dir(&handle.dir),
        "vercel: refusing to remove a path that is not a thegn login dir"
    );
    let argv = ["rm", "-rf", "--", handle.dir.as_str()]
        .map(String::from)
        .to_vec();
    match ops.exec(name, argv).await {
        Ok((0, _)) => Ok(()),
        Ok((code, _)) => bail!("vercel: removing the login dir exited {code}"),
        Err(e) if already_gone(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

impl VercelProvider {
    /// Narrow egress, then upload `payloads` into a fresh tmpfs login dir.
    /// Never resumes: a stopped sandbox is an error.
    pub async fn ensure_login(
        &self,
        name: &str,
        payloads: &[LoginPayload],
        egress_user_domains: &[String],
        install_agents: bool,
    ) -> Result<LoginHandle> {
        let nonce = random_nonce()?;
        ensure_login_with(
            self,
            name,
            payloads,
            egress_user_domains,
            install_agents,
            nonce,
        )
        .await
    }

    /// Remove a login dir this process uploaded (best-effort by the caller).
    /// A stopped or missing sandbox is success (tmpfs died with it).
    pub async fn remove_login(&self, name: &str, handle: &LoginHandle) -> Result<()> {
        remove_login_with(self, name, handle).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Read as _;
    use std::sync::Mutex;

    use super::*;

    const NONCE: &str = "0123456789abcdef";
    const DIR: &str = "/dev/shm/thegn-login-0123456789abcdef";

    /// Records every call as `"<op> <detail>"`; exec answers are scripted.
    #[derive(Default)]
    struct FakeOps {
        calls: Mutex<Vec<String>>,
        execs: Mutex<VecDeque<Result<(i32, String), String>>>,
        policy_fails: bool,
        stopped: bool,
        written: Mutex<Vec<Vec<u8>>>,
    }

    impl FakeOps {
        fn with_execs(execs: Vec<Result<(i32, String), String>>) -> Self {
            FakeOps {
                execs: Mutex::new(execs.into()),
                ..FakeOps::default()
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn ops(&self) -> Vec<String> {
            self.calls()
                .iter()
                .map(|c| c.split(' ').next().unwrap_or("").to_string())
                .collect()
        }
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl LoginOps for FakeOps {
        fn running<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.record("running".into());
                if self.stopped {
                    bail!(STOPPED_TEXT);
                }
                Ok(())
            })
        }
        fn apply_policy<'a>(
            &'a self,
            _name: &'a str,
            allow: &'a [String],
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.record(format!("policy {}", allow.join(",")));
                if self.policy_fails {
                    bail!("fake: policy refused (400)");
                }
                Ok(())
            })
        }
        fn exec<'a>(
            &'a self,
            _name: &'a str,
            argv: Vec<String>,
        ) -> BoxFuture<'a, Result<(i32, String)>> {
            Box::pin(async move {
                let head = if argv.first().map(String::as_str) == Some("/bin/sh") {
                    "chmod".to_string()
                } else {
                    argv.first().cloned().unwrap_or_default()
                };
                self.record(format!("exec {head} {}", argv.join(" ")));
                if self.stopped {
                    bail!(STOPPED_TEXT);
                }
                match self.execs.lock().unwrap().pop_front() {
                    Some(Ok(answer)) => Ok(answer),
                    Some(Err(text)) => Err(anyhow!(text)),
                    None => Ok((0, String::new())),
                }
            })
        }
        fn mkdir<'a>(&'a self, _name: &'a str, path: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.record(format!("mkdir {path}"));
                Ok(())
            })
        }
        fn write_targz<'a>(
            &'a self,
            _name: &'a str,
            cwd: &'a str,
            targz: Vec<u8>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.record(format!("write {cwd}"));
                self.written.lock().unwrap().push(targz);
                Ok(())
            })
        }
    }

    fn codex_payload() -> LoginPayload {
        LoginPayload::new(
            "codex",
            vec![("auth.json".into(), b"{\"secret\":\"sk-live-xyz\"}".to_vec())],
        )
        .unwrap()
    }

    fn good_stat() -> String {
        format!("700 {DIR}\n700 {DIR}/codex\n600 {DIR}/codex/auth.json\n")
    }

    async fn run(ops: &FakeOps) -> Result<LoginHandle> {
        ensure_login_with(
            ops,
            "tg-a",
            &[codex_payload()],
            &["example.com".into(), "*".into()],
            false,
            NONCE.into(),
        )
        .await
    }

    /// `(path, mode)` per entry of a gzipped ustar.
    fn tar_entries(targz: &[u8]) -> Vec<(String, u32)> {
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(targz)
            .read_to_end(&mut tar)
            .unwrap();
        let mut out = Vec::new();
        let mut at = 0;
        while at + 512 <= tar.len() && tar[at] != 0 {
            let h = &tar[at..at + 512];
            let field = |r: std::ops::Range<usize>| -> String {
                h[r].iter()
                    .take_while(|b| **b != 0 && **b != b' ')
                    .map(|b| *b as char)
                    .collect()
            };
            let name = field(0..100);
            let prefix = field(345..500);
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let mode = u32::from_str_radix(field(100..108).trim(), 8).unwrap();
            let size = usize::from_str_radix(field(124..136).trim(), 8).unwrap();
            out.push((path, mode));
            at += 512 + size.div_ceil(512) * 512;
        }
        out
    }

    #[tokio::test]
    async fn policy_is_applied_before_any_upload() {
        let ops = FakeOps::with_execs(vec![Ok((0, "tmpfs\n".into())), Ok((0, good_stat()))]);
        let handle = run(&ops).await.expect("upload succeeds");
        assert_eq!(
            ops.ops(),
            ["running", "policy", "exec", "mkdir", "write", "exec"],
            "{:?}",
            ops.calls()
        );
        let policy = &ops.calls()[1];
        assert!(policy.contains("api.openai.com"), "{policy}");
        assert!(policy.contains("example.com"), "{policy}");
        assert!(!policy.split([' ', ',']).any(|d| d == "*"), "{policy}");
        assert_eq!(handle.dir, DIR);
        assert_eq!(
            handle.env,
            [("CODEX_HOME".to_string(), format!("{DIR}/codex"))]
        );
    }

    #[tokio::test]
    async fn a_policy_failure_writes_nothing() {
        let ops = FakeOps {
            policy_fails: true,
            ..FakeOps::default()
        };
        let err = run(&ops).await.expect_err("policy failure refuses");
        assert!(format!("{err:#}").contains("nothing uploaded"), "{err:#}");
        assert_eq!(ops.ops(), ["running", "policy"], "{:?}", ops.calls());
        assert!(ops.written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stopped_sandbox_is_an_error_before_the_policy() {
        let ops = FakeOps {
            stopped: true,
            ..FakeOps::default()
        };
        run(&ops).await.expect_err("stopped refuses");
        assert_eq!(ops.ops(), ["running"]);
    }

    #[tokio::test]
    async fn a_non_tmpfs_shm_writes_nothing() {
        let ops = FakeOps::with_execs(vec![Ok((0, "ext2/ext3\n".into()))]);
        let err = run(&ops).await.expect_err("disk refuses");
        assert!(
            err.to_string()
                .contains("/dev/shm is not a tmpfs; refusing to write a login to disk"),
            "{err}"
        );
        assert_eq!(ops.ops(), ["running", "policy", "exec"]);
        assert!(ops.written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_tar_has_0600_entries_with_only_the_expected_names() {
        let ops = FakeOps::with_execs(vec![Ok((0, "tmpfs".into())), Ok((0, good_stat()))]);
        run(&ops).await.unwrap();
        let written = ops.written.lock().unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(
            tar_entries(&written[0]),
            [("codex/auth.json".to_string(), 0o600)]
        );
        assert!(ops.calls().contains(&format!("write {DIR}")));
        assert!(ops.calls().contains(&format!("mkdir {DIR}")));
    }

    #[tokio::test]
    async fn a_mode_mismatch_removes_the_dir_and_refuses() {
        let bad = format!("700 {DIR}\n755 {DIR}/codex\n600 {DIR}/codex/auth.json\n");
        let ops = FakeOps::with_execs(vec![Ok((0, "tmpfs".into())), Ok((0, bad))]);
        let err = run(&ops).await.expect_err("mismatch refuses");
        assert!(format!("{err:#}").contains("removed it"), "{err:#}");
        let last = ops.calls().last().cloned().unwrap();
        assert_eq!(last, format!("exec rm rm -rf -- {DIR}"));
    }

    #[tokio::test]
    async fn a_missing_path_in_the_readback_refuses() {
        let short = format!("700 {DIR}\n700 {DIR}/codex\n");
        let ops = FakeOps::with_execs(vec![Ok((0, "tmpfs".into())), Ok((0, short))]);
        run(&ops).await.expect_err("missing file refuses");
        assert!(ops.calls().last().unwrap().starts_with("exec rm "));
    }

    #[tokio::test]
    async fn an_unsupported_agent_is_refused_before_any_call() {
        let payload = LoginPayload::new("pi", vec![("auth.json".into(), b"x".to_vec())]).unwrap();
        let ops = FakeOps::default();
        let err = ensure_login_with(&ops, "tg-a", &[payload], &[], false, NONCE.into())
            .await
            .expect_err("pi refuses");
        assert!(
            err.to_string().contains("supports claude and codex"),
            "{err}"
        );
        assert!(ops.calls().is_empty());
    }

    #[test]
    fn payload_debug_has_names_but_no_bytes() {
        let shown = format!("{:?}", codex_payload());
        assert!(shown.contains("codex"), "{shown}");
        assert!(shown.contains("auth.json"), "{shown}");
        assert!(!shown.contains("sk-live-xyz"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn payload_refuses_a_path_as_a_file_name() {
        assert!(LoginPayload::new("codex", vec![("../x".into(), vec![])]).is_err());
        assert!(LoginPayload::new("codex", vec![("a/b".into(), vec![])]).is_err());
    }

    #[test]
    fn random_nonce_is_a_valid_login_dir() {
        let nonce = random_nonce().unwrap();
        let dir = creds::login_dir(&nonce).expect("16 lowercase hex");
        assert!(dir.starts_with(LOGIN_DIR_PREFIX));
        assert!(is_login_dir(&dir));
        assert_eq!(
            LOGIN_DIR_PREFIX,
            format!("{}/thegn-login-", creds::LOGIN_TMPFS_ROOT)
        );
    }

    #[tokio::test]
    async fn remove_login_checks_the_shape_and_tolerates_a_stopped_sandbox() {
        let ops = FakeOps::default();
        let handle = LoginHandle {
            dir: DIR.into(),
            env: Vec::new(),
        };
        remove_login_with(&ops, "tg-a", &handle).await.unwrap();
        assert_eq!(ops.calls(), [format!("exec rm rm -rf -- {DIR}")]);

        for bad in [
            "/",
            "/dev/shm",
            "/dev/shm/thegn-login-../x",
            "/root/.claude",
        ] {
            let ops = FakeOps::default();
            let handle = LoginHandle {
                dir: bad.into(),
                env: Vec::new(),
            };
            remove_login_with(&ops, "tg-a", &handle)
                .await
                .expect_err("bad shape refuses");
            assert!(ops.calls().is_empty(), "{bad}");
        }

        let stopped = FakeOps {
            stopped: true,
            ..FakeOps::default()
        };
        remove_login_with(&stopped, "tg-a", &handle)
            .await
            .expect("stopped is ok");
    }

    #[test]
    fn login_env_exception_is_path_shaped_only() {
        assert!(login_env_allowed(
            "CLAUDE_CONFIG_DIR",
            "/dev/shm/thegn-login-0123456789abcdef/claude"
        ));
        assert!(login_env_allowed("CODEX_HOME", &format!("{DIR}/codex")));
        assert!(!login_env_allowed("CLAUDE_CONFIG_DIR", "/root/.claude"));
        assert!(!login_env_allowed(
            "CLAUDE_CONFIG_DIR",
            "/dev/shm/thegn-login-x/../../root"
        ));
        assert!(!login_env_allowed(
            "CODEX_HOME",
            "/dev/shm/thegn-login-a\0b"
        ));
        assert!(!login_env_allowed("HOME", &format!("{DIR}/codex")));
    }
}
