#!/usr/bin/env python3
"""The single tokenizer for the performance harness key files."""

import os
import sys
import time


def events(data):
    """Yield key events using the byte rules shared by count and write."""
    index = 0
    while index < len(data):
        if data[index] == 0x1B and index + 1 < len(data):
            end = index + 2
            if data[index + 1] == ord("["):
                while end < len(data) and not 0x40 <= data[end] <= 0x7E:
                    end += 1
                end = min(end + 1, len(data))
            yield data[index:end]
            index = end
        else:
            yield data[index : index + 1]
            index += 1


def read_events(path):
    with open(path, "rb") as key_file:
        return list(events(key_file.read()))


def main(argv):
    if len(argv) < 2 or argv[1] not in ("count", "chars", "write"):
        print("usage: key-events.py {count|chars|write} FILE [FD]", file=sys.stderr)
        return 2

    action = argv[1]
    expected_args = 4 if action == "write" else 3
    if len(argv) != expected_args:
        print("usage: key-events.py {count|chars|write} FILE [FD]", file=sys.stderr)
        return 2
    path = argv[2]
    try:
        key_events = read_events(path)
        if action == "count":
            print(len(key_events))
        elif action == "chars":
            print(sum(len(event) == 1 and 0x20 <= event[0] <= 0x7E for event in key_events))
        else:
            fd = int(argv[3])
            for event in key_events:
                view = memoryview(event)
                while view:
                    written = os.write(fd, view)
                    view = view[written:]
                time.sleep(0.1)
    except (OSError, ValueError) as error:
        print(f"key-events.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
