PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS schema_version(version INTEGER PRIMARY KEY);
INSERT OR IGNORE INTO schema_version(version) VALUES(1);
