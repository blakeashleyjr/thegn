def test_codex_jsonl_parser():
    jsonl = '{"type": "start", "session": "abc"}\n{"type": "end"}'
    
    from src.adapters.codex import parse_codex_stream
    
    events = list(parse_codex_stream(jsonl.splitlines()))
    assert len(events) == 2
    assert events[0]["session"] == "abc"
