//! End-to-end tests against a real Postgres.
//!
//! Every SQL statement in this crate is a runtime string rather than a
//! compile-time-checked macro, so nothing but actually executing it proves it
//! parses, that the columns exist, and that the joins do what they claim. These
//! tests are where that happens.
//!
//! They are behind a feature flag because they need a database:
//!
//! ```sh
//! podman run -d --name go-notes-test-pg \
//!   -e POSTGRES_USER=go_notes -e POSTGRES_PASSWORD=go_notes -e POSTGRES_DB=go_notes \
//!   -p 55432:5432 docker.io/library/postgres:17-alpine
//!
//! DATABASE_URL=postgres://go_notes:go_notes@127.0.0.1:55432/go_notes \
//!   cargo test -p go-notes-server --features integration
//! ```
//!
//! Each test gets its own freshly-created database, so they neither interfere
//! with one another nor leave anything behind.

#![cfg(feature = "integration")]

use go_notes_server::db::{self, NewUser, User};
use go_notes_server::vault::{index, store, Vault};
use sqlx::{AssertSqlSafe, Executor, PgPool, Row};
use tempfile::TempDir;

/// A database and a vault directory, both discarded at the end of the test.
struct Harness {
    pool: PgPool,
    user: User,
    vault: Vault,
    database: String,
    admin_url: String,
    _dir: TempDir,
}

