//! Dev-only: seed a store with a realistic project rail + an asserted design
//! tree, so the app can be screenshotted populated instead of empty.
//!
//! WHERE IT WRITES — read this before running it. The target is
//! `$KOD_STORE_DIR`, or `$HOME/Library/Application Support/orchestrator` when
//! that is unset. The default IS your real application database. To seed a
//! throwaway instead (which is what you want for screenshots):
//!
//!     KOD_STORE_DIR=/tmp/kod-demo cargo run -p orchestrator-store --example seed_demo
//!     HOME=/tmp/kod-demo-home cargo run -p orchestrator-gui
//!
//! The path is printed before anything is written, so a mistake is visible
//! rather than silent.
use orchestrator_store::{DiffOp, Lifecycle, PartRef, StatusSource, Store};

fn main() {
    let dir = std::env::var("KOD_STORE_DIR").unwrap_or_else(|_| {
        std::env::var("HOME").expect("HOME unset and KOD_STORE_DIR unset")
            + "/Library/Application Support/orchestrator"
    });
    std::fs::create_dir_all(&dir).unwrap();
    // store.db, NOT design.db. The store outgrew the design tree and was renamed;
    // the GUI migrates a legacy design.db across ONLY when store.db is absent, so
    // seeding the old name on an installed app wrote a file that was then ignored
    // forever — the seed appeared to succeed and nothing showed up.
    let path = format!("{dir}/store.db");
    println!("seeding: {path}");
    let mut s = Store::open(std::path::Path::new(&path)).unwrap();

    // A rail with a few projects in it. Path keys, because these are stand-ins
    // for checked-out directories rather than git remotes.
    for (slug, name) in [
        ("path:/Users/me/code/atlas", "atlas"),
        ("path:/Users/me/code/harbor", "harbor"),
        ("path:/Users/me/code/ledger", "ledger"),
        ("path:/Users/me/code/beacon", "beacon"),
    ] {
        s.ensure_project(slug, name).unwrap();
    }

    let key = "path:/Users/me/code/atlas";
    let add = |t: &str, name: &str, detail: &str, anchor: &str| DiffOp::Add {
        temp: t.into(),
        parent: PartRef::Root,
        name: name.into(),
        detail: detail.into(),
        lifecycle: Lifecycle::Todo,
        anchors: vec![anchor.into()],
        kind: orchestrator_store::Kind::Area,
        detail_md: None,
        sort_order: None,
        source_file: None,
        source_quote: None,
        rationale: None,
    };
    s.accept_diff(
        key,
        &[
            add("a", "Identity registry", "project list, activity, DID", "crates/core/**"),
            add("b", "Terminal host", "PTY + emulator + hosted CLIs", "crates/host/**"),
            add("c", "Decision loop", "permission cards", "crates/host/src/decision.rs"),
            add("d", "Flow map", "store + extraction", "crates/store/**"),
            add("e", "GUI shell", "Standup + Workspace", "crates/gui/**"),
        ],
    )
    .unwrap();

    // assert realistic statuses (the user's calls)
    let parts = s.load_tree(key).unwrap();
    let id = |n: &str| parts.iter().find(|p| p.name.starts_with(n)).unwrap().id;
    for (n, lc) in [
        ("Identity", Lifecycle::Done),
        ("Terminal", Lifecycle::Done),
        ("Decision", Lifecycle::Done),
        ("Flow", Lifecycle::Building),
        ("GUI", Lifecycle::Building),
    ] {
        s.accept_diff(
            key,
            &[DiffOp::SetStatus {
                id: id(n),
                lifecycle: lc,
                source: StatusSource::User,
            }],
        )
        .unwrap();
    }
    println!("seeded 4 projects; atlas has {} parts", s.load_tree(key).unwrap().len());
}
