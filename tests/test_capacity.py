import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock


os.environ.setdefault("SOAK_API_KEY", "capacity-test-key")
os.environ.setdefault("SOAK_WEBHOOK_SECRET", "capacity-test-secret")
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "scripts"))
import capacity
import capacity_guard
import soak


class GuardTests(unittest.TestCase):
    def sample(self, **changes):
        result = {"elapsed": 60, "available_memory_bytes": 8 * 1024**3, "containers": []}
        result.update(changes)
        return result

    def test_aggregate_cpu_ticks_are_recorded_without_network(self):
        with mock.patch.object(pathlib.Path, "read_text", return_value="cpu 1 2 3 4 5 6 7 8 9 10\ncpu0 1 2 3 4\n"):
            ticks = capacity_guard.host_cpu_ticks()
        self.assertEqual(ticks["user"], 1)
        self.assertEqual(ticks["idle"], 4)
        self.assertEqual(ticks["guest_nice"], 10)

    def test_container_snapshot_is_scoped_and_includes_cumulative_cpu(self):
        files = {
            "/proc/meminfo": "MemAvailable: 8388608 kB\n",
            "/proc/stat": "cpu 1 2 3 4 5 6 7 8 9 10\n",
            "/proc/123/cgroup": "0::/capacity-test\n",
            "/sys/fs/cgroup/capacity-test/memory.current": "500\n",
            "/sys/fs/cgroup/capacity-test/pids.current": "20\n",
            "/sys/fs/cgroup/capacity-test/cpu.stat": "usage_usec 100\nuser_usec 70\nsystem_usec 30\n",
        }
        inspected = '[{"Name":"/capacity-dovecot-1","State":{"Pid":123},"HostConfig":{"Memory":1000}}]'
        with mock.patch.object(capacity_guard, "PROJECT", "capacity-test"):
            with mock.patch.object(capacity_guard, "docker", side_effect=["abc\n", inspected]) as docker:
                with mock.patch.object(pathlib.Path, "read_text", lambda path, *args, **kwargs: files[str(path)]):
                    sample = capacity_guard.snapshot()
        docker.assert_any_call("ps", "-q", "--filter", "label=com.docker.compose.project=capacity-test")
        self.assertEqual(sample["containers"][0]["cpu_usage_usec"], 100)
        self.assertEqual(sample["containers"][0]["cpu_user_usec"], 70)
        self.assertEqual(sample["host_cpu_ticks"]["idle"], 4)

    def test_safe_sample_does_not_stop(self):
        self.assertIsNone(capacity_guard.guard_reason(self.sample()))

    def test_host_memory_reserve_stops_ramp(self):
        reason = capacity_guard.guard_reason(self.sample(available_memory_bytes=5 * 1024**3))
        self.assertIn("host available memory", reason)

    def test_container_limit_stops_before_oom(self):
        reason = capacity_guard.guard_reason(self.sample(containers=[{
            "name": "/capacity-dovecot-1", "memory_limit_bytes": 1000,
            "memory_current_bytes": 901, "pids": 200,
        }]))
        self.assertIn("90%", reason)

    def test_process_guard_and_global_deadline(self):
        reason = capacity_guard.guard_reason(self.sample(containers=[{
            "name": "/capacity-dovecot-1", "memory_limit_bytes": 1000,
            "memory_current_bytes": 500, "pids": 24001,
        }]))
        self.assertIn("process safety limit", reason)
        self.assertIn("four-hour", capacity_guard.guard_reason(self.sample(elapsed=14401)))

    def test_external_stop_is_classified_as_host_guard(self):
        with tempfile.TemporaryDirectory() as directory:
            stop = pathlib.Path(directory) / "capacity.stop"
            stop.write_text("controlled test stop")
            with mock.patch.object(capacity, "STOP_FILE", stop):
                with self.assertRaises(capacity.StopRun) as raised:
                    capacity.guard()
                self.assertEqual(raised.exception.kind, "host_guard")
                self.assertEqual(raised.exception.details["guard_detail"], "controlled test stop")

    def test_internal_memory_guard_classification(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(capacity, "STOP_FILE", pathlib.Path(directory) / "absent"):
                with mock.patch.object(capacity, "host_memory", return_value={"MemAvailable": 1}):
                    with self.assertRaises(capacity.StopRun) as raised:
                        capacity.guard()
                    self.assertEqual(raised.exception.kind, "host_guard")
                    self.assertEqual(raised.exception.details["available_bytes"], 1)


class StageWindowTests(unittest.TestCase):
    def setUp(self):
        self.previous = {
            "PENDING": dict(soak.PENDING), "COUNTERS": dict(soak.COUNTERS),
            "LATENCIES": list(soak.LATENCIES), "METRICS": list(soak.METRICS),
            "READY": set(soak.READY), "SEEN": set(soak.SEEN),
        }
        soak.PENDING.clear()
        self.operations = mock.patch.object(soak, "OPERATIONS")
        self.mock_operations = self.operations.start()
        self.mock_operations.pending.return_value = 0

    def tearDown(self):
        self.operations.stop()
        for name, value in self.previous.items():
            current = getattr(soak, name)
            current.clear()
            if isinstance(current, list):
                current.extend(value)
            else:
                current.update(value)

    def test_stage_window_resets_measurements_but_keeps_dedup_and_readiness(self):
        for name in soak.COUNTERS:
            soak.COUNTERS[name] = 10
        soak.LATENCIES.extend([1.0, 2.0])
        soak.METRICS.append({"previous_stage": True})
        soak.READY.add("existing-account")
        soak.SEEN.add(123)
        capacity.reset_window()
        self.assertTrue(all(value == 0 for value in soak.COUNTERS.values()))
        self.assertEqual(soak.LATENCIES, [])
        self.assertEqual(soak.METRICS, [])
        self.assertIn("existing-account", soak.READY)
        self.assertIn(123, soak.SEEN)

    def test_does_not_discard_undelivered_messages_between_stages(self):
        soak.PENDING["pending-message"] = 123
        with self.assertRaises(capacity.StopRun) as raised:
            capacity.reset_window()
        self.assertEqual(raised.exception.kind, "infrastructure_failure")
        self.assertIn("pending-message", soak.PENDING)

    def test_waits_for_previous_mutation_and_send_checks(self):
        self.mock_operations.pending.return_value = 1
        with self.assertRaises(capacity.StopRun):
            capacity.reset_window()


if __name__ == "__main__":
    unittest.main()
