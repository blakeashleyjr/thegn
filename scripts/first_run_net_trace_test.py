"""Hermetic tests for the first-run network trace parser and report."""

from __future__ import annotations

from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import first_run_net_trace as net_trace


AF_INET = (
    'connect(5, {sa_family=AF_INET, sin_port=htons(443), '
    'sin_addr=inet_addr("93.184.216.34")}, 16) = -1 EINPROGRESS '
    '(Operation now in progress)'
)
AF_INET6 = (
    'connect(5, {sa_family=AF_INET6, sin6_port=htons(443), '
    'sin6_flowinfo=htonl(0), inet_pton(AF_INET6, "2606:2800::1", &sin6_addr), '
    'sin6_scope_id=0}, 28) = 0'
)


class NetTraceTests(unittest.TestCase):
    def run_main_with_trace(self, lines: list[str]) -> int:
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / "trace.out"
            trace.write_text("\n".join(lines) + "\n", encoding="utf-8")
            with redirect_stdout(StringIO()):
                return net_trace.main(["summarize", str(trace)])

    def test_unix_only_trace_is_clean(self) -> None:
        lines = [
            'connect(3, {sa_family=AF_UNIX, sun_path="/run/user/1000/bus"}, 110) = 0',
            'connect(4, {sa_family=AF_UNIX, sun_path=@"x"}, 3) = 0',
            '+++ exited with 0 +++',
        ]
        report = net_trace.summarize(lines)
        self.assertEqual(report.verdict, "clean")
        self.assertEqual(report.connect_other, 0)
        self.assertEqual(report.connect_unix, 2)
        self.assertEqual(self.run_main_with_trace(lines), 0)

    def test_inet_connect_is_flagged_with_address_and_port(self) -> None:
        report = net_trace.summarize([AF_INET])
        rendered = net_trace.render(report, "thegn", "first-frame")
        self.assertEqual(report.verdict, "non-unix-connect")
        self.assertIn("net-trace.target.1: AF_INET 93.184.216.34:443 x1\n", rendered)
        self.assertEqual(self.run_main_with_trace([AF_INET]), 1)

    def test_inet6_and_loopback_targets(self) -> None:
        loopback = (
            'connect(6, {sa_family=AF_INET, sin_port=htons(80), '
            'sin_addr=inet_addr("127.0.0.1")}, 16) = 0'
        )
        rendered = net_trace.render(net_trace.summarize([AF_INET6, loopback]), "thegn", "first-frame")
        self.assertIn("AF_INET6 [2606:2800::1]:443 x1", rendered)
        self.assertIn("AF_INET 127.0.0.1:80 (loopback) x1", rendered)
        self.assertEqual(self.run_main_with_trace([AF_INET6, loopback]), 1)

    def test_pid_and_timestamp_prefixes_are_stripped(self) -> None:
        plain = net_trace.parse_line(AF_INET)
        self.assertEqual(net_trace.parse_line(f"[pid 4242] 1699999999.123456 {AF_INET}"), plain)
        self.assertEqual(net_trace.parse_line(f"4242  1699999999.123456 {AF_INET}"), plain)

    def test_unfinished_and_resumed_count_once(self) -> None:
        unfinished = AF_INET.split(" =", 1)[0] + " <unfinished ...>"
        resumed = "<... connect resumed>) = -1 EINPROGRESS (Operation now in progress)"
        report = net_trace.summarize([unfinished, resumed])
        self.assertEqual(report.connect_total, 1)

    def test_sendto_to_inet_address_counts_as_other(self) -> None:
        line = (
            'sendto(4, "...", 32, 0, {sa_family=AF_INET, sin_port=htons(53), '
            'sin_addr=inet_addr("10.0.0.1")}, 16) = 32'
        )
        report = net_trace.summarize([line])
        self.assertEqual(report.connect_other, 1)
        self.assertEqual(report.verdict, "non-unix-connect")

    def test_netlink_is_not_unix(self) -> None:
        line = 'connect(3, {sa_family=AF_NETLINK, nl_pid=0, nl_groups=00000000}, 12) = 0'
        report = net_trace.summarize([line])
        self.assertEqual(report.connect_other, 1)
        self.assertEqual(report.verdict, "non-unix-connect")

    def test_other_family_without_extra_fields_is_still_flagged(self) -> None:
        report = net_trace.summarize(['connect(3, {sa_family=AF_UNSPEC}, 2) = 0'])
        self.assertEqual(report.connect_total, 1)
        self.assertEqual(report.connect_other, 1)
        self.assertIn("AF_UNSPEC unknown x1", net_trace.render(report, "thegn", "first-frame"))

    def test_sendmsg_inet_destination_counts_as_other(self) -> None:
        line = (
            'sendmsg(4, {msg_name={sa_family=AF_INET, sin_port=htons(53), '
            'sin_addr=inet_addr("10.0.0.2")}, msg_namelen=16}, 0) = 32'
        )
        report = net_trace.summarize([line])
        self.assertEqual(report.connect_other, 1)
        self.assertEqual(report.targets, (("AF_INET", "10.0.0.2:53", 1),))

    def test_duplicate_targets_are_counted_and_sorted(self) -> None:
        second = AF_INET.replace("93.184.216.34", "198.51.100.7")
        report = net_trace.summarize([second, AF_INET, AF_INET])
        self.assertEqual(
            report.targets,
            (("AF_INET", "198.51.100.7:443", 1), ("AF_INET", "93.184.216.34:443", 2)),
        )

    def test_garbage_lines_never_raise(self) -> None:
        for line in ("random bytes", "", "connect(3, {sa_fa", "\xff\x00garbage"):
            with self.subTest(line=line):
                self.assertIsNone(net_trace.parse_line(line))
        report = net_trace.summarize(["random bytes", "", "connect(3, {sa_fa"])
        self.assertEqual(report.syscalls_seen, 0)

    def test_missing_file_is_no_trace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "missing.strace"
            output = StringIO()
            with redirect_stdout(output):
                status = net_trace.main(["summarize", str(missing)])
        self.assertEqual(status, 2)
        self.assertIn("net-trace.verdict: no-trace\n", output.getvalue())

    def test_render_field_order_is_stable(self) -> None:
        report = net_trace.summarize([AF_INET, 'connect(3, {sa_family=AF_UNIX, sun_path="/tmp/x"}, 10) = 0'])
        rendered = net_trace.render(report, "/path/to/thegn", "first-frame")
        keys = [line.split(":", 1)[0] for line in rendered.splitlines()]
        self.assertEqual(
            keys,
            [
                "net-trace.version",
                "net-trace.binary",
                "net-trace.window",
                "net-trace.syscalls_seen",
                "net-trace.connect_total",
                "net-trace.connect_unix",
                "net-trace.connect_other",
                "net-trace.target.1",
                "net-trace.unix.1",
                "net-trace.verdict",
            ],
        )
        self.assertTrue(rendered.endswith("net-trace.verdict: non-unix-connect\n"))

    def test_script_header_documents_the_format(self) -> None:
        script = (Path(__file__).resolve().parent / "first-run-net-trace.sh").read_text(encoding="utf-8")
        for required in (
            "net-trace.version",
            "net-trace.window",
            "net-trace.connect_other",
            "net-trace.verdict",
            "setuid",
            "first frame",
        ):
            with self.subTest(required=required):
                self.assertIn(required, script)

    def test_script_is_opt_in_and_linux_only(self) -> None:
        script = (Path(__file__).resolve().parent / "first-run-net-trace.sh").read_text(encoding="utf-8")
        self.assertIn("uname -s", script)
        self.assertIn("Linux only", script)


if __name__ == "__main__":
    unittest.main()
