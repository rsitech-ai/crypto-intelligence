# Security Policy

## Supported versions and channels

The repository is pre-release. No stable release, beta, canary, or development
snapshot is currently promised security support. Private reports are still
accepted and triaged. When a supported release channel is opened, this table
must be updated before distribution; support is not inferred from a tag or
build artifact.

| Channel | Security fixes |
| --- | --- |
| Stable | None released |
| Beta or canary | None released |
| `main` and development snapshots | Best-effort investigation only |

## Private reporting

Use GitHub's private **Report a vulnerability** form. Do not open a public issue
for a suspected vulnerability. Do not disclose credentials, exploit details,
private keys, account identifiers, or proprietary data publicly. Include the
affected revision or version, impact, reproduction conditions, and a private
contact route where possible.

## Severity and response

Reports are triaged as Critical, High, Medium, or Low using exploitability,
required access, confidentiality/integrity/availability impact, local financial
authority, and recovery cost. CVSS may inform but does not replace product
impact. Maintainers work on a best-effort basis; there is no guaranteed response
or remediation SLA. Receipt, severity, fix timing, and disclosure timing are
confirmed privately when maintainer capacity permits.

## Coordinated disclosure and advisories

Keep details private until a fix or documented mitigation is available and the
coordinated disclosure date is agreed. A published security advisory names the
affected versions, first patched version, impact, mitigations, and credit when
requested. Advisories are published only after the referenced fix and release
artifacts are available. GitHub's private vulnerability reporting and
repository security-advisory workflow are the canonical coordination channels.

## Product authority boundary

Version 1 is local-first research and monitoring software. It has no trading,
withdrawal, account-mutation, hosted-inference, or remote-telemetry authority.
Public/read-only market data and configured loopback nodes are the only
potential external boundaries, and each must be explicitly owned and bounded.
See ADR 0003.

Security-sensitive assets include event integrity, exact books, point-in-time
features and labels, calibrated probabilities, model packages, ledgers, local
session secrets, backups, and update artifacts. Critical controls include
bounded parsers and queues, WAL-first durability, authenticated loopback RPC,
purpose-scoped egress, atomic writes, redaction, and deterministic evidence.

## Release artifact integrity

There are no supported release artifacts yet. A future supported release must
publish SHA-256 checksums and signed artifacts bound to the source revision,
plus installation and rollback evidence. The advisory or release notes must
identify the signing method and patched version; a signature claim is invalid
without a verifiable signer identity and artifact digest.
