# CMTI libsqlite3-sys Backport Provenance

This directory starts from the published `libsqlite3-sys` crate version
`0.37.0` from crates.io, which is licensed under the MIT License and is the FFI
dependency required by `rusqlite` 0.39.0.

The following four generated SQLite files are replaced verbatim from rusqlite
commit `901f9946efdaaa289e6b1c5bd56dc67f4b651e51`:

- `sqlite3/bindgen_bundled_version.rs`
- `sqlite3/bindgen_bundled_version_ext.rs`
- `sqlite3/sqlite3.c`
- `sqlite3/sqlite3.h`

That upstream commit updates the bundled SQLite amalgamation from 3.53.3 to
the official 3.53.4 release. The crate's Rust sources and build script remain
the published 0.37.0 versions so the dependency stays compatible with this
project's pinned Rust 1.88 toolchain.

Expected bundled SQLite identity:

- version: `3.53.4`
- source ID:
  `2026-07-24 19:02:57 bf7c7f30031888f4e796e429ab3978879485813aaca6f641c7b33e4e09459bcc`
- official `sqlite3.c` SHA3-256:
  `67f423e9ebbbdc473cbc4772c872ee6b89f31fde4ed0279a5c25d5f65c043a16`
- crates.io `libsqlite3-sys` 0.37.0 archive SHA-256:
  `b1f111c8c41e7c61a49cd34e44c7619462967221a6443b0ec299e0ac30cfb9b1`

Upstream sources:

- crate: `https://crates.io/crates/libsqlite3-sys/0.37.0`
- rusqlite commit:
  `https://github.com/rusqlite/rusqlite/commit/901f9946efdaaa289e6b1c5bd56dc67f4b651e51`
- SQLite release: `https://www.sqlite.org/releaselog/3_53_4.html`

The complete file inventory is recorded in `CMTI_FILES.sha256` and verified by
`scripts/verify-vendored-sqlite.sh`.
