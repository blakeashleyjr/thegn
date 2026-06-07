use superzej_core::plugin_api::*;

fn cap(s: &str) -> Capability {
    Capability::parse(s).expect("capability parses")
}

fn sample_manifest() -> PluginManifest {
    toml::from_str(
        r#"
id = "todoist"
name = "Todoist"
version = "1.2.3"
api = "0.1.0"
capabilities = ["surface:statusbar", "network:api.todoist.com", "state:todoist"]

[[contributions]]
id = "todoist.count"
extension_point = "StatusBarSegment"
label = "Todoist"
surface = "todoist.status"

[contributions.cadence]
kind = "interval"
millis = 60000

[contributions.metadata]
align = "right"
"#,
    )
    .expect("manifest parses")
}

#[test]
fn manifest_is_the_transport_neutral_contract_shape() {
    let manifest = sample_manifest();

    assert_eq!(manifest.id.as_str(), "todoist");
    assert_eq!(manifest.api, ApiVersion::new(0, 1, 0));
    assert_eq!(manifest.capabilities[1], cap("network:api.todoist.com"));
    assert_eq!(
        manifest.contributions[0].extension_point,
        ExtensionPoint::StatusBarSegment
    );
    assert_eq!(
        manifest.contributions[0].cadence,
        CadenceHint::Interval { millis: 60000 }
    );
    assert_eq!(
        manifest.contributions[0]
            .metadata
            .get("align")
            .map(String::as_str),
        Some("right")
    );

    // Forward-compatible manifests ignore unknown fields instead of failing load.
    let with_unknown = r#"
id = "future"
name = "Future"
version = "9.9.9"
api = "0.1.0"
unknown_future_field = "ignored"
capabilities = []
"#;
    let future: PluginManifest = toml::from_str(with_unknown).expect("unknown fields ignored");
    assert_eq!(future.id.as_str(), "future");

    // Same logical shape is serializable for JSON-RPC / WASM buffers.
    let json = serde_json::to_value(&manifest).expect("json serializes");
    assert_eq!(json["id"], "todoist");
}

#[test]
fn negotiation_grants_capabilities_and_filters_missing_extension_points() {
    let mut manifest = sample_manifest();
    manifest.capabilities.push(cap("run:notmuch"));
    manifest.contributions.push(Contribution {
        id: ContributionId::new("todoist.theme"),
        extension_point: ExtensionPoint::Theme,
        label: "Unsupported theme".into(),
        surface: None,
        cadence: CadenceHint::OnDemand,
        metadata: Default::default(),
    });

    let host = HostContract::new(ApiVersion::new(0, 1, 0))
        .with_extension_points([
            ExtensionPoint::StatusBarSegment,
            ExtensionPoint::PaletteAction,
        ])
        .with_grants([
            cap("surface:statusbar"),
            cap("network:api.todoist.com"),
            cap("state:todoist"),
        ]);

    let negotiated = host.negotiate(&manifest).expect("compatible API");

    assert!(negotiated.is_capability_granted(&cap("network:api.todoist.com")));
    assert!(negotiated.is_capability_denied(&cap("run:notmuch")));
    assert_eq!(negotiated.accepted_contributions.len(), 1);
    assert_eq!(
        negotiated.accepted_contributions[0].id.as_str(),
        "todoist.count"
    );
    assert_eq!(negotiated.unsupported_contributions.len(), 1);
    assert_eq!(
        negotiated.unsupported_contributions[0].extension_point,
        ExtensionPoint::Theme
    );

    let too_new = PluginManifest {
        api: ApiVersion::new(1, 0, 0),
        ..sample_manifest()
    };
    assert!(matches!(
        host.negotiate(&too_new),
        Err(PluginApiError::IncompatibleApi { .. })
    ));
}

#[test]
fn unknown_extension_points_parse_and_degrade_as_unsupported() {
    let manifest: PluginManifest = toml::from_str(
        r#"
id = "future"
name = "Future Tile"
version = "0.0.1"
api = "0.1.0"
capabilities = []

[[contributions]]
id = "future.tile"
extension_point = "DailyDriverTile"
label = "Future"
"#,
    )
    .expect("future extension points are forward-compatible");

    assert_eq!(
        manifest.contributions[0].extension_point,
        ExtensionPoint::Unknown("DailyDriverTile".into())
    );

    let host = HostContract::new(ApiVersion::new(0, 1, 0))
        .with_extension_points([ExtensionPoint::StatusBarSegment]);
    let negotiated = host.negotiate(&manifest).expect("compatible API");
    assert!(negotiated.accepted_contributions.is_empty());
    assert_eq!(negotiated.unsupported_contributions.len(), 1);
}

