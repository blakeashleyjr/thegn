use super::*;

fn row(name: &str, backend: &str) -> ContainerRecord {
    let json = format!(
        r#"[{{"Id":"abcdef0123456789","Names":["{name}"],"State":"running","Status":"Up"}}]"#
    );
    parse_podman_ps_all(&json, backend).rows.remove(0)
}

#[test]
fn podman_array_fields_and_sentinels() {
    let parsed = parse_podman_ps_all(
        r#"[
      {"Id":"abcdef0123456789","Names":["web","alias"],"Image":"demo","ImageID":"imgid","Command":["run","web"],"State":"running","Status":"Up 4 days","Created":1770000000,"Pid":42,"Restarts":2,"Health":"healthy","Labels":{"com.docker.compose.project":"stack","com.docker.compose.service":"web","com.docker.compose.container-number":"2"},"Ports":[{"host_ip":"127.0.0.1","host_port":8080,"container_port":80,"range":1,"protocol":"tcp"}],"Networks":["bridge"],"Pod":"pod1","Mounts":["/src"]},
      {"Id":"tmp","Names":["thegn-tmp"],"State":"created","Status":"Created","Created":-62135596800,"StartedAt":-62135596800,"ExitedAt":-62135596800,"Pid":0,"Labels":null,"Ports":null,"Command":null,"Pod":""},
      {"Id":"exit","Names":["old"],"State":"exited","Status":"Exited (137)"},
      {"Id":"pause","Names":["paused"],"State":"paused","Status":"Up (Paused)","HealthStatus":"starting"}]"#,
        "podman",
    );
    assert_eq!(parsed.skipped, 0);
    assert_eq!(parsed.rows.len(), 4);
    let r = &parsed.rows[0];
    assert_eq!(r.names, ["web", "alias"]);
    assert_eq!(r.image, "demo");
    assert_eq!(r.status, "Up 4 days");
    assert_eq!(r.pid, Some(42));
    assert_eq!(r.restarts, Some(2));
    assert_eq!(r.created, Some(1770000000));
    assert_eq!(r.health, Some(HealthState::Healthy));
    assert_eq!(r.labels["com.docker.compose.project"], "stack");
    assert_eq!(r.command.as_deref(), Some("run web"));
    assert_eq!(r.image_id.as_deref(), Some("imgid"));
    assert_eq!(r.ports[0].host_port, Some(8080));
    assert_eq!(r.networks, ["bridge"]);
    assert_eq!(r.pod.as_deref(), Some("pod1"));
    assert_eq!(r.mounts, ["/src"]);
    assert_eq!(r.compose_project(), Some("stack"));
    assert_eq!(r.compose_service(), Some("web"));
    assert_eq!(r.compose_container_number(), Some(2));
    assert_eq!(parsed.rows[1].created, None);
    assert_eq!(parsed.rows[1].pid, None);
    assert_eq!(parsed.rows[1].pod, None);
    assert_eq!(parsed.rows[1].started, None);
    assert_eq!(parsed.rows[1].exited_at, None);
    assert_eq!(parsed.rows[2].exit_code, Some(137));
    assert_eq!(parsed.rows[3].state, EngineState::Paused);
    assert_eq!(parsed.rows[3].health, Some(HealthState::Starting));
    assert_eq!(r.short_id(), "abcdef012345");
    assert_eq!(row("short", "podman").short_id(), "abcdef012345");
    let short = parse_podman_ps_all(r#"[{"Id":"abc","Names":["n"]}]"#, "podman");
    assert_eq!(short.rows[0].short_id(), "abc");
}

