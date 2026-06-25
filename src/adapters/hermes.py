import sqlite3

def get_hermes_sessions(db_path: str):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    cursor = conn.execute("SELECT * FROM sessions")
    return [dict(row) for row in cursor.fetchall()]