#[test]
fn every_side_effect_is_capability_checked_and_audited() {
    let mut manifest = sample_manifest();
    manifest.capabilities.push(cap("notify:telegram"));
    let host = HostContract::new(ApiVersion::new(0, 1, 0))
        .with_extension_points([ExtensionPoint::StatusBarSegment])
        .with_grants([
            cap("surface:statusbar"),
            cap("network:api.todoist.com"),
            cap("state:todoist"),
            cap("notify:telegram"),
        ]);
    let negotiated = host.negotiate(&manifest).unwrap();
    let mut runtime = PluginRuntime::new(negotiated);

    let ok = runtime
        .io(
            PluginId::new("todoist"),
            IoRequest::network("GET", "https://api.todoist.com/rest/v2/tasks"),
        )
        .expect("granted network call is accepted");
    assert_eq!(ok.status, IoStatus::Accepted);

    let denied = runtime
        .io(
            PluginId::new("todoist"),
            IoRequest::run("notmuch", ["search", "tag:inbox"]),
        )
        .expect_err("undeclared run is denied");
    assert!(matches!(denied, PluginApiError::CapabilityDenied { .. }));

    runtime
        .notify(
            PluginId::new("todoist"),
            Alert::new("telegram", "tasks ready"),
        )
        .expect("notify grant is checked");
    runtime
        .state_set(PluginId::new("todoist"), "cursor", serde_json::json!(42))
        .expect("state grant is checked");
    assert_eq!(
        runtime
            .state_get(PluginId::new("todoist"), "cursor")
            .unwrap(),
        Some(serde_json::json!(42))
    );

    let audit = runtime.audit_log();
    assert_eq!(audit.len(), 5, "audit entries: {audit:?}");
    assert!(audit.iter().any(|e| e.decision == AuditDecision::Granted));
    assert!(audit.iter().any(|e| e.decision == AuditDecision::Denied));
    assert!(audit.iter().any(|e| e.operation == "io.run"));
}

#[test]
fn runtime_implements_register_subscribe_update_invalidate_emit_and_host_value_verbs() {
    let manifest = sample_manifest();
    let host = HostContract::new(ApiVersion::new(0, 1, 0))
        .with_extension_points([ExtensionPoint::StatusBarSegment])
        .with_grants([
            cap("surface:statusbar"),
            cap("network:api.todoist.com"),
            cap("state:todoist"),
        ]);
    let negotiated = host.negotiate(&manifest).unwrap();
    let mut runtime =
        PluginRuntime::new(negotiated).with_host_value("branch", serde_json::json!("main"));
    let plugin = PluginId::new("todoist");
    let surface = SurfaceId::new("todoist.status");

    runtime
        .register(plugin.clone(), sample_manifest().contributions.remove(0))
        .expect("accepted contribution can be registered idempotently");
    runtime
        .subscribe(plugin.clone(), EventKind::Timer)
        .expect("subscriptions are recorded");
    assert!(runtime
        .subscriptions()
        .contains(&(plugin.clone(), EventKind::Timer)));

    let view = View::line([Span::styled("main", StyleRole::Accent)]);
    runtime
        .update(plugin.clone(), surface.clone(), view.clone())
        .expect("surface update uses the accepted surface grant");
    assert_eq!(runtime.view(&surface), Some(&view));
    runtime.invalidate(plugin.clone(), surface.clone()).unwrap();
    assert!(runtime.is_dirty(&surface));

    runtime
        .emit(
            plugin.clone(),
            Event::new(EventKind::BusMessage, serde_json::json!({"x": 1})),
        )
        .expect("bus emit is recorded");
    assert_eq!(runtime.events().len(), 1);
    assert_eq!(
        runtime.host_value(plugin, "branch").unwrap(),
        Some(serde_json::json!("main"))
    );
}

#[test]
fn surface_cache_keeps_last_good_view_and_degrades_on_budget_overrun() {
    let mut cache = SurfaceCache::default();
    let surface = SurfaceId::new("todoist.status");
    let view = View::line([Span::styled(" 3 tasks ", StyleRole::Accent)]);

    let update = cache.update(surface.clone(), view.clone());
    assert!(update.changed);
    assert!(!cache.is_dirty(&surface));
    assert_eq!(cache.view(&surface), Some(&view));

    cache.invalidate(&surface);
    assert!(cache.is_dirty(&surface));

    let degraded = cache.degrade(&surface, DegradeReason::RenderBudgetExceeded);
    assert!(degraded.degraded);
    assert_eq!(degraded.text_content(), " 3 tasks  ⚠");
    assert_eq!(
        cache.view(&surface),
        Some(&view),
        "last-good view remains cached"
    );
}

#[test]
fn json_rpc_projection_uses_the_same_method_names_as_the_contract_verbs() {
    let request = RpcMessage::request(
        7,
        HostVerb::Register,
        serde_json::to_value(sample_manifest().contributions.remove(0)).unwrap(),
    );
    let wire = serde_json::to_string(&request).unwrap();
    assert!(wire.contains(r#""method":"register""#), "{wire}");

    let event = Event::new(
        EventKind::FocusChanged,
        serde_json::json!({ "tab": "repo/main" }),
    );
    let callback = RpcMessage::notification(
        PluginCallback::OnEvent,
        serde_json::to_value(event).unwrap(),
    );
    let parsed: RpcMessage =
        serde_json::from_str(&serde_json::to_string(&callback).unwrap()).unwrap();
    assert_eq!(parsed.method(), Some("on_event"));
}
