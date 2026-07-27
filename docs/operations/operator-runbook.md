# Operator Runbook

Repository-owned implementation artifact for `docs/operations/operator-runbook.md`. It preserves local-only, point-in-time, bounded-resource, calibrated-probability, and no-execution requirements.

## Raw WAL recovery boundary

The current `CMTIWAL1` frame header contains magic, schema version, and payload
length, but the checksum covers only the payload. The length field therefore
cannot prove where a checksum-failing frame ends. A corrupt length can consume
bytes belonging to later intact frames while still making the claimed frame
extent reach EOF exactly.

Automatic recovery is intentionally narrow:

- a valid frame-prefix ending partway through its header may be truncated and
  synced;
- a complete header declaring bytes beyond EOF is corruption;
- a full frame reaching EOF with a checksum mismatch is corruption; and
- any candidate repair containing a later decodable frame is corruption.

On corruption, the daemon preserves the WAL byte for byte and fails startup.
Keep the daemon stopped, retain a copy of the WAL as incident evidence, and
restore from a separately verified source instead of manually guessing a
truncation offset. Broader automatic tail repair requires a future,
explicitly-versioned frame format with independently authenticated boundary
metadata; no incompatible hidden header change is part of the current runtime.
