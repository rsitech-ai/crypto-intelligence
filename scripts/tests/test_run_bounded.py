#!/usr/bin/env python3
import os
import pathlib
import signal
import subprocess
import importlib.util
import io
import tempfile
import time
import unittest
from contextlib import redirect_stderr
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[2]
HELPER = ROOT / "scripts" / "run-bounded.py"


def load_helper_module():
    spec = importlib.util.spec_from_file_location("run_bounded", HELPER)
    if spec is None or spec.loader is None:
        raise RuntimeError("run-bounded helper could not be imported")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RunBoundedTests(unittest.TestCase):
    def test_permission_denied_group_probe_is_treated_as_still_existing(self) -> None:
        helper_module = load_helper_module()
        with mock.patch.object(
            helper_module.os,
            "killpg",
            side_effect=PermissionError(1, "operation not permitted"),
        ):
            self.assertTrue(helper_module.group_exists(4242))

    def test_sigkill_reap_uses_a_timeout_when_the_leader_never_reports_exit(self) -> None:
        helper_module = load_helper_module()
        process = mock.Mock(pid=4242)
        process.poll.return_value = None
        process.wait.side_effect = [
            subprocess.TimeoutExpired(["resistant-child"], 2),
            subprocess.TimeoutExpired(["resistant-child"], 2),
        ]
        with (
            mock.patch.object(
                helper_module, "wait_for_group_exit", side_effect=[False, False]
            ),
            mock.patch.object(helper_module, "signal_group"),
        ):
            self.assertFalse(helper_module.terminate_group(process))

        self.assertEqual(
            process.wait.call_args_list,
            [mock.call(timeout=2), mock.call(timeout=2)],
        )

    def test_verifier_has_no_unbounded_wait_immediately_after_sigkill(self) -> None:
        verifier = (ROOT / "scripts" / "verify-foundation-runtime.sh").read_text()
        lines = verifier.splitlines()
        violations = []
        for index, line in enumerate(lines):
            if "kill -KILL" not in line:
                continue
            following = lines[index + 1 : index + 7]
            if any(candidate.lstrip().startswith("wait ") for candidate in following):
                violations.append(index + 1)
        self.assertEqual(
            violations,
            [],
            f"unbounded wait follows SIGKILL near lines {violations}",
        )

    def test_verifier_empty_app_root_cleanup_is_safe_under_bash_nounset(self) -> None:
        verifier = (ROOT / "scripts" / "verify-foundation-runtime.sh").read_text()
        self.assertNotIn('"${app_runtime_roots[@]}"', verifier)
        self.assertEqual(verifier.count('"${app_runtime_roots[@]-}"'), 2)
        completed = subprocess.run(
            [
                "/bin/bash",
                "-u",
                "-c",
                'app_runtime_roots=(); for root in "${app_runtime_roots[@]-}"; do :; done',
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_verifier_stages_the_canonical_layered_runtime_config(self) -> None:
        verifier = (ROOT / "scripts" / "verify-foundation-runtime.sh").read_text()

        self.assertIn(
            'configs/default.toml > "${runtime_root}/config.toml"',
            verifier,
        )
        self.assertIn(
            '"${runtime_root}/fixtures/binance"',
            verifier,
        )
        self.assertIn(
            '"${runtime_root}/models/public-test-artifacts"',
            verifier,
        )
        self.assertIn(
            'chmod 700 "${runtime_root}/data" "${runtime_root}/logs"',
            verifier,
        )
        self.assertIn(
            '"${runtime_root}/fixtures/binance/btcusdt-book-v1.jsonl"',
            verifier,
        )
        self.assertNotIn('data_root = "./data"', verifier)
        self.assertNotIn('fixture_input = "./fixture.jsonl"', verifier)

    def test_cleanup_failure_overrides_a_nominal_command_success(self) -> None:
        helper_module = load_helper_module()
        process = mock.Mock(pid=4242)
        process.wait.return_value = 0
        stderr = io.StringIO()
        with (
            mock.patch.object(helper_module.subprocess, "Popen", return_value=process),
            mock.patch.object(helper_module.signal, "signal"),
            mock.patch.object(helper_module, "group_exists", return_value=True),
            mock.patch.object(helper_module, "terminate_group", return_value=False),
            mock.patch.object(helper_module.sys, "argv", ["run-bounded.py", "1", "true"]),
            redirect_stderr(stderr),
        ):
            status = helper_module.main()

        self.assertEqual(status, 125)
        self.assertIn("command group survived SIGKILL (pgid=4242)", stderr.getvalue())

    def test_verifier_termination_escalates_and_cleans_the_command_group(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "child.pid"
            helper = subprocess.Popen(
                [
                    str(HELPER),
                    "30",
                    "/bin/sh",
                    "-c",
                    (
                        f"/bin/sh -c 'trap \"\" TERM; printf \"%s\" $$ > {pid_file}; "
                        "sleep 30' & wait"
                    ),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            child_pid = None
            try:
                for _ in range(100):
                    if pid_file.exists():
                        child_pid = int(pid_file.read_text())
                        break
                    time.sleep(0.01)
                self.assertIsNotNone(child_pid)
                helper.send_signal(signal.SIGTERM)
                self.assertEqual(helper.wait(timeout=5), 128 + signal.SIGTERM)
                for _ in range(100):
                    try:
                        os.kill(child_pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail("interrupted helper left its TERM-ignoring child alive")
            finally:
                if helper.poll() is None:
                    helper.kill()
                    helper.wait()
                if child_pid is not None:
                    try:
                        os.kill(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_normal_leader_exit_cleans_surviving_group_members(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "child.pid"
            result = subprocess.run(
                [
                    str(HELPER),
                    "5",
                    "/bin/sh",
                    "-c",
                    (
                        f"/bin/sh -c 'trap \"\" TERM; printf \"%s\" $$ > {pid_file}; "
                        "sleep 30' & "
                        "while [ ! -s "
                        f"{pid_file}"
                        " ]; do sleep 0.01; done; exit 0"
                    ),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            child_pid = int(pid_file.read_text())
            for _ in range(100):
                try:
                    os.kill(child_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.fail("normal leader exit left a command-group member alive")

    def test_timeout_is_exit_124_and_terminates_the_command_group(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            pid_file = pathlib.Path(directory) / "child.pid"
            result = subprocess.run(
                [
                    str(HELPER),
                    "0.1",
                    "/bin/sh",
                    "-c",
                    f"printf '%s' $$ > {pid_file}; exec sleep 10",
                ],
                capture_output=True,
                text=True,
                check=False,
            )

            self.assertEqual(result.returncode, 124)
            self.assertIn("deterministic timeout (0.1s)", result.stderr)
            child_pid = int(pid_file.read_text())
            for _ in range(20):
                try:
                    os.kill(child_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("timed-out child process survived its command group")

    def test_command_exit_and_streams_are_preserved(self) -> None:
        result = subprocess.run(
            [
                str(HELPER),
                "2",
                "/bin/sh",
                "-c",
                "printf visible-output; printf visible-error >&2; exit 7",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, "visible-output")
        self.assertEqual(result.stderr, "visible-error")


if __name__ == "__main__":
    unittest.main()
