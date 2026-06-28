import sqlite3
import re

def get_hermes_sessions(db_path: str):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    cursor = conn.execute("SELECT * FROM sessions")
    return [dict(row) for row in cursor.fetchall()]

def parse_hermes_status(output: str):
    result = {}
    current_section = None
    
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
            
        if line.startswith('◆'):
            current_section = line[1:].strip()
            result[current_section] = {}
        elif current_section:
            if ':' in line:
                parts = line.split(':', 1)
                key = parts[0].strip()
                val = parts[1].strip()
                result[current_section][key] = val
            elif ' ' in line:
                # E.g. "OpenRouter    ✓ sk-o...2b6f"
                parts = re.split(r'\s{2,}', line, maxsplit=1)
                if len(parts) == 2:
                    key = parts[0].strip()
                    val = parts[1].strip()
                    result[current_section][key] = val
            
    return result
