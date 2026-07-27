#!/usr/bin/env python3
import os
import signal
import subprocess
import sys
import time


class Interrupted(Exception):
    def __init__(self, signum: int):
        self.signum = signum


def group_exists(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # EPERM still proves that the process group exists. Keep polling
        # within the caller's deadline and fail closed if it never disappears.
        return True
    return True


def signal_group(process_group: int, signum: int) -> None:
    try:
        os.killpg(process_group, signum)
    except ProcessLookupError:
        pass


def wait_for_group_exit(process_group: int, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not group_exists(process_group):
            return True
        time.sleep(0.01)
    return not group_exists(process_group)


def terminate_group(process: subprocess.Popen[bytes]) -> bool:
    process_group = process.pid
    signal_group(process_group, signal.SIGTERM)
    if process.poll() is None:
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
    if wait_for_group_exit(process_group, 2):
        return True
    signal_group(process_group, signal.SIGKILL)
    if process.poll() is None:
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
    return wait_for_group_exit(process_group, 2)


def cleanup_status(process: subprocess.Popen[bytes]) -> int | None:
    if terminate_group(process):
        return None
    print(
        f"error: command group survived SIGKILL (pgid={process.pid})",
        file=sys.stderr,
    )
    return 125


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: run-bounded.py SECONDS COMMAND [ARG ...]", file=sys.stderr)
        return 2
    try:
        timeout = float(sys.argv[1])
    except ValueError:
        print("error: timeout must be a positive number", file=sys.stderr)
        return 2
    if timeout <= 0:
        print("error: timeout must be a positive number", file=sys.stderr)
        return 2

    command = sys.argv[2:]
    def interrupt(signum: int, _frame: object) -> None:
        raise Interrupted(signum)

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupt)

    process = subprocess.Popen(command, start_new_session=True)
    try:
        status = process.wait(timeout=timeout)
        if group_exists(process.pid):
            cleanup_failure = cleanup_status(process)
            if cleanup_failure is not None:
                return cleanup_failure
        return status
    except subprocess.TimeoutExpired:
        cleanup_failure = cleanup_status(process)
        if cleanup_failure is not None:
            return cleanup_failure
        print(
            f"error: command exceeded deterministic timeout ({timeout:g}s): "
            + " ".join(command),
            file=sys.stderr,
        )
        return 124
    except Interrupted as interruption:
        cleanup_failure = cleanup_status(process)
        if cleanup_failure is not None:
            return cleanup_failure
        return 128 + interruption.signum
    except KeyboardInterrupt:
        cleanup_failure = cleanup_status(process)
        if cleanup_failure is not None:
            return cleanup_failure
        return 128 + signal.SIGINT


if __name__ == "__main__":
    raise SystemExit(main())
