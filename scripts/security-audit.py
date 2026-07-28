#!/usr/bin/env python3
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
VENDOR_ROOT = "apps/macos/Vendor/grpc-swift-nio-transport"
VENDOR_VERIFIER = "scripts/verify-vendored-grpc-transport.sh"
VENDORED_PUBLIC_TLS_TESTS = (
    (
        f"{VENDOR_ROOT}/Tests/GRPCNIOTransportHTTP2Tests/"
        "HTTP2TransportNIOTransportServicesTests.swift"
    ),
    (
        f"{VENDOR_ROOT}/Tests/GRPCNIOTransportHTTP2Tests/"
        "HTTP2TransportTLSEnabledTests.swift"
    ),
)
SCANNED_SUFFIXES = {
    ".json",
    ".py",
    ".rs",
    ".sh",
    ".swift",
    ".toml",
    ".yaml",
    ".yml",
}
REQUIRED_SECURITY_BOUNDARIES = (
    "SECURITY.md",
    "docs/security/threat-model.md",
    "crates/security-policy/src/lib.rs",
    "crates/recovery/src/lib.rs",
    "crates/artifact-signing/src/lib.rs",
    "crates/shadow-ledger/src/lib.rs",
)
PRIVATE_KEY_PATTERN = re.compile(
    b"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----\\r?\\n"
    b"[A-Za-z0-9+/=\\r\\n]{32,}\\r?\\n-----END "
)
SECRET_ASSIGNMENT_PATTERN = re.compile(
    r"(?i)(?<![\"'])\b(password|api[_-]?secret|private[_-]?key)"
    r"\b\s*=\s*[\"'][^\"']+[\"']"
)


def verified_secret_pattern_exclusions() -> list[dict[str, str]]:
    provenance = ROOT / "licenses/artifact-provenance.toml"
    verifier = ROOT / VENDOR_VERIFIER
    if not provenance.is_file() or not verifier.is_file():
        return []

    provenance_text = provenance.read_text(encoding="utf-8", errors="replace")
    required_declarations = (
        f'scope = "{VENDOR_ROOT}"',
        f'verification = "{VENDOR_VERIFIER}"',
    )
    if not all(declaration in provenance_text for declaration in required_declarations):
        return []

    try:
        completed = subprocess.run(
            [str(verifier)],
            cwd=ROOT,
            check=False,
            capture_output=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if completed.returncode != 0:
        return []

    return [
        {
            "path": relative_path,
            "reason": "integrity-verified upstream public TLS test fixture",
            "verifier": VENDOR_VERIFIER,
        }
        for relative_path in VENDORED_PUBLIC_TLS_TESTS
        if (ROOT / relative_path).is_file()
    ]


def main() -> int:
    findings: list[dict[str, str]] = []

    def add(severity: str, path: str, issue: str) -> None:
        findings.append({"severity": severity, "path": path, "issue": issue})

    verified_exclusions = verified_secret_pattern_exclusions()
    excluded_secret_patterns = {
        exclusion["path"] for exclusion in verified_exclusions
    }

    for path in ROOT.rglob("*"):
        if (
            not path.is_file()
            or ".git" in path.parts
            or ".build" in path.parts
            or "__pycache__" in path.parts
        ):
            continue
        relative_path = str(path.relative_to(ROOT))
        if relative_path == "scripts/security-audit.py":
            continue
        data = path.read_bytes()
        if PRIVATE_KEY_PATTERN.search(data):
            add("critical", relative_path, "private key material")
        if len(data) > 64 * 1024 * 1024:
            add("medium", relative_path, "unexpected oversized repository file")
        if path.suffix in SCANNED_SUFFIXES:
            text = data.decode("utf-8", "replace")
            if (
                relative_path not in excluded_secret_patterns
                and SECRET_ASSIGNMENT_PATTERN.search(text)
            ):
                add("high", relative_path, "hard-coded secret-shaped value")

    for required in REQUIRED_SECURITY_BOUNDARIES:
        if not (ROOT / required).is_file():
            add("high", required, "required security boundary missing")

    counts = {
        severity: sum(
            finding["severity"] == severity for finding in findings
        )
        for severity in ("critical", "high", "medium", "low")
    }
    status = (
        "passed"
        if not any(
            finding["severity"] in {"critical", "high"}
            for finding in findings
        )
        else "failed"
    )
    report = {
        "schema_version": 1,
        "repository": "s1korrrr/crypto-inteligence",
        "scope": "secret-pattern and required-boundary-presence scan",
        "limitations": [
            "does not verify security implementation or runtime behavior"
        ],
        "verified_secret_pattern_exclusions": verified_exclusions,
        "findings": findings,
        "counts": counts,
        "status": status,
    }
    output = ROOT / "release/evidence/security-audit.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(counts))
    return 0 if status == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
