import importlib.util
import concurrent.futures
import json
from pathlib import Path
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("benchmark_local", Path(__file__).with_name("benchmark-local.py"))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class DriverTests(unittest.TestCase):
    def test_presentation_reads_fragmented_response(self):
        stream = mock.Mock()
        with mock.patch.object(benchmark.select, "select", return_value=([stream], [], [])), \
             mock.patch.object(benchmark.os, "read", side_effect=[b'[{"checked":', b'true}]\n']):
            self.assertEqual(benchmark.read_presentation(stream), [{"checked": True}])

    def test_presentation_bounds_wait_and_response_size(self):
        stream = mock.Mock()
        for chunks, ready, error in [
                ([b"["], [], "deadline"),
                ([b"[", b""], [stream], "closed before replying"),
                ([b" " * 65536] * 17, [stream], "exceeds limit")]:
            with self.subTest(error=error), \
                 mock.patch.object(benchmark.select, "select", return_value=(ready, [], [])), \
                 mock.patch.object(benchmark.os, "read", side_effect=chunks):
                with self.assertRaisesRegex(RuntimeError, error):
                    benchmark.read_presentation(stream)

    def test_shared_session_resource_selection_uses_exact_instance(self):
        first = dict(instance=dict(id="first", owner_generation=1), process=dict(pid=10))
        second = dict(instance=dict(id="second", owner_generation=3), process=dict(pid=20))
        self.assertIs(benchmark.known_owner([first, second], dict(instance="second", generation=3)), second)
        with self.assertRaisesRegex(RuntimeError, "exact process/generation"):
            benchmark.known_owner([first, second], dict(instance="second", generation=2))

    def test_five_minute_idle_checks_gui_cpu_and_rss_too(self):
        before = dict(clock="mach", observed_ns=1, cpu_ns=1, resident_bytes=100,
                      interrupt_wakeups=0, package_idle_wakeups=0, read_bytes=0, written_bytes=0)
        after = dict(before, observed_ns=300_000_000_001, cpu_ns=3_000_000_001,
                     resident_bytes=300 * 1024 * 1024)
        result = benchmark.idle_observation(before, after)
        self.assertFalse(result["five_minute_cpu_under_0_5_percent"])
        self.assertFalse(result["sampled_rss_at_most_256_mib"])

    def tearDown(self):
        benchmark.drivers.clear()
        if hasattr(benchmark.driver_local, "processes"):
            del benchmark.driver_local.processes

    def test_deadline_discards_driver_and_next_command_starts_another(self):
        dead, replacement = mock.Mock(), mock.Mock()
        dead.poll.return_value = None
        response = json.dumps(dict(response={}, start_ns=2, end_ns=3)).encode() + b"\n"
        with mock.patch.object(benchmark.subprocess, "Popen", side_effect=[dead, replacement]) as launch, \
             mock.patch.object(benchmark.select, "select", side_effect=[([], [], []), ([replacement.stdout], [], [])]), \
             mock.patch.object(benchmark.os, "read", return_value=response), \
             mock.patch.object(benchmark, "now_ns", side_effect=[1, 1, 4]):
            with self.assertRaisesRegex(RuntimeError, "response deadline"):
                benchmark.exchange("/fixture", {"op": "status"})
            dead.kill.assert_called_once()
            dead.wait.assert_called_once_with(timeout=5)
            self.assertFalse(benchmark.driver_local.processes)
            self.assertNotIn(dead, benchmark.drivers)
            self.assertEqual(benchmark.exchange("/fixture", {"op": "status"})["response"], {})
            self.assertEqual(launch.call_count, 2)
            self.assertEqual(launch.call_args.args[0], [
                str(Path(__file__).resolve().parents[1] / "target/release/examples/local-ipc-driver"),
                "/fixture",
            ])

    def test_closed_driver_is_discarded_without_replaying_command(self):
        process = mock.Mock()
        process.poll.return_value = 1
        with mock.patch.object(benchmark.subprocess, "Popen", return_value=process) as launch, \
             mock.patch.object(benchmark.select, "select", return_value=([process.stdout], [], [])), \
             mock.patch.object(benchmark.os, "read", return_value=b""):
            with self.assertRaisesRegex(RuntimeError, "closed before replying"):
                benchmark.app_exchange("/fixture", dict(session="session", instance="instance", generation=1), {"op": "input", "kind": 2, "target": 100, "data": []})
            self.assertFalse(benchmark.driver_local.processes)
            launch.assert_called_once()
            process.kill.assert_not_called()

    def test_alternating_profiles_keep_their_own_driver_and_cleanup(self):
        alice, bob = mock.Mock(), mock.Mock()
        alice.poll.return_value = bob.poll.return_value = None
        replies = [json.dumps(dict(response={"actor": actor}, start_ns=start,
            end_ns=start + 1)).encode() + b"\n" for actor, start in
            [("alice", 2), ("bob", 6), ("alice", 10)]]
        with mock.patch.object(benchmark.subprocess, "Popen", side_effect=[alice, bob]) as launch, \
             mock.patch.object(benchmark.select, "select", return_value=([alice.stdout], [], [])), \
             mock.patch.object(benchmark.os, "read", side_effect=replies), \
             mock.patch.object(benchmark, "now_ns", side_effect=[1, 4, 5, 8, 9, 12]):
            for actor in ["alice", "bob", "alice"]:
                self.assertEqual(benchmark.call("/" + actor, {"op": "status"}), {"actor": actor})
            self.assertEqual(launch.call_count, 2)
            self.assertEqual([call.args[0][1] for call in launch.call_args_list], ["/alice", "/bob"])
            self.assertEqual(alice.stdin.write.call_count, 2)
            self.assertEqual(bob.stdin.write.call_count, 1)
            benchmark.close_thread_drivers()
            self.assertFalse(benchmark.drivers)
            self.assertFalse(benchmark.driver_local.processes)
            alice.kill.assert_called_once()
            bob.kill.assert_called_once()

    def test_profile_and_global_connection_limits_do_not_launch_more_drivers(self):
        benchmark.driver_local.processes = {Path("/alice"): mock.Mock(), Path("/bob"): mock.Mock()}
        with mock.patch.object(benchmark.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(RuntimeError, "thread profile limit"):
                benchmark.call("/third", {"op": "input"})
            benchmark.driver_local.processes.clear()
            benchmark.drivers.extend(mock.Mock() for _ in range(benchmark.MAX_DRIVERS))
            with self.assertRaisesRegex(RuntimeError, "connection limit"):
                benchmark.call("/alice", {"op": "input"})
            launch.assert_not_called()

    def test_repeated_short_lived_workers_release_global_connection_slots(self):
        response = json.dumps(dict(response={}, start_ns=2, end_ns=3)).encode() + b"\n"
        processes = [mock.Mock() for _ in range(20)]
        for process in processes:
            process.poll.return_value = None
        def work():
            try:
                return benchmark.call("/alice", {"op": "status"})
            finally:
                benchmark.close_thread_drivers()
        with mock.patch.object(benchmark.subprocess, "Popen", side_effect=processes), \
             mock.patch.object(benchmark.select, "select", return_value=([mock.Mock()], [], [])), \
             mock.patch.object(benchmark.os, "read", return_value=response), \
             mock.patch.object(benchmark, "now_ns", side_effect=[1, 4] * 20):
            for _ in processes:
                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                    self.assertEqual(pool.submit(work).result(), {})
                self.assertFalse(benchmark.drivers)
        for process in processes:
            process.kill.assert_called_once()


if __name__ == "__main__":
    unittest.main()
