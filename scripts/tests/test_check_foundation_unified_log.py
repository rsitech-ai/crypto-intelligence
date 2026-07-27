#!/usr/bin/env python3
import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
CHECKER = ROOT / "scripts" / "check-foundation-unified-log.py"


def load_checker_module():
    spec = importlib.util.spec_from_file_location("check_foundation_unified_log", CHECKER)
    if spec is None or spec.loader is None:
        raise RuntimeError("unified-log checker could not be imported")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FoundationUnifiedLogTests(unittest.TestCase):
    def test_ignores_framework_errors_and_substring_false_positives(self) -> None:
        checker = load_checker_module()
        entries = [
            {
                "subsystem": "com.apple.appintents",
                "messageType": "Error",
                "eventMessage": "notification token changed",
            },
            {
                "subsystem": checker.PRODUCT_SUBSYSTEM,
                "messageType": "Default",
                "eventMessage": "availability notification token changed",
            },
        ]

        self.assertEqual(checker.inspect(entries), [])

    def test_rejects_product_error_fault_and_whole_word_runtime_failures(self) -> None:
        checker = load_checker_module()
        entries = [
            {
                "processID": 10,
                "subsystem": checker.PRODUCT_SUBSYSTEM,
                "messageType": "Error",
                "eventMessage": "startup failed",
            },
            {
                "processID": 11,
                "processImagePath": "/tmp/cryptoriskd",
                "messageType": "Fault",
                "eventMessage": "transport unavailable",
            },
            {
                "processID": 12,
                "subsystem": checker.PRODUCT_SUBSYSTEM,
                "messageType": "Default",
                "eventMessage": "runtime panic observed",
            },
        ]

        findings = checker.inspect(entries)

        self.assertEqual(len(findings), 3)
        self.assertEqual(findings[0]["reasons"], ["messageType=Error"])
        self.assertEqual(findings[1]["reasons"], ["messageType=Fault"])
        self.assertEqual(findings[2]["reasons"], ["runtime-failure-term"])

    def test_rejects_non_array_and_non_object_shapes(self) -> None:
        checker = load_checker_module()

        with self.assertRaisesRegex(ValueError, "root must be an array"):
            checker.inspect({})
        with self.assertRaisesRegex(ValueError, "entry 0 must be an object"):
            checker.inspect(["not-an-entry"])


if __name__ == "__main__":
    unittest.main()
