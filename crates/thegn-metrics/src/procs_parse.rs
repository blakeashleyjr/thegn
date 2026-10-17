//! Zero-allocation parsers for Linux `/proc` process records.

use crate::procs_model::{CgroupLine, Comm, Io, ProcState, SchedStat, Stat, Status};

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn number_u64(bytes: &[u8]) -> Option<u64> {
    let bytes = trim_ascii(bytes);
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0u64;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(*byte - b'0'))?;
    }
    Some(value)
}

fn number_i64(bytes: &[u8]) -> Option<i64> {
    let bytes = trim_ascii(bytes);
    if bytes.is_empty() {
        return None;
    }
    let (negative, digits) = if let Some(rest) = bytes.strip_prefix(b"-") {
        (true, rest)
    } else if let Some(rest) = bytes.strip_prefix(b"+") {
        (false, rest)
    } else {
        (false, bytes)
    };
    let magnitude = number_u64(digits)?;
    if negative && magnitude == (i64::MAX as u64) + 1 {
        Some(i64::MIN)
    } else if negative {
        i64::try_from(magnitude).ok()?.checked_neg()
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// Parse `/proc/<pid>/stat`. Fields 3 through 24 are required; later kernel
/// fields are optional and extra trailing fields are tolerated.
pub fn parse_stat(input: &[u8]) -> Option<Stat> {
    let open = input.iter().position(|byte| *byte == b'(')?;
    let close = input.iter().rposition(|byte| *byte == b')')?;
    if close <= open {
        return None;
    }
    let pid = number_u64(&input[..open])?;
    let pid = u32::try_from(pid).ok()?;
    let mut tokens = input
        .get(close.checked_add(1)?..)?
        .split(u8::is_ascii_whitespace)
        .filter(|t| !t.is_empty());
    let state_byte = *tokens.next()?.first()?;
    let mut ppid = None;
    let mut pgrp = None;
    let mut session = None;
    let mut tty_nr = None;
    let mut tpgid = None;
    let mut flags = None;
    let mut minflt = None;
    let mut majflt = None;
    let mut utime = None;
    let mut stime = None;
    let mut cutime = None;
    let mut cstime = None;
    let mut priority = None;
    let mut nice = None;
    let mut num_threads = None;
    let mut starttime = None;
    let mut vsize = None;
    let mut rss_pages = None;
    let mut processor = None;
    let mut rt_priority = None;
    let mut policy = None;
    let mut delayacct_blkio_ticks = None;
    let mut count = 0usize;
    for token in tokens {
        let field = count + 4;
        if field > 42 {
            break;
        }
        match field {
            4 => ppid = Some(u32::try_from(number_u64(token)?).ok()?),
            5 => pgrp = Some(u32::try_from(number_u64(token)?).ok()?),
            6 => session = Some(u32::try_from(number_u64(token)?).ok()?),
            7 => tty_nr = Some(i32::try_from(number_i64(token)?).ok()?),
            8 => tpgid = Some(i32::try_from(number_i64(token)?).ok()?),
            9 => flags = Some(number_u64(token)?),
            10 => minflt = Some(number_u64(token)?),
            12 => majflt = Some(number_u64(token)?),
            14 => utime = Some(number_u64(token)?),
            15 => stime = Some(number_u64(token)?),
            16 => cutime = Some(number_i64(token)?),
            17 => cstime = Some(number_i64(token)?),
            18 => priority = Some(number_i64(token)?),
            19 => nice = Some(number_i64(token)?),
            20 => num_threads = Some(number_u64(token)?),
            22 => starttime = Some(number_u64(token)?),
            23 => vsize = Some(number_u64(token)?),
            24 => rss_pages = Some(number_i64(token)?),
            39 => processor = number_u64(token).and_then(|value| u32::try_from(value).ok()),
            40 => rt_priority = number_u64(token).and_then(|value| u32::try_from(value).ok()),
            41 => policy = number_u64(token).and_then(|value| u32::try_from(value).ok()),
            42 => delayacct_blkio_ticks = number_u64(token),
            _ => {}
        }
        count += 1;
    }
    if count < 21 {
        return None;
    }
    Some(Stat {
        pid,
        comm: Comm::from_bytes(&input[open + 1..close]),
        state: ProcState::from_byte(state_byte),
        ppid: ppid?,
        pgrp: pgrp?,
        session: session?,
        tty_nr: tty_nr?,
        tpgid: tpgid?,
        flags: flags?,
        minflt: minflt?,
        majflt: majflt?,
        utime: utime?,
        stime: stime?,
        cutime: cutime?,
        cstime: cstime?,
        priority: priority?,
        nice: nice?,
        num_threads: num_threads?,
        starttime: starttime?,
        vsize: vsize?,
        rss_pages: rss_pages?,
        processor,
        rt_priority,
        policy,
        delayacct_blkio_ticks,
    })
}

fn line_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    Some((&line[..colon], trim_ascii(&line[colon + 1..])))
}

fn memory_value(bytes: &[u8]) -> Option<u64> {
    let bytes = trim_ascii(bytes);
    let value = bytes.strip_suffix(b"kB").map(trim_ascii).unwrap_or(bytes);
    number_u64(value)
}

fn four_values(bytes: &[u8]) -> Option<[u32; 4]> {
    let mut tokens = bytes
        .split(u8::is_ascii_whitespace)
        .filter(|t| !t.is_empty());
    let mut values = [0u32; 4];
    for value in &mut values {
        *value = u32::try_from(number_u64(tokens.next()?)?).ok()?;
    }
    if tokens.next().is_some() {
        None
    } else {
        Some(values)
    }
}

/// Parse selected fields from `/proc/<pid>/status`; unknown fields are ignored.
pub fn parse_status(input: &[u8]) -> Option<Status> {
    let mut status = Status::default();
    for line in input.split(|byte| *byte == b'\n') {
        let Some((key, value)) = line_value(line) else {
            continue;
        };
        match key {
            b"Tgid" => status.tgid = number_u64(value).and_then(|v| u32::try_from(v).ok()),
            b"Pid" => status.pid = number_u64(value).and_then(|v| u32::try_from(v).ok()),
            b"PPid" => status.ppid = number_u64(value).and_then(|v| u32::try_from(v).ok()),
            b"Uid" => status.uid = four_values(value),
            b"Gid" => status.gid = four_values(value),
            b"VmRSS" => status.vm_rss_kib = memory_value(value),
            b"RssAnon" => status.rss_anon_kib = memory_value(value),
            b"RssFile" => status.rss_file_kib = memory_value(value),
            b"RssShmem" => status.rss_shmem_kib = memory_value(value),
            b"VmSwap" => status.vm_swap_kib = memory_value(value),
            b"Threads" => status.threads = number_u64(value).and_then(|v| u32::try_from(v).ok()),
            b"voluntary_ctxt_switches" => status.voluntary_ctxt = number_u64(value),
            b"nonvoluntary_ctxt_switches" => status.nonvoluntary_ctxt = number_u64(value),
            _ => {}
        }
    }
    if status.pid.is_some() || status.tgid.is_some() {
        Some(status)
    } else {
        None
    }
}

/// Parse `/proc/<pid>/io`. Missing counters in a valid file default to zero.
pub fn parse_io(input: &[u8]) -> Option<Io> {
    let mut io = Io::default();
    let mut have_read = false;
    let mut have_write = false;
    for line in input.split(|byte| *byte == b'\n') {
        let Some((key, value)) = line_value(line) else {
            continue;
        };
        match key {
            b"rchar" => io.rchar = number_u64(value)?,
            b"wchar" => io.wchar = number_u64(value)?,
            b"syscr" => io.syscr = number_u64(value)?,
            b"syscw" => io.syscw = number_u64(value)?,
            b"read_bytes" => {
                io.read_bytes = number_u64(value)?;
                have_read = true;
            }
            b"write_bytes" => {
                io.write_bytes = number_u64(value)?;
                have_write = true;
            }
            b"cancelled_write_bytes" => io.cancelled_write_bytes = number_u64(value)?,
            _ => {}
        }
    }
    if have_read && have_write {
        Some(io)
    } else {
        None
    }
}

/// Parse the first three `/proc/<pid>/schedstat` counters.
pub fn parse_schedstat(input: &[u8]) -> Option<SchedStat> {
    let mut tokens = input
        .split(u8::is_ascii_whitespace)
        .filter(|t| !t.is_empty());
    Some(SchedStat {
        run_ns: number_u64(tokens.next()?)?,
        wait_ns: number_u64(tokens.next()?)?,
        timeslices: number_u64(tokens.next()?)?,
    })
}

/// Iterate over valid `/proc/<pid>/cgroup` membership lines.
pub fn cgroup_lines(input: &[u8]) -> impl Iterator<Item = CgroupLine<'_>> {
    input.split(|byte| *byte == b'\n').filter_map(|line| {
        let line = trim_ascii(line);
        let first = line.iter().position(|byte| *byte == b':')?;
        let after_first = first.checked_add(1)?;
        let second_rel = line
            .get(after_first..)?
            .iter()
            .position(|byte| *byte == b':')?;
        let second = after_first.checked_add(second_rel)?;
        let hier_id = u32::try_from(number_u64(&line[..first])?).ok()?;
        let controllers = &line[after_first..second];
        let path = trim_ascii(line.get(second.checked_add(1)?..)?);
        Some(CgroupLine {
            hier_id,
            controllers,
            path,
        })
    })
}

