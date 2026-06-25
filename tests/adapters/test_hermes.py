def test_hermes_sqlite_query(tmp_path):
    import sqlite3
    from src.adapters.hermes import get_hermes_sessions
    
    db = tmp_path / "state.db"
    conn = sqlite3.connect(db)
    conn.execute("CREATE TABLE sessions (session_id TEXT, title TEXT)")
    conn.execute("INSERT INTO sessions VALUES ('h_123', 'Build DB')")
    conn.commit()
    
    sessions = get_hermes_sessions(db)
    assert sessions[0]["title"] == "Build DB"
