# Contributing

## Developer Certificate of Origin

Every non-merge commit must certify the
[Developer Certificate of Origin 1.1](https://developercertificate.org/). The
sign-off states that you have the right to submit the contribution under the
project's license and that the `Signed-off-by:` line becomes a permanent public
record.

```bash
git commit -s -m "type: focused change"
```

Use your real contribution identity:

```text
Signed-off-by: Name <email@example.com>
```

## Engineering boundary

Contributions must preserve exact numeric authority, point-in-time correctness,
bounded resources, local-only operation, explicit data quality and abstention,
no automated execution, and evidence-backed model promotion. Add or tighten the
behavioral test first, run all locally available checks, and record unavailable
platform verification as debt rather than as passing.

## Artifact licensing and provenance

The code and documentation license does not automatically license third-party
or separately sourced artifacts. The authoritative, machine-checked inventory
is `licenses/artifact-provenance.toml`. Update it in the same change whenever an
artifact or its content changes.

- A fixture must name whether it is synthetic or sourced, its origin/version,
  explicit license, redistribution permission, owner, retention purpose, and
  integrity digest.
- A dataset must have source terms and redistribution permission before any
  data is committed. An absent-dataset declaration must stay empty.
- A model or model weight must record training/source provenance, license,
  redistribution, owner, version, integrity, and whether it contains executable
  code. Metadata-only model scaffolds must not claim that weights exist.
- A generated schema must identify its generator/source and be reproducible;
  its checked-in digest must match.
- A vendored source must retain its upstream license and notices, upstream URL,
  version/commit, local modifications, and an integrity inventory.

Unknown, incompatible, non-redistributable, or placeholder provenance fails the
license gate. Do not commit private datasets, credentials, wallet material, or
artifacts whose redistribution rights are uncertain.