/// Return the unified cgroup path, falling back to the v1 systemd controller.
pub fn unified_cgroup_path(input: &[u8]) -> Option<&[u8]> {
    let mut systemd_path = None;
    for line in cgroup_lines(input) {
        if line.hier_id == 0 && line.controllers.is_empty() {
            return Some(line.path);
        }
        if line
            .controllers
            .split(|byte| *byte == b',')
            .any(|part| part == b"name=systemd")
        {
            systemd_path = Some(line.path);
        }
    }
    systemd_path
}

/// Convert process start ticks to Unix seconds using the host's boot time.
pub fn start_unix_secs(start_ticks: u64, clk_tck: u64, btime: u64) -> Option<u64> {
    if clk_tck == 0 {
        return None;
    }
    btime.checked_add(start_ticks / clk_tck)
}

#[cfg(test)]
mod tests {
    use super::{
        cgroup_lines, parse_io, parse_schedstat, parse_stat, parse_status, start_unix_secs,
        unified_cgroup_path,
    };
    use crate::procs_model::ProcState;

    const STAT_SIMPLE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_simple"
    ));
    const STAT_SPACES: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_comm_spaces"
    ));
    const STAT_PARENS: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_comm_parens"
    ));
    const STAT_PARENS_ALT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_comm_parens_alt"
    ));
    const STAT_KTHREAD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_kthread"
    ));
    const STAT_KWORKER: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_kworker_long"
    ));
    const STAT_ZOMBIE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_zombie"
    ));
    const STAT_OLD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/stat_old_kernel"
    ));
    const STATUS_FULL: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/status_full"
    ));
    const STATUS_KTHREAD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/status_kthread"
    ));
    const IO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/io"
    ));
    const SCHEDSTAT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/schedstat"
    ));
    const CGROUP_THEGN: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/cgroup_thegn_slice"
    ));
    const CGROUP_LIBPOD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/cgroup_libpod"
    ));
    const CGROUP_DOCKER: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/cgroup_docker"
    ));
    const CGROUP_MIXED: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/cgroup_v1_mixed"
    ));
    const CGROUP_V1: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/proc/cgroup_v1_only"
    ));

    #[test]
    fn stat_handles_comm_delimiters_flags_and_old_kernels() {
        let spaces = parse_stat(STAT_SPACES).unwrap();
        assert_eq!(spaces.comm.as_bytes(), b"tmux: server");
        assert_eq!(spaces.ppid, 1);
        assert_eq!(spaces.starttime, 424242);
        let parens = parse_stat(STAT_PARENS).unwrap();
        assert_eq!(parens.comm.as_bytes(), b"(sd-pam)");
        assert_eq!(parens.ppid, 1);
        assert_eq!(parens.starttime, 424242);
        let parens_alt = parse_stat(STAT_PARENS_ALT).unwrap();
        assert_eq!(parens_alt.comm.as_bytes(), b"a) b) c");
        assert_eq!(parens_alt.ppid, 1);
        assert_eq!(parens_alt.starttime, 424242);
        assert_eq!(
            parse_stat(STAT_KWORKER).unwrap().comm.as_bytes(),
            b"kworker/u64:2-events_unbound"
        );
        let kthread = parse_stat(STAT_KTHREAD).unwrap();
        assert!(kthread.is_kthread());
        assert!(!parse_stat(STAT_SIMPLE).unwrap().is_kthread());
        let zombie = parse_stat(STAT_ZOMBIE).unwrap();
        assert!(zombie.is_zombie());
        assert_eq!(zombie.state, ProcState::Zombie);
        assert_eq!(zombie.vsize, 0);
        assert_eq!(zombie.rss_pages, 0);
        let old = parse_stat(STAT_OLD).unwrap();
        assert_eq!(old.starttime, 424242);
        assert_eq!(old.processor, None);
        assert_eq!(old.policy, None);
        let simple = parse_stat(STAT_SIMPLE).unwrap();
        assert_eq!(simple.processor, Some(2));
        assert_eq!(simple.rt_priority, Some(0));
        assert_eq!(simple.policy, Some(0));
        assert_eq!(simple.delayacct_blkio_ticks, Some(0));
        let missing_f24 = STAT_OLD.strip_suffix(b"12\n").unwrap().to_vec();
        assert_eq!(parse_stat(&missing_f24), None);
        let mut extended = STAT_SIMPLE.strip_suffix(b"\n").unwrap().to_vec();
        extended.extend_from_slice(b" 99 100 future_field\n");
        assert_eq!(parse_stat(&extended).unwrap().starttime, 424242);
        assert_eq!(
            crate::procs_model::Comm::from_bytes(&[b'x'; 80])
                .as_bytes()
                .len(),
            64
        );
    }

    #[test]
    fn stat_rejects_each_prefix_before_required_fields_finish() {
        let f24_start = STAT_SIMPLE
            .windows(4)
            .position(|window| window == b" 12 ")
            .unwrap();
        for end in 0..=f24_start {
            assert_eq!(
                parse_stat(&STAT_SIMPLE[..end]),
                None,
                "prefix ended at {end}"
            );
        }
        assert!(parse_stat(&STAT_SIMPLE[..f24_start + 2]).is_some());
    }

    #[test]
    fn stat_ignores_unstored_fields_and_accepts_signed_exit_signal() {
        let mut fields: Vec<Vec<u8>> = STAT_SIMPLE
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .map(|field| field.to_vec())
            .collect();
        fields[10] = b"not-a-number".to_vec();
        fields[37] = b"-1".to_vec();
        let mut input = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                input.push(b' ');
            }
            input.extend_from_slice(field);
        }
        let stat = parse_stat(&input).unwrap();
        assert_eq!(stat.starttime, 424242);
        assert_eq!(stat.processor, Some(2));

        let mut invalid_processor_fields: Vec<Vec<u8>> = STAT_SIMPLE
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .map(|field| field.to_vec())
            .collect();
        invalid_processor_fields[38] = b"invalid".to_vec();
        let mut invalid_processor = Vec::new();
        for (index, field) in invalid_processor_fields.iter().enumerate() {
            if index > 0 {
                invalid_processor.push(b' ');
            }
            invalid_processor.extend_from_slice(field);
        }
        assert_eq!(parse_stat(&invalid_processor).unwrap().processor, None);
    }

    #[test]
    fn status_malformed_numeric_fields_remain_optional() {
        let status =
            parse_status(b"Tgid: invalid\nPid: 1234\nPPid: invalid\nThreads: invalid\n").unwrap();
        assert_eq!(status.tgid, None);
        assert_eq!(status.pid, Some(1234));
        assert_eq!(status.ppid, None);
        assert_eq!(status.threads, None);
    }

    #[test]
    fn status_io_and_schedstat_preserve_optional_values() {
        let status = parse_status(STATUS_FULL).unwrap();
        assert_eq!(status.uid, Some([1000, 1001, 1002, 1003]));
        assert_eq!(status.gid, Some([4, 5, 6, 7]));
        assert_eq!(status.vm_swap_kib, Some(0));
        assert_eq!(status.vm_rss_kib, Some(1024));
        assert_eq!(parse_status(STATUS_KTHREAD).unwrap().vm_swap_kib, None);
        let io = parse_io(IO).unwrap();
        assert_eq!((io.rchar, io.wchar, io.syscr, io.syscw), (100, 200, 3, 4));
        assert_eq!(
            (io.read_bytes, io.write_bytes, io.cancelled_write_bytes),
            (500, 600, 7)
        );
        assert_eq!(
            parse_schedstat(SCHEDSTAT).unwrap(),
            crate::procs_model::SchedStat {
                run_ns: 1_000_000,
                wait_ns: 2_000_000,
                timeslices: 3
            }
        );
        assert_eq!(parse_schedstat(b"1 2\n"), None);
    }

    #[test]
    fn cgroup_membership_prefers_unified_and_preserves_colons() {
        assert_eq!(
            unified_cgroup_path(CGROUP_THEGN),
            Some(&b"/user.slice/user-1000.slice/thegn.slice/run.scope"[..])
        );
        assert_eq!(
            unified_cgroup_path(CGROUP_LIBPOD),
            Some(&b"/machine.slice/libpod-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.scope/container"[..])
        );
        assert_eq!(
            unified_cgroup_path(CGROUP_DOCKER),
            Some(&b"/system.slice/docker-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.scope"[..])
        );
        assert_eq!(unified_cgroup_path(CGROUP_MIXED), Some(&b"/user.slice/user-1000.slice/user@1000.service/thegn.slice/run-p15278-i4595.scope"[..]));
        assert_eq!(
            unified_cgroup_path(CGROUP_V1),
            Some(&b"/user.slice/user-1000.slice/session-1.scope"[..])
        );
        let lines: Vec<_> = cgroup_lines(b"garbage\n1:cpu:/has:a:colon\n").collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].path, b"/has:a:colon");
    }

    #[test]
    fn start_time_and_state_bytes_round_trip() {
        assert_eq!(start_unix_secs(250, 100, 10_000), Some(10_002));
        assert_eq!(start_unix_secs(1, 0, 10), None);
        assert_eq!(start_unix_secs(u64::MAX, 1, 1), None);
        for byte in b"RSDZ TtXxKWP I"
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace())
        {
            assert_eq!(ProcState::from_byte(byte).as_byte(), byte);
        }
        assert_eq!(ProcState::from_byte(0xff).as_byte(), 0xff);
    }

    #[test]
    fn seeded_arbitrary_bytes_and_fixture_mutations_never_panic() {
        let mut seed = 0x837a_42f0_d15c_a11eu64;
        let alphabet = b"() 0123456789\n:\t-abcXYZ";
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed as usize) % 513;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                bytes.push(alphabet[(seed as usize) % alphabet.len()]);
            }
            exercise_all(&bytes);
        }
        let fixtures: [&[u8]; 17] = [
            STAT_SIMPLE,
            STAT_SPACES,
            STAT_PARENS,
            STAT_PARENS_ALT,
            STAT_KTHREAD,
            STAT_KWORKER,
            STAT_ZOMBIE,
            STAT_OLD,
            STATUS_FULL,
            STATUS_KTHREAD,
            IO,
            SCHEDSTAT,
            CGROUP_THEGN,
            CGROUP_LIBPOD,
            CGROUP_DOCKER,
            CGROUP_MIXED,
            CGROUP_V1,
        ];
        for fixture in fixtures {
            for end in 0..=fixture.len() {
                exercise_all(&fixture[..end]);
            }
            for index in 0..fixture.len() {
                let mut changed = fixture.to_vec();
                changed[index] ^= 0x5a;
                exercise_all(&changed);
            }
        }
    }

    fn exercise_all(bytes: &[u8]) {
        let _stat = parse_stat(bytes);
        let _status = parse_status(bytes);
        let _io = parse_io(bytes);
        let _schedstat = parse_schedstat(bytes);
        let _lines: Vec<_> = cgroup_lines(bytes).collect();
        let _unified = unified_cgroup_path(bytes);
        let _time = start_unix_secs(42, 100, 1_000);
    }
}
