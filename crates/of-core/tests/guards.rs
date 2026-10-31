//! Source-level guard for a call that compiles fine and must never be made.
//!
//! `otto_tenant::Db::migrate` is a perfectly reasonable method that is wrong
//! *for this deployment*: it would apply otto-platform's own migration history
//! to this database, and the only thing between a caller and the mistake is that
//! nobody reaches for it. A test that reads the source is crude but it is the one
//! thing that fails when someone does. Go through `of_core::migrate`.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name != "target" && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `.rs` file in the workspace's `crates/` directory, with its text.
fn workspace_sources() -> Vec<(PathBuf, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_sources(&crates, &mut files);
    files
        .into_iter()
        .map(|p| {
            let text = fs::read_to_string(&p).expect("read source");
            (p, text)
        })
        .collect()
}

/// Lines of `text` that are code, not comments.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
}

#[test]
fn nothing_runs_the_platforms_migrations_on_this_database() {
    let this_file = Path::new(file!()).file_name().unwrap().to_owned();
    let mut offenders = Vec::new();

    for (path, text) in workspace_sources() {
        if path.file_name() == Some(&this_file) {
            continue;
        }
        for (n, line) in code_lines(&text) {
            if line.contains(".migrate()") || line.contains("otto_tenant::db::MIGRATOR") {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "this database has its own migration history (`of_core::migrate`, \
         `of_core::MIGRATOR`); otto-platform's would collide with it:\n{}",
        offenders.join("\n")
    );
}

/// The factory's database is domain-only: identity, auth, and billing belong to
/// the Savvagent platform's database, and nothing here may grow a foreign key into
/// them. A `REFERENCES orgs (...)` would compile, pass every test against a
/// single database, and make the cutover to a separate platform database
/// impossible; a `CREATE TABLE users` would quietly start a second source of
/// truth for who someone is.
#[test]
fn the_schema_holds_no_identity_tables_and_no_foreign_keys_to_them() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut offenders = Vec::new();
    let mut files = 0;

    for entry in fs::read_dir(&dir).expect("migrations dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "sql") {
            continue;
        }
        files += 1;
        let text = fs::read_to_string(&path).expect("read migration");

        for (n, line) in code_lines_sql(&text) {
            let lower = line.to_lowercase();
            for table in [
                "users",
                "orgs",
                "teams",
                "org_members",
                "team_members",
                "org_invites",
                "access_tokens",
                "refresh_tokens",
                "oauth_clients",
                "authorization_codes",
                "sessions",
                "passkeys",
                "idp_connections",
                "claimed_domains",
                "user_identities",
                "resource_servers",
                "usage_events",
                "org_period_usage",
                "subscriptions",
            ] {
                let creates = lower.contains(&format!("create table {table} "))
                    || lower.contains(&format!("create table {table}("));
                let references = lower.contains(&format!("references {table} "))
                    || lower.contains(&format!("references {table}("))
                    || lower.contains(&format!("references public.{table}"));
                if creates || references {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
    }

    assert!(files > 0, "found no migrations to check");
    assert!(
        offenders.is_empty(),
        "this database is domain-only; identity lives in the platform's database:\n{}",
        offenders.join("\n")
    );
}

/// SQL lines that are code, not `--` comments.
fn code_lines_sql(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("--"))
}
