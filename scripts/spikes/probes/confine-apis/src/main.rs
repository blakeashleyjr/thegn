//! WS03-P0-01 throwaway probe. Not part of the thegn workspace, never merged.
//!
//! Every irreversible step (Landlock `restrict_self`, seccomp `apply_filter`)
//! runs in a re-exec'd child so the parent keeps running and can report.
//!
//!   confine-apis-spike abi                    host Landlock ABI via the raw syscall
//!   confine-apis-spike landlock <case>        spawn child for a Landlock case
//!   confine-apis-spike seccomp <case>         spawn child for a seccomp case
//!   confine-apis-spike dump <kind>            print the compiled BPF + analysis
//!   confine-apis-spike syscalls               print syscall numbers
//!
//! Landlock cases: fs-v6-hard, fs-v6-best, plus one per enabled cargo feature
//! (udp, resolve_unix, scope_abstract, scope_signal). Seccomp cases/kinds:
//! dword, masked, qword (each tries TIOCSTI plain and with the high 32 bits set).

use std::collections::BTreeMap;
use std::env;
use std::process::{Command, Stdio};

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, RestrictionStatus,
    Ruleset, RulesetAttr, RulesetCreatedAttr,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, apply_filter,
};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const TIOCSTI: u64 = 0x5412;
const TIOCLINUX: u64 = 0x541C;
const TIOCCONS: u64 = 0x541D;
const TIOCSETD: u64 = 0x5423;

fn main() {
    let args: Vec<String> = env::args().collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match a.as_slice() {
        [_, "abi"] => {
            println!("host_landlock_abi={}", host_abi());
            0
        }
        [_, "syscalls"] => {
            syscalls();
            0
        }
        [_, "landlock", case] => spawn_child("child-landlock", case),
        [_, "seccomp", case] => spawn_child("child-seccomp", case),
        [_, "child-landlock", case] => report(landlock_case(case)),
        [_, "child-seccomp", case] => seccomp_child(case),
        [_, "dump", kind] => match dump(kind) {
            Ok(()) => 0,
            Err(e) => {
                println!("error={e:?}");
                3
            }
        },
        _ => {
            eprintln!("usage: see the module comment in src/main.rs");
            2
        }
    };
    std::process::exit(code);
}

fn spawn_child(sub: &str, case: &str) -> i32 {
    let exe = env::current_exe().expect("current_exe");
    let out = Command::new(exe)
        .arg(sub)
        .arg(case)
        .stdin(Stdio::null())
        .output()
        .expect("spawn child"); // output() waits: no zombie
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    println!("child_exit={:?}", out.status.code());
    0
}

/// Raw `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`.
fn host_abi() -> i64 {
    // SAFETY: documented probe form of the syscall: null attr, size 0, flags=VERSION.
    unsafe { libc::syscall(444, std::ptr::null::<u8>(), 0usize, 1u32) as i64 }
}

fn report(r: Res<RestrictionStatus>) -> i32 {
    match r {
        Ok(st) => {
            println!("restriction_status={st:?}");
            0
        }
        Err(e) => {
            println!("error={e}");
            println!("error_debug={e:?}");
            3
        }
    }
}

fn root_rule(abi: ABI) -> Res<PathBeneath<PathFd>> {
    Ok(PathBeneath::new(PathFd::new("/")?, AccessFs::from_all(abi)))
}

