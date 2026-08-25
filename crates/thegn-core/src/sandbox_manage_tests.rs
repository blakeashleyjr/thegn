//! Tests for the pure container-management op surface. The load-bearing ones
//! are the ownership invariants (§structural): no destructive argv can be built
//! for a resource thegn does not own, and every owned-listing builder carries
//! its ownership filter.

use super::*;
use crate::sandbox::Backend;

// --- ownership: structural, not reviewed-for --------------------------------

#[test]
fn owned_container_claim_rejects_foreign_names() {
    assert!(OwnedContainer::claim("thegn-myrepo-abc").is_some());
    assert!(OwnedContainer::claim("thegn-agent-1-tgvpn").is_some());
    // Anything not in the thegn- family cannot be claimed — so no control/logs/
    // exec argv can ever be spelled for it.
    for foreign in ["postgres", "redis", "my-thegn-thing", "", "docker-proxy"] {
        assert!(
            OwnedContainer::claim(foreign).is_none(),
            "foreign `{foreign}` must not be claimable"
        );
    }
}

#[test]
fn no_control_argv_exists_for_a_foreign_container() {
    // The type system is the proof: `mgmt_control_argv` takes `&OwnedContainer`,
    // and the only constructor rejects foreign names. This test documents the
    // consequence — a foreign name yields no witness, hence no argv.
    assert!(OwnedContainer::claim("someone-elses-db").is_none());
    let ours = OwnedContainer::claim("thegn-w1").unwrap();
    for op in [
        ControlOp::Stop,
        ControlOp::Start,
        ControlOp::Restart,
        ControlOp::Remove,
    ] {
        let argv = mgmt_control_argv(Backend::Docker, op, &ours).unwrap();
        // The container name is always the last arg, and it is always ours.
        assert_eq!(argv.last().unwrap(), "thegn-w1");
        assert!(is_owned_container(argv.last().unwrap()));
    }
    // Remove force-removes.
    let rm = mgmt_control_argv(Backend::Docker, ControlOp::Remove, &ours).unwrap();
    assert_eq!(rm, vec!["rm", "-f", "thegn-w1"]);
}

#[test]
fn every_owned_volume_listing_carries_the_label_filter() {
    // The destructive path for volumes is list(label-filtered) → rm. If a
    // volume-listing builder ever omitted the filter, a foreign volume could be
    // parsed into an OwnedVolume and removed. Pin the filter on every backend
    // that supports it.
    for b in Backend::ALL {
        if let Some(argv) = mgmt_volume_list_argv(b) {
            assert!(
                argv.iter().any(|a| a == OWNED_LABEL_FILTER),
                "{b:?} volume listing missing the ownership filter: {argv:?}"
            );
        }
    }
}

#[test]
fn parse_owned_images_drops_foreign_references() {
    // A mixed `images` listing: one thegn base image, several foreign. Only the
    // owned one survives — its remove_ref (an id) is the only thing rm accepts.
    let docker = concat!(
        r#"{"ID":"aaa","Repository":"localhost/thegn/base","Tag":"abc123","Size":"120MB"}"#,
        "\n",
        r#"{"ID":"bbb","Repository":"docker.io/library/postgres","Tag":"16","Size":"400MB"}"#,
        "\n",
        r#"{"ID":"ccc","Repository":"nginx","Tag":"latest","Size":"180MB"}"#,
    );
    let owned = parse_owned_images(docker);
    assert_eq!(owned.len(), 1, "only the thegn base image is owned");
    assert_eq!(owned[0].reference, "localhost/thegn/base:abc123");
    assert_eq!(owned[0].remove_ref(), "aaa");
    assert_eq!(owned[0].size_bytes, Some(120_000_000));
    // The rm argv targets the owned id.
    let rm = mgmt_image_rm_argv(Backend::Docker, &owned[0]).unwrap();
    assert_eq!(rm, vec!["image", "rm", "aaa"]);
}

#[test]
fn is_owned_image_matches_only_the_managed_repo() {
    assert!(is_owned_image("localhost/thegn/base:deadbeef"));
    assert!(is_owned_image("localhost/thegn/base"));
    assert!(!is_owned_image("localhost/thegn/basefoo:1")); // not the repo
    assert!(!is_owned_image("docker.io/library/debian:stable"));
    assert!(!is_owned_image(""));
}

// --- persistent-role volume skip --------------------------------------------

#[test]
fn persistent_role_volumes_are_kept() {
    // podman-shape: Labels is a map.
    let podman = r#"[
        {"Name":"thegn-nix-store","Labels":{"thegn.managed":"true","thegn.volume.role":"thegn-nix-store"}},
        {"Name":"thegn-scratch-xyz","Labels":{"thegn.managed":"true"}}
    ]"#;
    let vols = parse_owned_volumes(podman);
    assert_eq!(vols.len(), 2);
    let warm = vols.iter().find(|v| v.name() == "thegn-nix-store").unwrap();
    assert!(warm.is_persistent(), "a role-labelled warm volume is kept");
    assert_eq!(warm.role, "thegn-nix-store");
    let scratch = vols
        .iter()
        .find(|v| v.name() == "thegn-scratch-xyz")
        .unwrap();
    assert!(
        !scratch.is_persistent(),
        "a role-less managed volume is prunable"
    );
    // Only the ephemeral one is removable.
    assert!(mgmt_volume_rm_argv(Backend::Podman, scratch).is_some());
}