impl Harness {
    async fn new(label: &str) -> Harness {
        let admin_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to run the integration tests");

        // A unique database per test. Postgres will not let us CREATE DATABASE
        // inside a transaction, so this connects to the default one first.
        let database = format!(
            "go_notes_test_{label}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let admin = PgPool::connect(&admin_url).await.expect("connect");
        // `AssertSqlSafe` because sqlx refuses runtime-built SQL by default —
        // rightly. The database name here is generated from a timestamp a few
        // lines above and never comes from input, so there is nothing to inject.
        admin
            .execute(AssertSqlSafe(format!(r#"CREATE DATABASE "{database}""#)))
            .await
            .expect("create test database");
        admin.close().await;

        let url = swap_database(&admin_url, &database);
        let pool = PgPool::connect(&url).await.expect("connect to test database");
        db::migrate(&pool).await.expect("migrations");

        let dir = TempDir::new().unwrap();
        let user = db::find_or_create_user(
            &pool,
            NewUser {
                username: "tester",
                display_name: "Tester",
                email: None,
                auth_provider: "local",
                auth_subject: "tester",
            },
        )
        .await
        .expect("create user");

        let vault = Vault::open(dir.path(), &user.vault_dir).expect("open vault");

        Harness {
            pool,
            user,
            vault,
            database,
            admin_url,
            _dir: dir,
        }
    }

    /// Writes a note and indexes it, the way a save does.
    async fn write(&self, rel_path: &str, markdown: &str) {
        let path = self.vault.resolve_note(rel_path).expect("valid path");
        let file = store::write_note(&path, markdown).await.expect("write");
        index::index_note_content(&self.pool, &self.user, &self.vault, &path, &file)
            .await
            .expect("index");
    }

    async fn read(&self, rel_path: &str) -> String {
        let path = self.vault.resolve_note(rel_path).expect("valid path");
        store::read_note(&path).await.expect("read").markdown
    }

    /// The paths this note's links resolve to, `None` where a link is broken.
    async fn outgoing(&self, rel_path: &str) -> Vec<(String, Option<String>)> {
        sqlx::query(
            "SELECT l.target_raw, n.rel_path
             FROM links l
             JOIN notes source ON source.id = l.source_note_id
             LEFT JOIN notes n ON n.id = l.target_note_id
             WHERE source.user_id = $1 AND source.rel_path = $2
             ORDER BY l.ordinal",
        )
        .bind(self.user.id)
        .bind(rel_path)
        .fetch_all(&self.pool)
        .await
        .expect("outgoing links")
        .into_iter()
        .map(|row| (row.get("target_raw"), row.get("rel_path")))
        .collect()
    }

    /// Paths of notes linking to this one.
    async fn backlinks(&self, rel_path: &str) -> Vec<String> {
        sqlx::query(
            "SELECT source.rel_path
             FROM links l
             JOIN notes source ON source.id = l.source_note_id
             JOIN notes target ON target.id = l.target_note_id
             WHERE target.user_id = $1 AND target.rel_path = $2
             ORDER BY source.rel_path",
        )
        .bind(self.user.id)
        .bind(rel_path)
        .fetch_all(&self.pool)
        .await
        .expect("backlinks")
        .into_iter()
        .map(|row| row.get("rel_path"))
        .collect()
    }

    async fn note_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM notes WHERE user_id = $1")
            .bind(self.user.id)
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }

    async fn tags(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM tags WHERE user_id = $1 ORDER BY name")
            .bind(self.user.id)
            .fetch_all(&self.pool)
            .await
            .expect("tags")
    }

    async fn cleanup(self) {
        let Harness {
            pool,
            database,
            admin_url,
            ..
        } = self;
        pool.close().await;

        if let Ok(admin) = PgPool::connect(&admin_url).await {
            let _ = admin
                .execute(AssertSqlSafe(format!(
                    r#"DROP DATABASE IF EXISTS "{database}" WITH (FORCE)"#
                )))
                .await;
            admin.close().await;
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    match url.rfind('/') {
        Some(index) => format!("{}/{database}", &url[..index]),
        None => format!("{url}/{database}"),
    }
}

#[tokio::test]
async fn links_resolve_and_produce_backlinks() {
    let harness = Harness::new("links").await;

    harness
        .write("Projects/Kitchen Reno.md", "Budget is in [[Budget]].\n")
        .await;
    harness.write("Budget.md", "Back to [[Kitchen Reno]].\n").await;

    assert_eq!(
        harness.outgoing("Projects/Kitchen Reno.md").await,
        vec![("Budget".to_string(), Some("Budget.md".to_string()))]
    );
    assert_eq!(
        harness.backlinks("Budget.md").await,
        vec!["Projects/Kitchen Reno.md"]
    );

    harness.cleanup().await;
}

/// Writing a link before its target exists is normal in a linked vault. The link
/// must be stored as broken and heal by itself once the note appears.
#[tokio::test]
async fn broken_links_heal_when_the_target_is_created() {
    let harness = Harness::new("heal").await;

    harness.write("Source.md", "See [[Not Yet Written]].\n").await;
    assert_eq!(
        harness.outgoing("Source.md").await,
        vec![("Not Yet Written".to_string(), None)],
        "the link should start out broken"
    );

    harness.write("Not Yet Written.md", "Here now.\n").await;
    assert_eq!(
        harness.outgoing("Source.md").await,
        vec![(
            "Not Yet Written".to_string(),
            Some("Not Yet Written.md".to_string())
        )],
        "creating the target should have healed the link"
    );

    harness.cleanup().await;
}

/// The operation people most need to trust: renaming a note must not break the
/// notes that referred to it.
#[tokio::test]
async fn renaming_rewrites_links_in_other_notes() {
    let harness = Harness::new("rename").await;

    harness.write("Budget.md", "# Budget\n").await;
    harness
        .write("Index.md", "Bare [[Budget]] and full [[Budget]].\n")
        .await;

    let from = harness.vault.resolve_note("Budget.md").unwrap();
    let to = harness.vault.resolve_note("Archive/Q3 Budget.md").unwrap();
    store::move_entry(&from, &to).await.expect("move");
    index::rename_note_row(&harness.pool, harness.user.id, "Budget.md", "Archive/Q3 Budget.md")
        .await
        .expect("rename row");
    let rewritten = index::rewrite_links_after_move(
        &harness.pool,
        &harness.user,
        &harness.vault,
        "Budget.md",
        "Archive/Q3 Budget.md",
    )
    .await
    .expect("rewrite");

    assert_eq!(rewritten, 1);
    assert_eq!(
        harness.read("Index.md").await,
        "Bare [[Q3 Budget]] and full [[Q3 Budget]].\n",
        "the file on disk should name the new title"
    );
    assert_eq!(
        harness.outgoing("Index.md").await,
        vec![
            ("Q3 Budget".to_string(), Some("Archive/Q3 Budget.md".to_string())),
            ("Q3 Budget".to_string(), Some("Archive/Q3 Budget.md".to_string())),
        ],
        "and the links should still resolve"
    );

    harness.cleanup().await;
}

/// Deleting a note must break the links pointing at it rather than leaving a
/// dangling reference to a row that no longer exists.
#[tokio::test]
async fn deleting_a_note_breaks_inbound_links() {
    let harness = Harness::new("delete").await;

    harness.write("Target.md", "# Target\n").await;
    harness.write("Source.md", "See [[Target]].\n").await;
    assert_eq!(harness.backlinks("Target.md").await, vec!["Source.md"]);

    index::remove_note(&harness.pool, harness.user.id, "Target.md")
        .await
        .expect("remove");

    assert_eq!(
        harness.outgoing("Source.md").await,
        vec![("Target".to_string(), None)],
        "the link should now be broken, not dangling"
    );

    harness.cleanup().await;
}

/// The claim the whole design rests on: the database is a cache, and the files
/// are the truth.
#[tokio::test]
async fn the_index_rebuilds_itself_from_the_filesystem() {
    let harness = Harness::new("rebuild").await;

    harness
        .write("A.md", "---\ntags: [alpha]\n---\n\nLinks to [[B]].\n")
        .await;
    harness.write("B.md", "Links back to [[A]].\n").await;

    assert_eq!(harness.note_count().await, 2);
    assert_eq!(harness.tags().await, vec!["alpha"]);

    // Wipe every derived table, exactly as the README invites the reader to do.
    harness
        .pool
        .execute("TRUNCATE notes, folders, tags, attachments CASCADE")
        .await
        .expect("truncate");
    assert_eq!(harness.note_count().await, 0);

    let report = index::reconcile_vault(&harness.pool, &harness.user, &harness.vault)
        .await
        .expect("reconcile");

    assert_eq!(report.added, 2);
    assert_eq!(harness.note_count().await, 2);
    assert_eq!(harness.tags().await, vec!["alpha"]);
    assert_eq!(
        harness.outgoing("A.md").await,
        vec![("B".to_string(), Some("B.md".to_string()))],
        "the link graph should be back too"
    );

    harness.cleanup().await;
}

/// A file that vanishes from disk must vanish from the index on the next scan.
#[tokio::test]
async fn reconcile_forgets_notes_whose_files_are_gone() {
    let harness = Harness::new("forget").await;

    harness.write("Stays.md", "# Stays\n").await;
    harness.write("Goes.md", "# Goes\n").await;
    assert_eq!(harness.note_count().await, 2);

    let doomed = harness.vault.resolve_note("Goes.md").unwrap();
    tokio::fs::remove_file(doomed.abs()).await.expect("unlink");

    let report = index::reconcile_vault(&harness.pool, &harness.user, &harness.vault)
        .await
        .expect("reconcile");

    assert_eq!(report.removed, 1);
    assert_eq!(harness.note_count().await, 1);

    harness.cleanup().await;
}

/// Two notes can share a filename. Resolution must be deterministic — the
/// shortest path wins — so a link does not flip between them between requests.
#[tokio::test]
async fn ambiguous_filenames_resolve_deterministically() {
    let harness = Harness::new("ambiguous").await;

    harness.write("Deep/Nested/Note.md", "# Deep\n").await;
    harness.write("Note.md", "# Shallow\n").await;
    harness.write("Source.md", "See [[Note]].\n").await;

    for _ in 0..3 {
        assert_eq!(
            harness.outgoing("Source.md").await,
            vec![("Note".to_string(), Some("Note.md".to_string()))],
            "the shortest path should win, every time"
        );
    }

    // Addressing the other one by its full path must still work.
    harness
        .write("Explicit.md", "See [[Deep/Nested/Note]].\n")
        .await;
    assert_eq!(
        harness.outgoing("Explicit.md").await,
        vec![(
            "Deep/Nested/Note".to_string(),
            Some("Deep/Nested/Note.md".to_string())
        )]
    );

    harness.cleanup().await;
}

/// Full-text search has to actually return rows, and the `tsvector` generated
/// column has to populate — neither of which any unit test can show.
#[tokio::test]
async fn full_text_search_finds_notes() {
    let harness = Harness::new("search").await;

    harness
        .write(
            "Kitchen.md",
            "---\ntitle: Kitchen Renovation\n---\n\nWe need new cabinets and a worktop.\n",
        )
        .await;
    harness.write("Unrelated.md", "Nothing to do with rooms.\n").await;

    let hits: Vec<String> = sqlx::query_scalar(
        "SELECT rel_path FROM notes, websearch_to_tsquery('english', $2) AS q
         WHERE user_id = $1 AND search @@ q
         ORDER BY ts_rank(search, q) DESC",
    )
    .bind(harness.user.id)
    .bind("cabinets")
    .fetch_all(&harness.pool)
    .await
    .expect("search");

    assert_eq!(hits, vec!["Kitchen.md"]);

    // The title comes from frontmatter, and is weighted above the body.
    let by_title: Vec<String> = sqlx::query_scalar(
        "SELECT rel_path FROM notes, websearch_to_tsquery('english', $2) AS q
         WHERE user_id = $1 AND search @@ q",
    )
    .bind(harness.user.id)
    .bind("renovation")
    .fetch_all(&harness.pool)
    .await
    .expect("search by title");

    assert_eq!(by_title, vec!["Kitchen.md"]);

    harness.cleanup().await;
}

/// Two users must not be able to see each other's notes, at the query level and
/// not merely because the handlers remember to filter.
#[tokio::test]
async fn vaults_are_isolated_between_users() {
    let harness = Harness::new("isolation").await;

    let other = db::find_or_create_user(
        &harness.pool,
        NewUser {
            username: "someone-else",
            display_name: "Someone Else",
            email: None,
            auth_provider: "local",
            auth_subject: "someone-else",
        },
    )
    .await
    .expect("second user");

    assert_ne!(
        other.vault_dir, harness.user.vault_dir,
        "each user needs their own directory"
    );

    harness.write("Private.md", "secret\n").await;

    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM notes WHERE user_id = $1")
        .bind(other.id)
        .fetch_one(&harness.pool)
        .await
        .expect("count");
    assert_eq!(visible, 0, "the other user should see nothing");

    harness.cleanup().await;
}

/// Sessions must be looked up by hash, expire, and be revocable.
#[tokio::test]
async fn sessions_expire_and_can_be_revoked() {
    use go_notes_server::auth::session;

    let harness = Harness::new("sessions").await;

    let token = session::create(
        &harness.pool,
        harness.user.id,
        chrono::Duration::days(30),
        Some("integration test"),
    )
    .await
    .expect("create session");

    let resolved = session::resolve(&harness.pool, &token, chrono::Duration::days(30))
        .await
        .expect("resolve");
    assert_eq!(resolved.map(|user| user.id), Some(harness.user.id));

    // A token that was never issued must not resolve.
    assert!(session::resolve(&harness.pool, "not-a-real-token", chrono::Duration::days(30))
        .await
        .expect("resolve unknown")
        .is_none());

    session::destroy(&harness.pool, &token)
        .await
        .expect("destroy");
    assert!(
        session::resolve(&harness.pool, &token, chrono::Duration::days(30))
            .await
            .expect("resolve after destroy")
            .is_none(),
        "a destroyed session must stop working immediately"
    );

    // An already-expired session is not returned, and is purged.
    let expired = session::create(
        &harness.pool,
        harness.user.id,
        chrono::Duration::seconds(-60),
        None,
    )
    .await
    .expect("create expired");
    assert!(
        session::resolve(&harness.pool, &expired, chrono::Duration::days(30))
            .await
            .expect("resolve expired")
            .is_none()
    );
    assert!(session::purge_expired(&harness.pool).await.expect("purge") >= 1);

    harness.cleanup().await;
}