fn landlock_case(case: &str) -> Res<RestrictionStatus> {
    let abi = ABI::V6;
    match case {
        "fs-v6-hard" => Ok(Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(abi))?
            .create()?
            .add_rule(root_rule(abi)?)?
            .restrict_self()?),
        "fs-v6-best" => Ok(Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(abi))?
            .create()?
            .add_rule(root_rule(abi)?)?
            .restrict_self()?),
        #[cfg(feature = "resolve_unix")]
        "resolve_unix" => Ok(Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::ResolveUnix)?
            .create()?
            .restrict_self()?),
        #[cfg(feature = "scope_abstract")]
        "scope_abstract" => Ok(Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .scope(landlock::Scope::AbstractUnixSocket)?
            .create()?
            .restrict_self()?),
        #[cfg(feature = "scope_signal")]
        "scope_signal" => Ok(Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .scope(landlock::Scope::Signal)?
            .create()?
            .restrict_self()?),
        // NOTE: the variant names below are a best guess at the crate's UDP rights;
        // a compile error here is the answer "no (or renamed)": read docs.rs for
        // landlock 0.4.7 `AccessNet` and fix the names before concluding "no".
        #[cfg(feature = "udp")]
        "udp" => Ok(Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(landlock::AccessNet::BindUdp | landlock::AccessNet::ConnectUdp)?
            .create()?
            .restrict_self()?),
        other => Err(format!("unknown or disabled landlock case: {other}").into()),
    }
}

fn target_arch() -> Res<seccompiler::TargetArch> {
    Ok(env::consts::ARCH.try_into()?)
}

/// One rule per tty ioctl, each comparing argument 1 (the command) with `len`.
fn program(kind: &str) -> Res<BpfProgram> {
    let mut rules: Vec<SeccompRule> = Vec::new();
    for cmd in [TIOCSTI, TIOCLINUX, TIOCCONS, TIOCSETD] {
        let cond = match kind {
            "dword" => SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, cmd)?,
            "masked" => SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(0xffff_ffff),
                cmd,
            )?,
            "qword" => SeccompCondition::new(1, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, cmd)?,
            other => return Err(format!("unknown kind {other}").into()),
        };
        rules.push(SeccompRule::new(vec![cond])?);
    }
    let mut table = BTreeMap::new();
    table.insert(libc::SYS_ioctl, rules);
    let filter = SeccompFilter::new(
        table,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch()?,
    )?;
    Ok(filter.try_into()?)
}

fn seccomp_child(kind: &str) -> i32 {
    let prog = match program(kind) {
        Ok(p) => p,
        Err(e) => {
            println!("error={e:?}");
            return 3;
        }
    };
    if let Err(e) = apply_filter(&prog) {
        println!("apply_error={e:?}");
        return 3;
    }
    for (label, cmd) in [("plain", TIOCSTI), ("high_bits", TIOCSTI | (1u64 << 32))] {
        // SAFETY: fd 0 is /dev/null in this child; a plain allowed call returns ENOTTY.
        let rc = unsafe { libc::syscall(libc::SYS_ioctl, 0, cmd as libc::c_ulong, 0usize) };
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let verdict = if rc == -1 && errno == libc::EPERM {
            "BLOCKED(EPERM)"
        } else {
            "reached-ioctl(allowed)"
        };
        println!("kind={kind} call={label} cmd={cmd:#x} rc={rc} errno={errno} => {verdict}");
    }
    0
}

/// Print the compiled program and a small analysis: does it check the arch word
/// itself, and which halves of args[1] does it load (offsets 0x18 low, 0x1c high).
fn dump(kind: &str) -> Res<()> {
    let prog = program(kind)?;
    println!("kind={kind} instructions={}", prog.len());
    let (mut arch_check, mut low, mut high) = (false, false, false);
    for (i, ins) in prog.iter().enumerate() {
        println!(
            "{i:03}: code={:#06x} jt={} jf={} k={:#010x}",
            ins.code, ins.jt, ins.jf, ins.k
        );
        // BPF_LD|BPF_W|BPF_ABS = 0x20 loads from seccomp_data at offset k.
        if ins.code == 0x20 {
            match ins.k {
                4 => arch_check = true,
                0x18 => low = true,
                0x1c => high = true,
                _ => {}
            }
        }
    }
    println!("loads_arch_word={arch_check} loads_args1_low={low} loads_args1_high={high}");
    Ok(())
}

fn syscalls() {
    println!("ioctl={}", libc::SYS_ioctl);
    println!("landlock_create_ruleset=444 (kernel UAPI, all archs)");
    #[cfg(feature = "libc_open_tree_attr")]
    println!("libc_SYS_open_tree_attr={}", libc::SYS_open_tree_attr);
    println!("arch={}", env::consts::ARCH);
}