#[test]
fn docker_ndjson_parses_text_and_counts_bad_rows() {
    let text = concat!(
        r#"{"ID":"0123456789abcdef","Names":"web","Image":"img","Status":"Up 2 hours","CreatedAt":"2026-10-17 12:34:56 +0000 UTC","Labels":"a=b,com.docker.compose.project=demo","Ports":"0.0.0.0:5433->5432/tcp, 80/tcp","Networks":"bridge, private"}"#,
        "\n",
        r#"{"ID":"fresh","Names":"fresh","Status":"Created","Ports":"8000-8001->8000-8001/tcp"}"#,
        "\n",
        r#"{"ID":"old","Names":"old","Status":"Exited (137) 2 days ago"}"#,
        "\n",
        r#"{"ID":"nameless","Names":"","Status":"Created"}"#,
        "\ngarbage\n{}\n",
    );
    let parsed = parse_docker_ps_ndjson(text, "docker");
    assert_eq!(parsed.skipped, 3);
    assert_eq!(parsed.rows.len(), 3);
    assert_eq!(parsed.rows[0].state, EngineState::Running);
    assert_eq!(parsed.rows[0].created, Some(1792240496));
    assert_eq!(parsed.rows[0].labels["a"], "b");
    assert_eq!(parsed.rows[0].networks, ["bridge", "private"]);
    assert_eq!(parsed.rows[1].ports[0].range, 2);
    assert_eq!(parsed.rows[2].state, EngineState::Exited);
    assert_eq!(parsed.rows[2].exit_code, Some(137));
}

#[test]
fn api_and_inspect_shapes_are_structured() {
    let api = parse_docker_api_list(
        r#"[{"Id":"full","Names":["/api"],"Image":"x","Created":123,"State":"running","Status":"Up","Ports":[{"IP":"127.0.0.1","PrivatePort":80,"PublicPort":8080,"Type":"tcp"}],"Labels":{"x":"y"},"NetworkSettings":{"Networks":{"bridge":{},"private":{}}}},{"Id":"old","Names":["/old"],"State":"exited","Status":"Exited (23) 2 days ago"},{"Names":["/missing-id"],"State":"running","Status":"Up"}]"#,
        "docker",
    );
    assert_eq!(api.rows[0].name, "api");
    assert_eq!(api.rows[0].ports[0].container_port, 80);
    assert_eq!(api.rows[0].state, EngineState::Running);
    assert_eq!(api.rows[0].networks, ["bridge", "private"]);
    assert_eq!(api.rows[1].exit_code, Some(23));
    assert_eq!(api.skipped, 1);
    let probe = "é".repeat(61);
    let inspect_json = r#"[
      {"Id":"p","Name":"/pod","OCIRuntime":"crun","RestartCount":3,"HostConfig":{"RestartPolicy":{"Name":"always"}},"State":{"Status":"running","Pid":42,"StartedAt":"2026-10-17T12:34:56.123456789+02:00","FinishedAt":"0001-01-01T00:00:00Z","CgroupPath":"/x"},"NetworkSettings":{"Networks":{"z":{"IPAddress":"10.0.0.1"},"a":{"IPAddress":"10.0.0.9"}}}},
      {"Id":"d","Name":"/docker","HostConfig":{"Runtime":"runsc"},"State":{"Status":"exited","ExitCode":137,"OOMKilled":true,"Health":{"Status":"unhealthy","FailingStreak":4,"Log":[{"Output":"OUTPUT-HERE"}]}}}]"#
        .replace("OUTPUT-HERE", &probe);
    let inspected = parse_inspect_batch(&inspect_json);
    assert_eq!(inspected.rows.len(), 2);
    assert_eq!(inspected.rows[0].oci_runtime.as_deref(), Some("crun"));
    assert_eq!(inspected.rows[0].restart_policy.as_deref(), Some("always"));
    assert_eq!(inspected.rows[0].restart_count, Some(3));
    assert_eq!(inspected.rows[0].finished, None);
    assert_eq!(inspected.rows[0].started, Some(1792233296));
    assert_eq!(inspected.rows[0].ips, ["10.0.0.9", "10.0.0.1"]);
    assert_eq!(inspected.rows[0].cgroup_path.as_deref(), Some("/x"));
    assert_eq!(inspected.rows[1].oci_runtime.as_deref(), Some("runsc"));
    assert!(inspected.rows[1].oom_killed);
    assert_eq!(inspected.rows[1].exit_code, Some(137));
    assert_eq!(inspected.rows[1].health, Some(HealthState::Unhealthy));
    assert_eq!(inspected.rows[1].failing_streak, Some(4));
    assert_eq!(
        inspected.rows[1]
            .last_probe
            .as_ref()
            .map(|s| s.chars().count()),
        Some(60)
    );
    assert_eq!(
        inspected.rows[1].last_probe.as_ref().map(String::len),
        Some(120)
    );
}

