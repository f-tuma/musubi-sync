//! `kill -9` of a process in the middle of an `apply_page` loop (plan K1).
//!
//! The test binary re-runs itself as the child (`crash_child`, a no-op unless
//! `MUSUBI_CRASH_DIR` is set). `MUSUBI_CRASH_ROUNDS` overrides the 200 rounds
//! (the nightly job runs more).

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use musubi_model::{Cursor, Seq, Storage, TenantStore};
use musubi_store_sqlite::{SqliteStorage, SqliteTenant};

const PER_PAGE: u64 = 10;

/// Page number stored as the collection cursor (0 before the first page).
fn last_page(tn: &SqliteTenant) -> u64 {
    let c = tn.collections().unwrap().pop().unwrap();
    c.cursor
        .map_or(0, |c| String::from_utf8(c.0).unwrap().parse().unwrap())
}

/// The child: writes pages `c+1, c+2, …` until killed. Page `n` creates
/// `PER_PAGE` items `p{n}-{i}`, updates item `shared` to version `n` and moves
/// the cursor to `n`, all in one `apply_page`.
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var("MUSUBI_CRASH_DIR") else {
        return;
    };
    let s = SqliteStorage::new(Path::new(&dir), 1).unwrap();
    let mut tn = s.open(TENANT).unwrap();
    let col = tn.collections().unwrap()[0].id;
    let mut n = last_page(&tn);
    // libtest prints "test crash_child ... " before this, without a newline.
    println!("ready");
    loop {
        n += 1;
        let mut items: Vec<_> = (0..PER_PAGE)
            .map(|i| event(&format!("p{n}-{i}"), "1", 1))
            .collect();
        items.push(event("shared", &n.to_string(), 0));
        let mut p = page(col, items);
        p.cursor = Some(Cursor(n.to_string().into_bytes()));
        tn.apply_page(&p).unwrap();
    }
}

#[test]
fn kill_mid_apply_page() {
    let rounds: u32 = std::env::var("MUSUBI_CRASH_ROUNDS").map_or(200, |r| r.parse().unwrap());
    let d = TempDir::new();
    {
        let s = storage(d.path());
        collection(&mut s.open(TENANT).unwrap(), "a");
    }
    let exe = std::env::current_exe().unwrap();
    let mut rng = jiff::Timestamp::now().as_nanosecond() as u64 | 1;
    let (mut head, mut pages) = (Seq(0), 0);
    for round in 0..rounds {
        let mut child = Command::new(&exe)
            .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
            .env("MUSUBI_CRASH_DIR", d.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        assert!(
            out.lines().any(|l| l.unwrap().ends_with("ready")),
            "child did not start"
        );
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        std::thread::sleep(Duration::from_millis(1 + (rng >> 33) % 30));
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();

        let s = SqliteStorage::new(d.path(), 1).unwrap();
        let tn = s.open(TENANT).unwrap();
        let c = last_page(&tn);
        let st = tn.stats().unwrap();
        let want = PER_PAGE * c + u64::from(c > 0);
        assert_eq!(
            (st.items, st.entities),
            (want, want),
            "round {round}: cursor {c}"
        );
        assert!(c >= pages && st.head >= head, "round {round}: went back");
        let new = tn.changes(head, u32::MAX).unwrap().changes;
        assert!(new.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(new.iter().all(|ch| ch.seq > head && ch.seq <= st.head));
        (head, pages) = (st.head, c);
    }

    // Every page up to the cursor is there once, and nothing after it.
    let s = SqliteStorage::new(d.path(), 1).unwrap();
    let tn = s.open(TENANT).unwrap();
    let col = tn.collections().unwrap()[0].id;
    let mut all = std::collections::HashSet::new();
    let mut after = None;
    loop {
        let p = tn
            .unseen(col, musubi_model::ReadId(u64::MAX), after.as_ref(), 10_000)
            .unwrap();
        let Some(last) = p.last().cloned() else { break };
        for k in p {
            assert!(all.insert(k.0), "duplicate key");
        }
        after = Some(last);
    }
    let mut want: std::collections::HashSet<_> = (1..=pages)
        .flat_map(|n| (0..PER_PAGE).map(move |i| format!("p{n}-{i}")))
        .collect();
    if pages > 0 {
        want.insert("shared".into());
    }
    assert_eq!(all, want);
    assert!(
        pages > u64::from(rounds) / 2,
        "children made too little progress: {pages} pages"
    );
    println!("{rounds} kills, {pages} whole pages, head {head:?}");
}
