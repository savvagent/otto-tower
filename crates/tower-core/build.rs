// `sqlx::migrate!` embeds the migration files at compile time but does not tell
// cargo to watch the directory, so a new migration file would not rebuild the
// crate and a local test run would use the stale embedded migrator (surfacing
// as, say, `column "meter" does not exist`). CI builds from scratch and never
// noticed.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
