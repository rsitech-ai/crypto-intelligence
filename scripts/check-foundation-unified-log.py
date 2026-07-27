#!/usr/bin/env python3
import json
import pathlib
import re
import sys


PRODUCT_SUBSYSTEM = "ai.rsitech.CuspObservatory"
RUNTIME_FAILURE = re.compile(r"\b(?:panic|panicked|crash|crashed|hang|hung)\b", re.I)


def inspect(entries: object) -> list[dict[str, object]]:
    if not isinstance(entries, list):
        raise ValueError("unified log JSON root must be an array")
    findings = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise ValueError(f"unified log entry {index} must be an object")
        image = pathlib.PurePath(str(entry.get("processImagePath", ""))).name
        product_owned = (
            entry.get("subsystem") == PRODUCT_SUBSYSTEM or image == "cryptoriskd"
        )
        if not product_owned:
            continue
        message_type = str(entry.get("messageType", ""))
        message = " ".join(
            str(entry.get(field, "")) for field in ("eventMessage", "formatString")
        )
        reasons = []
        if message_type.casefold() in {"error", "fault"}:
            reasons.append(f"messageType={message_type}")
        if RUNTIME_FAILURE.search(message):
            reasons.append("runtime-failure-term")
        if reasons:
            findings.append(
                {
                    "entry_index": index,
                    "process_id": entry.get("processID"),
                    "process_image_path": entry.get("processImagePath"),
                    "subsystem": entry.get("subsystem"),
                    "category": entry.get("category"),
                    "message_type": message_type,
                    "event_message": entry.get("eventMessage"),
                    "reasons": reasons,
                }
            )
    return findings


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check-foundation-unified-log.py LOG_JSON", file=sys.stderr)
        return 2
    try:
        entries = json.loads(pathlib.Path(sys.argv[1]).read_text())
        findings = inspect(entries)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(json.dumps({"parse_error": str(error)}, sort_keys=True))
        return 2
    print(json.dumps({"findings": findings}, sort_keys=True))
    return 1 if findings else 0


if __name__ == "__main__":
    raise SystemExit(main())
