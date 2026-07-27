# Governance

Maintainers approve architecture, security boundaries, public/persisted schemas,
release policy, and stable model promotion. Model-risk and stable-release
decisions require an independent second approval.

Models progress through `research → shadow → production → revoked`. Promotion is
package-specific, immutable, reversible, and evaluated per fold, regime, asset,
event, and horizon. Missing evidence cannot be waived by CI failures.

Architecture and safety changes are recorded as ADRs. Stable releases require
complete acceptance evidence and a tested rollback path.