#[test]
fn parse_owned_volumes_docker_label_string() {
    // docker-shape: Labels is a comma-joined string.
    let docker = concat!(
        r#"{"Name":"thegn-cargo","Labels":"thegn.managed=true,thegn.volume.role=thegn-cargo"}"#,
        "\n",
        r#"{"Name":"thegn-tmp","Labels":"thegn.managed=true"}"#,
    );
    let vols = parse_owned_volumes(docker);
    assert_eq!(vols.len(), 2);
    assert!(
        vols.iter()
            .find(|v| v.name() == "thegn-cargo")
            .unwrap()
            .is_persistent()
    );
    assert!(
        !vols
            .iter()
            .find(|v| v.name() == "thegn-tmp")
            .unwrap()
            .is_persistent()
    );
}

#[test]
fn role_is_persistent_predicate() {
    assert!(role_is_persistent("thegn-nix-store"));
    assert!(role_is_persistent("anything-nonempty"));
    assert!(!role_is_persistent(""));
    assert!(!role_is_persistent("   "));
}

// --- caps ⇔ optional ops -----------------------------------------------------

#[test]
fn manage_ops_matches_the_builders_for_every_backend() {
    for b in Backend::ALL {
        let ops = manage_ops(b);
        assert_eq!(ops.list, mgmt_list_argv(b).is_some(), "{b:?} list");
        assert_eq!(ops.stats, mgmt_stats_argv(b).is_some(), "{b:?} stats");
        assert_eq!(ops.df, mgmt_df_argv(b).is_some(), "{b:?} df");
        // logs/control need a witness; check via a claimed owned container.
        let c = OwnedContainer::claim("thegn-x").unwrap();
        assert_eq!(ops.logs, mgmt_logs_argv(b, &c, 100).is_some(), "{b:?} logs");
        assert_eq!(
            ops.control,
            mgmt_control_argv(b, ControlOp::Stop, &c).is_some(),
            "{b:?} control"
        );
        // prune ⇔ both image and volume listing available.
        assert_eq!(
            ops.prune,
            mgmt_image_list_argv(b).is_some() && mgmt_volume_list_argv(b).is_some(),
            "{b:?} prune"
        );
    }
}

#[test]
fn verified_daemon_backends_get_the_full_op_set() {
    for b in [Backend::Podman, Backend::PodmanRootful, Backend::Docker] {
        let ops = manage_ops(b);
        assert!(ops.list && ops.stats && ops.df && ops.logs && ops.control && ops.prune);
        assert_eq!(
            ops.names(),
            vec!["list", "stats", "df", "logs", "control", "prune"]
        );
    }
}

#[test]
fn apple_is_list_only_and_smol_is_list_only() {
    // Apple's `container` CLI has no `stats`/`system df` and unverified control
    // verbs — it is offered read-only listing, never a guessed destructive verb.
    let apple = manage_ops(Backend::Apple);
    assert_eq!(apple.names(), vec!["list"]);
    assert_eq!(
        mgmt_list_argv(Backend::Apple).unwrap(),
        vec!["ls", "-a", "--format", "json"]
    );
    // smol is unverified: read-only listing only.
    assert_eq!(manage_ops(Backend::Smol).names(), vec!["list"]);
    // Non-container backends advertise nothing.
    for b in [
        Backend::Bwrap,
        Backend::Systemd,
        Backend::None,
        Backend::Wsl,
    ] {
        assert_eq!(manage_ops(b), ManageOps::default());
        assert!(mgmt_list_argv(b).is_none());
    }
}

// --- read-only listing dialects ---------------------------------------------

#[test]
fn list_argv_dialects() {
    assert_eq!(
        mgmt_list_argv(Backend::Podman).unwrap(),
        vec!["ps", "-a", "--format", "json"]
    );
    assert_eq!(
        mgmt_list_argv(Backend::Docker).unwrap(),
        vec!["ps", "-a", "--format", "{{json .}}"]
    );
    assert_eq!(
        mgmt_stats_argv(Backend::Podman).unwrap(),
        vec!["stats", "--no-stream", "--format", "json"]
    );
    assert_eq!(
        mgmt_df_argv(Backend::Docker).unwrap(),
        vec!["system", "df", "--format", "json"]
    );
}

#[test]
fn logs_and_exec_argv() {
    let c = OwnedContainer::claim("thegn-w1").unwrap();
    assert_eq!(
        mgmt_logs_argv(Backend::Docker, &c, 200).unwrap(),
        vec!["logs", "--tail", "200", "thegn-w1"]
    );
    assert_eq!(
        mgmt_exec_argv(Backend::Podman, &c, &["/bin/sh", "-l"]).unwrap(),
        vec!["exec", "-it", "thegn-w1", "/bin/sh", "-l"]
    );
    assert!(mgmt_exec_argv(Backend::Apple, &c, &["sh"]).is_none());
}

