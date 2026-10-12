// `sqlx::migrate!` embeds the migration dir at compile time; rebuild when a
// migration is added (sqlx's recommended build script).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
