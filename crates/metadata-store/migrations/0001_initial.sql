CREATE TABLE schema_migrations (
  version INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  applied_at_ns INTEGER NOT NULL,
  checksum BLOB NOT NULL CHECK(length(checksum) = 32)
) STRICT;

CREATE TRIGGER schema_migrations_no_update
BEFORE UPDATE ON schema_migrations
BEGIN
  SELECT RAISE(ABORT, 'schema migrations are append-only');
END;

CREATE TRIGGER schema_migrations_no_delete
BEFORE DELETE ON schema_migrations
BEGIN
  SELECT RAISE(ABORT, 'schema migrations are append-only');
END;

CREATE TABLE runtime_state (
  singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
  clean_shutdown INTEGER NOT NULL CHECK(clean_shutdown IN (0, 1)),
  startup_generation INTEGER NOT NULL CHECK(startup_generation >= 0)
) STRICT;

INSERT INTO runtime_state(singleton, clean_shutdown, startup_generation)
VALUES(1, 1, 0);

CREATE TABLE audit_chain_head (
  singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
  last_sequence INTEGER NOT NULL CHECK(last_sequence >= 0),
  last_hash BLOB NOT NULL CHECK(length(last_hash) = 32)
) STRICT;

INSERT INTO audit_chain_head(singleton, last_sequence, last_hash)
VALUES(1, 0, zeroblob(32));

CREATE TABLE audit_records (
  sequence INTEGER PRIMARY KEY CHECK(sequence > 0),
  audit_id TEXT NOT NULL UNIQUE CHECK(length(audit_id) BETWEEN 1 AND 128),
  occurred_at_ns INTEGER NOT NULL,
  actor TEXT NOT NULL CHECK(length(actor) BETWEEN 1 AND 128),
  category TEXT NOT NULL CHECK(length(category) BETWEEN 1 AND 128),
  payload_schema TEXT NOT NULL CHECK(length(payload_schema) BETWEEN 1 AND 128),
  payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 1048576),
  payload_hash BLOB NOT NULL CHECK(length(payload_hash) = 32),
  previous_hash BLOB NOT NULL CHECK(length(previous_hash) = 32),
  record_hash BLOB NOT NULL CHECK(length(record_hash) = 32)
) STRICT;

CREATE TRIGGER audit_records_no_update
BEFORE UPDATE ON audit_records
BEGIN
  SELECT RAISE(ABORT, 'audit records are append-only');
END;

CREATE TRIGGER audit_records_no_delete
BEFORE DELETE ON audit_records
BEGIN
  SELECT RAISE(ABORT, 'audit records are append-only');
END;

CREATE TABLE catalog_head (
  singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
  current_revision INTEGER NOT NULL CHECK(current_revision >= 0),
  catalog_digest BLOB NOT NULL CHECK(length(catalog_digest) = 32),
  history_digest BLOB NOT NULL CHECK(length(history_digest) = 32)
) STRICT;

INSERT INTO catalog_head(singleton, current_revision, catalog_digest, history_digest)
VALUES(1, 0, zeroblob(32), zeroblob(32));

CREATE TABLE catalog_requests (
  request_id TEXT PRIMARY KEY CHECK(length(request_id) BETWEEN 1 AND 120),
  request_payload BLOB NOT NULL CHECK(length(request_payload) BETWEEN 1 AND 1048576),
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  expected_revision INTEGER NOT NULL CHECK(expected_revision >= 0),
  result_revision INTEGER NOT NULL CHECK(result_revision >= 0),
  appended_records INTEGER NOT NULL CHECK(appended_records BETWEEN 0 AND 64),
  idempotent_records INTEGER NOT NULL CHECK(idempotent_records BETWEEN 0 AND 64),
  catalog_digest BLOB NOT NULL CHECK(length(catalog_digest) = 32),
  history_digest BLOB NOT NULL CHECK(length(history_digest) = 32),
  audit_sequence INTEGER NOT NULL UNIQUE REFERENCES audit_records(sequence),
  CHECK(appended_records + idempotent_records BETWEEN 1 AND 64)
) STRICT;

CREATE TRIGGER catalog_requests_no_update
BEFORE UPDATE ON catalog_requests
BEGIN
  SELECT RAISE(ABORT, 'catalog requests are append-only');
END;

CREATE TRIGGER catalog_requests_no_delete
BEFORE DELETE ON catalog_requests
BEGIN
  SELECT RAISE(ABORT, 'catalog requests are append-only');
END;

CREATE TABLE catalog_commits (
  revision INTEGER PRIMARY KEY CHECK(revision > 0),
  previous_revision INTEGER NOT NULL CHECK(previous_revision >= 0),
  known_at_ns INTEGER NOT NULL,
  record_count INTEGER NOT NULL CHECK(record_count BETWEEN 1 AND 64),
  catalog_digest BLOB NOT NULL CHECK(length(catalog_digest) = 32),
  history_digest BLOB NOT NULL CHECK(length(history_digest) = 32),
  audit_sequence INTEGER NOT NULL UNIQUE REFERENCES audit_records(sequence),
  request_id TEXT NOT NULL UNIQUE REFERENCES catalog_requests(request_id)
) STRICT;

CREATE TRIGGER catalog_commits_no_update
BEFORE UPDATE ON catalog_commits
BEGIN
  SELECT RAISE(ABORT, 'catalog commits are append-only');
END;

CREATE TRIGGER catalog_commits_no_delete
BEFORE DELETE ON catalog_commits
BEGIN
  SELECT RAISE(ABORT, 'catalog commits are append-only');
END;

CREATE TABLE catalog_records (
  revision INTEGER NOT NULL REFERENCES catalog_commits(revision),
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 1048576),
  payload_hash BLOB NOT NULL CHECK(length(payload_hash) = 32),
  PRIMARY KEY(revision, ordinal)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER catalog_records_no_update
BEFORE UPDATE ON catalog_records
BEGIN
  SELECT RAISE(ABORT, 'catalog records are append-only');
END;

CREATE TRIGGER catalog_records_no_delete
BEFORE DELETE ON catalog_records
BEGIN
  SELECT RAISE(ABORT, 'catalog records are append-only');
END;
