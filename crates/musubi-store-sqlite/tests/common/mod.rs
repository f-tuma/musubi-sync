//! Helpers shared by the integration tests: a temporary directory and fixtures.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jiff::{SignedDuration, Timestamp};
use musubi_model::provider::{Access, Pass, RemoteCollection};
use musubi_model::remote::MediaType;
use musubi_model::store::DiscoveryWrite;
use musubi_model::store::{
    Correlation, ItemWrite, OccurrenceRow, Occurrences, PageWrite, SeriesSpan,
};
use musubi_model::{
    AccountId, CollectionId, CollectionRef, EntityKind, Raw, RemoteKey, Storage, TenantId,
    TenantStore, Validator,
};
use musubi_store_sqlite::{SqliteStorage, SqliteTenant};
use serde_json::value::RawValue;

/// A directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "musubi-store-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            Timestamp::now().as_nanosecond()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub const ACCOUNT: AccountId = AccountId::from_u128(0xacc);
pub const TENANT: TenantId = TenantId::from_u128(0x7e);

/// `h` hours after 2026-01-05T00:00Z.
pub fn t(h: i64) -> Timestamp {
    "2026-01-05T00:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_hours(h)
}

/// A storage with one created tenant.
pub fn storage(dir: &Path) -> SqliteStorage {
    let s = SqliteStorage::new(dir, 8).unwrap();
    s.create_tenant(TENANT).unwrap();
    s
}

pub fn remote(name: &str) -> RemoteCollection {
    RemoteCollection::new(
        CollectionRef(name.into()),
        EntityKind::Event,
        name,
        Access::Owner,
    )
}

/// Discovers one collection `name` and returns its id.
pub fn collection(t: &mut SqliteTenant, name: &str) -> CollectionId {
    t.apply_discovery(&DiscoveryWrite {
        account: ACCOUNT,
        collections: vec![remote(name)],
        complete: false,
    })
    .unwrap();
    t.collections()
        .unwrap()
        .into_iter()
        .find(|c| c.remote.remote.0 == name)
        .unwrap()
        .id
}

/// A one-hour event at hour `h` with content `version`.
pub fn event(key: &str, version: &str, h: i64) -> ItemWrite {
    item(
        key,
        version,
        Occurrences {
            rows: vec![row(h, None)],
            series: None,
        },
    )
}

pub fn row(h: i64, rid: Option<i64>) -> OccurrenceRow {
    OccurrenceRow {
        start: t(h),
        end: t(h + 1),
        recurrence_id: rid.map(t),
        floating: false,
    }
}

/// A daily series from hour `first`, indexed for `indexed` days, ending after `days` (or never).
pub fn series(key: &str, first: i64, indexed: i64, days: Option<i64>) -> ItemWrite {
    let rows = (0..indexed)
        .map(|d| row(first + 24 * d, Some(first + 24 * d)))
        .collect();
    let mut w = item(
        key,
        "1",
        Occurrences {
            rows,
            series: Some(SeriesSpan {
                first: t(first),
                last: days.map(|d| t(first + 24 * (d - 1) + 1)),
                indexed_until: t(first + 24 * indexed),
            }),
        },
    );
    w.correlation = Some(Correlation {
        uid: key.into(),
        recurrence_id: None,
    });
    w
}

pub fn item(key: &str, version: &str, occurrences: Occurrences) -> ItemWrite {
    let body = format!(r#"{{"id":"{key}","v":"{version}"}}"#);
    let raw = Raw {
        media_type: MediaType::Json,
        body: body.clone().into_bytes(),
    };
    ItemWrite {
        key: RemoteKey(key.into()),
        validator: Validator(format!("{key}@{version}")),
        raw_hash: raw.hash(),
        raw,
        kind: EntityKind::Event,
        canonical: Some(RawValue::from_string(body).unwrap()),
        schema_version: 1,
        mapping_version: 1,
        correlation: None,
        occurrences,
    }
}

pub fn page(col: CollectionId, upserts: Vec<ItemWrite>) -> PageWrite {
    PageWrite {
        collection: col,
        pass: Pass::Delta,
        read: None,
        last: false,
        cursor: None,
        upserts,
        unchanged: Vec::new(),
        removals: Vec::new(),
    }
}

pub fn keys(k: &[&str]) -> Vec<RemoteKey> {
    k.iter().map(|k| RemoteKey((*k).into())).collect()
}

/// The whole journal after `since`, read page by page (`changes` caps a page).
pub fn journal(
    tn: &SqliteTenant,
    mut since: musubi_model::Seq,
) -> Vec<musubi_model::journal::Change> {
    let mut all = Vec::new();
    loop {
        let p = tn.changes(since, u32::MAX).unwrap();
        all.extend(p.changes);
        since = p.next;
        if !p.more {
            return all;
        }
    }
}
