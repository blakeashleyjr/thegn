def test_sidecar_event_emission(capsys):
    from src.sidecar import emit_live_update
    emit_live_update("pi", "ses_123", {"input": 100, "output": 50}, 0.02)
    captured = capsys.readouterr()
    assert '{"agent": "pi"' in captured.out
    assert '"session_id": "ses_123"' in captured.out
    assert '"cost": 0.02' in captured.out