#[test]
fn events_state_roles_and_attribution_are_deterministic() {
    let events = parse_events(concat!(
        r#"{"Type":"container","ID":"a","Name":"x","Status":"start","Time":10}"#,
        "\n",
        r#"{"Type":"container","ID":"b","Name":"x","Status":"died","Time":11,"ContainerExitCode":137}"#,
        "\n",
        r#"{"Type":"container","ID":"c","Name":"x","Status":"health_status","HealthStatus":"unhealthy","Time":12}"#,
        "\n",
        r#"{"Type":"container","Action":"die","Actor":{"ID":"d","Attributes":{"name":"x","exitCode":"137"}},"time":13}"#,
        "\n",
        r#"{"Type":"container","Action":"health_status: unhealthy","Actor":{"ID":"e","Attributes":{"name":"x"}},"time":14}"#,
        "\n",
        r#"{"Type":"image","ID":"z","Name":"im","Status":"create","Time":15}"#,
        "\ngarbage\n",
    ));
    assert_eq!(events.skipped, 2);
    assert_eq!(events.rows[0].action, EventAction::Start);
    assert_eq!(events.rows[1].action, EventAction::Die);
    assert_eq!(
        events.rows[2].action,
        EventAction::HealthStatus(Some(HealthState::Unhealthy))
    );
    assert_eq!(events.rows[3].exit_code, Some(137));
    assert_eq!(
        events.rows[4].action,
        EventAction::HealthStatus(Some(HealthState::Unhealthy))
    );
    assert_eq!(
        EngineState::from_status("Up 1h (Paused)"),
        EngineState::Paused
    );
    assert_eq!(
        EngineState::from_status("Removal In Progress"),
        EngineState::Removing
    );
    for (raw, expected) in [
        ("created", EngineState::Created),
        ("configured", EngineState::Created),
        ("initialized", EngineState::Created),
        ("running", EngineState::Running),
        ("paused", EngineState::Paused),
        ("restarting", EngineState::Restarting),
        ("stopping", EngineState::Restarting),
        ("exited", EngineState::Exited),
        ("stopped", EngineState::Exited),
        ("dead", EngineState::Dead),
        ("removing", EngineState::Removing),
        ("unknown-value", EngineState::Unknown),
    ] {
        assert_eq!(EngineState::parse(raw), expected);
        assert_eq!(EngineState::parse(&raw.to_ascii_uppercase()), expected);
    }
    let states = [
        EngineState::Created,
        EngineState::Running,
        EngineState::Paused,
        EngineState::Restarting,
        EngineState::Exited,
        EngineState::Dead,
        EngineState::Removing,
        EngineState::Unknown,
    ];
    let healths = [
        None,
        Some(HealthState::Healthy),
        Some(HealthState::Unhealthy),
        Some(HealthState::Starting),
    ];
    let expected_roles = [
        [crate::theme::StateRole::Stale; 4],
        [
            crate::theme::StateRole::Ok,
            crate::theme::StateRole::Ok,
            crate::theme::StateRole::Crit,
            crate::theme::StateRole::Warn,
        ],
        [crate::theme::StateRole::Stale; 4],
        [crate::theme::StateRole::Warn; 4],
        [crate::theme::StateRole::Stale; 4],
        [crate::theme::StateRole::Crit; 4],
        [crate::theme::StateRole::Stale; 4],
        [crate::theme::StateRole::Unknown; 4],
    ];
    for (state, expected) in states.into_iter().zip(expected_roles) {
        for (health, role) in healths.into_iter().zip(expected) {
            assert_eq!(state_role(state, health), role, "{state:?} with {health:?}");
        }
    }
    for (raw, expected) in [
        ("create", EventAction::Create),
        ("kill", EventAction::Kill),
        ("oom", EventAction::Oom),
        ("restart", EventAction::Restart),
        ("remove", EventAction::Remove),
        ("cleanup", EventAction::Other("cleanup".into())),
    ] {
        assert_eq!(event_action(raw), expected);
    }
    assert!(
        parse_event_line(
            r#"{"Type":"container","ID":"empty","Name":"","Action":"create","time":1}"#
        )
        .is_none()
    );
    assert_eq!(
        parse_events(r#"{"Type":"container","ID":"empty","Name":"","Action":"create","time":1}"#)
            .skipped,
        1
    );
    assert_eq!(EngineState::from_status("Up 2h"), EngineState::Running);
    assert_eq!(
        EngineState::from_status("Exited (1) 2h ago"),
        EngineState::Exited
    );
    assert_eq!(
        EngineState::from_status("Restarting (1) 2h"),
        EngineState::Restarting
    );
    assert_eq!(EngineState::from_status("Dead"), EngineState::Dead);
    assert_eq!(EngineState::from_status("mystery"), EngineState::Unknown);
    assert_eq!(
        state_role(EngineState::Running, Some(HealthState::Unhealthy)),
        crate::theme::StateRole::Crit
    );
    assert_eq!(
        state_role(EngineState::Running, Some(HealthState::Starting)),
        crate::theme::StateRole::Warn
    );
    let base = container_name_for_config_profile("/repo/feat", "default");
    let at = Attribution::new("default", &[("/repo/feat", Some("podman"))], Some("mine"));
    let mut ours = row(&base, "podman");
    at.attribute(&mut ours);
    assert_eq!(ours.ownership, Ownership::Ours);
    assert_eq!(ours.role, Role::Worktree);
    assert_eq!(ours.worktree.as_deref(), Some("/repo/feat"));
    let mut sidecar = row(&vpn_sidecar_name(&base), "podman");
    at.attribute(&mut sidecar);
    assert_eq!(sidecar.ownership, Ownership::Ours);
    assert_eq!(sidecar.role, Role::VpnSidecar);
    let mut agent = row(&agent_container_name(&base), "podman");
    at.attribute(&mut agent);
    assert_eq!(agent.role, Role::Agent);
    let mut wrong_backend = row(&base, "podman-rootful");
    at.attribute(&mut wrong_backend);
    assert_ne!(wrong_backend.ownership, Ownership::Ours);
    let mut tmp = row("thegn-tmp-nix-shell-1", "podman");
    tmp.labels.insert("thegn.owner".into(), "mine".into());
    at.attribute(&mut tmp);
    assert_eq!(tmp.ownership, Ownership::Unattributed);
    let mut foreign = row("my-container", "docker");
    foreign.labels.insert("thegn.owner".into(), "theirs".into());
    at.attribute(&mut foreign);
    assert_eq!(foreign.ownership, Ownership::OtherProfile);
    let mut user_made = row("thegn-foo", "docker");
    at.attribute(&mut user_made);
    assert_eq!(user_made.ownership, Ownership::Foreign);
    let mut forged = row("thegn-foo", "podman");
    forged.labels.insert("thegn.owner".into(), "mine".into());
    at.attribute(&mut forged);
    assert_eq!(forged.ownership, Ownership::Unattributed);
    let mut own_label_foreign_name = row("custom", "podman");
    own_label_foreign_name
        .labels
        .insert("thegn.owner".into(), "mine".into());
    at.attribute(&mut own_label_foreign_name);
    assert_eq!(own_label_foreign_name.ownership, Ownership::Foreign);
    let owned = OwnedContainers::new("default", [("/repo/feat", Some("podman"))]);
    for (name, backend) in [
        (base.clone(), "podman"),
        (vpn_sidecar_name(&base), "podman"),
        (agent_container_name(&base), "podman"),
        (base, "podman-rootful"),
        (String::from("thegn-foo"), "docker"),
    ] {
        let mut candidate = row(&name, backend);
        at.attribute(&mut candidate);
        assert_eq!(
            candidate.ownership == Ownership::Ours,
            owned.owns(&name, backend)
        );
    }
}

#[test]
fn malformed_documents_and_wrong_field_types_never_panic() {
    assert_eq!(parse_podman_ps_all("", "podman").skipped, 1);
    assert_eq!(parse_docker_api_list("{", "docker").skipped, 1);
    assert_eq!(parse_inspect_batch("[]").skipped, 0);
    assert_eq!(parse_inspect_batch(r#"{"Id":"not-an-array"}"#).skipped, 1);
    assert_eq!(parse_inspect_batch(r#"[{"Name":"nameless"}]"#).skipped, 1);
    assert_eq!(
        parse_podman_ps_all(r#"[{"Id":"x","Names":4,"Ports":{},"Labels":[]}]"#, "podman").skipped,
        1
    );
    assert_eq!(parse_docker_ps_ndjson("{}", "docker").skipped, 1);
    assert_eq!(parse_events("").skipped, 0);
}
