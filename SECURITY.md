# Security Policy

Use GitHub's private **Report a vulnerability** form. Do not disclose suspected
vulnerabilities, credentials, exploit details, private keys, account identifiers,
or proprietary data in a public issue.

Version 1 is local-first research and monitoring software. It has no trading,
withdrawal, account-mutation, hosted-inference, or remote-telemetry authority.
Public/read-only market data and configured loopback nodes are the supported
external boundary.

Security-sensitive assets include event integrity, exact books, point-in-time
features and labels, calibrated probabilities, model packages, ledgers, local
session secrets, backups, and update artifacts. Critical controls include
bounded parsers and queues, WAL-first durability, connection epochs and
checksums, bitemporal lineage, authenticated loopback RPC, inherited-descriptor
secrets, purpose-scoped egress, immutable hash chains, role-separated signatures,
atomic writes and activation, quality/OOD abstention, and deterministic
evidence rendering.

Stable releases require a closed security scan, dependency and license review,
SBOM, provenance, signed artifacts, notarization, installation and rollback
evidence, recovery/chaos drills, and no unresolved critical or high-severity
finding.
