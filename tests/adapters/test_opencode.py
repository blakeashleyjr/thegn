def test_opencode_session_list():
    raw_json = '[{"id": "ses_123", "title": "Refactor auth", "model": "claude"}]'
    
    from src.adapters.opencode import parse_opencode_sessions
    
    sessions = parse_opencode_sessions(raw_json)
    assert sessions[0]["title"] == "Refactor auth"
