use rusqlite::Connection;

const EXPECTED_VERSION: &str = "3.53.4";
const EXPECTED_VERSION_NUMBER: i32 = 3_053_004;
const EXPECTED_SOURCE_ID: &str =
    "2026-07-24 19:02:57 bf7c7f30031888f4e796e429ab3978879485813aaca6f641c7b33e4e09459bcc";

#[test]
fn bundled_sqlite_runtime_matches_the_audited_release() {
    assert_eq!(rusqlite::version(), EXPECTED_VERSION);
    assert_eq!(rusqlite::version_number(), EXPECTED_VERSION_NUMBER);

    let connection = Connection::open_in_memory().expect("open bundled SQLite");
    let source_id = connection
        .query_row("SELECT sqlite_source_id()", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("read SQLite source ID");
    assert_eq!(source_id, EXPECTED_SOURCE_ID);
}