// --- health / running -------------------------------------------------------

#[test]
fn health_from_status() {
    assert!(container_running("Up 3 minutes"));
    assert!(container_running("running"));
    assert!(!container_running("Exited (0) 2 hours ago"));
    assert!(!container_running("Created"));

    assert_eq!(container_health("Up 3 minutes (healthy)"), Health::Healthy);
    assert_eq!(
        container_health("Up 1 second (health: starting)"),
        Health::Starting
    );
    assert_eq!(
        container_health("Up 5 minutes (unhealthy)"),
        Health::Unhealthy
    );
    assert_eq!(container_health("Up 5 minutes"), Health::None);
    assert_eq!(container_health("Exited (137) 1 hour ago"), Health::Stopped);
    assert_eq!(Health::Healthy.label(), "healthy");
    assert_eq!(Health::Stopped.label(), "stopped");
}

// --- size parsing / formatting ----------------------------------------------

#[test]
fn size_parse_and_format() {
    assert_eq!(parse_size_bytes("0B"), Some(0));
    assert_eq!(parse_size_bytes("120MB"), Some(120_000_000));
    assert_eq!(parse_size_bytes("1.2GB"), Some(1_200_000_000));
    assert_eq!(parse_size_bytes("512kB"), Some(512_000));
    assert_eq!(parse_size_bytes("12MiB"), Some(12 * 1024 * 1024));
    assert_eq!(
        parse_size_bytes("1.5 GiB"),
        Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64)
    );
    assert_eq!(parse_size_bytes(""), None);
    assert_eq!(parse_size_bytes("notasize"), None);
    assert_eq!(parse_size_bytes("10ZB"), None); // unknown unit

    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(999), "999 B");
    assert_eq!(human_bytes(1_500), "1.5 kB");
    assert_eq!(human_bytes(1_200_000_000), "1.2 GB");
}

// --- system df parsing ------------------------------------------------------

#[test]
fn system_df_podman_array_shape() {
    // podman: array of {Type, Total, Size (int bytes)}.
    let podman = r#"[
        {"Type":"Images","Total":4,"Active":2,"Size":536870912,"Reclaimable":"..."},
        {"Type":"Containers","Total":3,"Active":1,"Size":1048576},
        {"Type":"Local Volumes","Total":2,"Active":2,"Size":104857600}
    ]"#;
    let du = parse_system_df(podman);
    assert_eq!(du.images, (4, 536_870_912));
    assert_eq!(du.containers, (3, 1_048_576));
    assert_eq!(du.volumes, (2, 104_857_600));
}

#[test]
fn system_df_docker_ndjson_shape() {
    // docker: NDJSON of {Type, TotalCount, Size ("human")}.
    let docker = concat!(
        r#"{"Type":"Images","TotalCount":5,"Size":"1.2GB","Reclaimable":"400MB"}"#,
        "\n",
        r#"{"Type":"Containers","TotalCount":2,"Size":"12MB"}"#,
        "\n",
        r#"{"Type":"Local Volumes","TotalCount":1,"Size":"512MB"}"#,
    );
    let du = parse_system_df(docker);
    assert_eq!(du.images, (5, 1_200_000_000));
    assert_eq!(du.containers, (2, 12_000_000));
    assert_eq!(du.volumes, (1, 512_000_000));
}

#[test]
fn system_df_garbage_degrades_to_zero() {
    assert_eq!(parse_system_df("not json"), DiskUsage::default());
    assert_eq!(parse_system_df(""), DiskUsage::default());
    // A row with an unknown Type is ignored, not an error.
    assert_eq!(
        parse_system_df(r#"[{"Type":"Build Cache","Total":9,"Size":1}]"#),
        DiskUsage::default()
    );
}

#[test]
fn image_reference_from_podman_names_array() {
    let podman = r#"[
        {"Id":"deadbeef","Names":["localhost/thegn/base:abc"],"Size":120000000},
        {"Id":"cafef00d","Names":["docker.io/library/redis:7"],"Size":40000000}
    ]"#;
    let owned = parse_owned_images(podman);
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].reference, "localhost/thegn/base:abc");
    assert_eq!(owned[0].remove_ref(), "deadbeef");
    assert_eq!(owned[0].size_bytes, Some(120_000_000));
}

#[test]
fn owned_label_constants_are_stable() {
    // These strings are matched by the engine and by provisioning; a rename is a
    // breaking change to the owned estate.
    assert_eq!(OWNED_LABEL, "thegn.managed=true");
    assert_eq!(OWNED_LABEL_FILTER, "label=thegn.managed=true");
    assert_eq!(VOLUME_ROLE_LABEL, "thegn.volume.role");
    assert_eq!(ControlOp::Stop.label(), "stop");
    assert_eq!(ControlOp::Remove.label(), "remove");
}
