//! `musubi-dav` with profile `icloud` against the fake DAV server in its
//! `icloud` mode (plan, checkpoint K7; the facts are T11's,
//! docs/research/phase2/live-icloud.md).
//!
//! The fake listens on `http://127.0.0.1`, which iCloud's host pattern never
//! matches, so these tests confirm the fake's hosts through `allow_origins`.
//! The pattern itself (`Profile::host_allowed`, with `:443`) is proven only
//! by unit tests in `profile.rs` and `net.rs`, not end to end.

use futures_util::StreamExt;
use jiff::Timestamp;
use musubi_dav::{ConnectError, DavConfig, DavProvider, DavSetup, Profile};
use musubi_model::provider::{Access, Discovery, FetchHint, Start};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, ChangePage, CollectionRef, Credential, Cursor, EntityKind, Provider,
    ReauthReason, RemoteKey, SyncError,
};
use musubi_testkit::dav::{Config, FakeDav, Host, Kind, Profile as Server};
use secrecy::SecretString;

fn event(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//t//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20261101T000000Z\r\nDTSTART:20261113T100000Z\r\nDTEND:20261113T110000Z\r\n\
         SUMMARY:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn contact(uid: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{uid}\r\nN:{uid};;;;\r\nEND:VCARD\r\n")
}

/// An Apple contact group: vCard 3.0 with `X-ADDRESSBOOKSERVER-KIND:group`.
fn group(uid: &str, member: &str) -> String {
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:Family\r\nN:Family;;;;\r\n\
         X-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:{member}\r\n\
         END:VCARD\r\n"
    )
}

async fn fake() -> FakeDav {
    FakeDav::start(Config {
        profile: Server::Icloud,
        ..Config::default()
    })
    .await
    .unwrap()
}

fn icloud() -> DavProvider {
    DavProvider::with_profile(
        DavConfig {
            allow_private: true,
        },
        Profile::Icloud,
    )
    .unwrap()
}

/// The fake's entry hosts in place of `caldav.icloud.com` and
/// `contacts.icloud.com`, and its partition hosts when `partitions`.
fn setup(dav: &FakeDav, partitions: bool) -> DavSetup {
    DavSetup {
        caldav_url: Some(dav.server_url()),
        carddav_url: Some(dav.carddav_url()),
        allow_origins: if partitions {
            vec![
                dav.url(Host::CaldavPartition),
                dav.url(Host::CarddavPartition),
            ]
        } else {
            Vec::new()
        },
        ..DavSetup::default()
    }
}

fn cx(setup: &DavSetup, password: &str) -> AccountCx {
    let mut cx = musubi_testkit::account_cx(AccountId::from_u128(7), Timestamp::now());
    cx.credential = Credential::Basic {
        username: "alice".into(),
        password: SecretString::from(password),
    };
    cx.endpoint = Some(setup.endpoint_for(Profile::Icloud).unwrap());
    cx
}

async fn connected(p: &DavProvider, dav: &FakeDav) -> (AccountCx, Discovery) {
    let mut cx = cx(&setup(dav, true), "secret");
    let d = p.connect(&cx).await.unwrap();
    cx.endpoint = d.endpoint.clone();
    (cx, d)
}

async fn read(
    p: &DavProvider,
    cx: &AccountCx,
    key: &str,
    cursor: Option<Cursor>,
) -> Result<Vec<ChangePage>, SyncError> {
    let c = CollectionRef(key.to_owned());
    p.changes(cx, &c, cursor, FetchHint::new(Start::Full))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

fn keys(pages: &[ChangePage]) -> Vec<String> {
    let mut k: Vec<String> = pages
        .iter()
        .flat_map(|p| p.stubs.iter().map(|s| s.key.0.clone()))
        .collect();
    k.sort();
    k
}

fn rref(col: &str, key: String) -> RemoteRef {
    RemoteRef {
        collection: CollectionRef(col.to_owned()),
        key: RemoteKey(key),
    }
}

#[tokio::test]
async fn discovers_and_reads_across_partition_hosts() {
    let dav = fake().await;
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let tasks = dav.add_collection(Kind::Calendar(&["VTODO"]), "tasks", "Reminders");
    let book = dav.add_collection(Kind::AddressBook, "card", "Contacts");
    dav.put(&cal, "e.ics", &event("e"));
    dav.put(&book, "a.vcf", &contact("a"));
    dav.put(&book, "g.vcf", &group("g", "a"));
    let p = icloud();

    let (cx, d) = connected(&p, &dav).await;
    let mut found: Vec<&str> = d.collections.iter().map(|c| c.remote.0.as_str()).collect();
    found.sort();
    // VTODO-only, inbox, outbox and notification are skipped.
    assert_eq!(found, [&book, &cal]);
    assert!(!found.contains(&tasks.as_str()));
    for c in &d.collections {
        // T11: iCloud reports write privileges on both.
        assert_eq!(c.access, Access::Writer, "{}", c.remote.0);
    }
    // Calendar and address book homes are on their own partition hosts.
    let hosts = |path: &str| -> Vec<Host> {
        dav.requests()
            .iter()
            .filter(|r| r.path == path && r.method == "PROPFIND")
            .map(|r| r.host)
            .collect()
    };
    assert_eq!(hosts(&dav.calendar_home()), [Host::CaldavPartition]);
    assert_eq!(hosts(&dav.addressbook_home()), [Host::CarddavPartition]);

    // Reads: the collection itself in the initial sync-collection (CardDAV
    // without its slash) is not an object.
    let pages = read(&p, &cx, &cal, None).await.unwrap();
    assert_eq!(keys(&pages), [format!("{cal}e.ics")]);
    let pages = read(&p, &cx, &book, None).await.unwrap();
    let (a, g) = (format!("{book}a.vcf"), format!("{book}g.vcf"));
    assert_eq!(keys(&pages), [a.clone(), g.clone()]);
    assert!(
        dav.requests()
            .iter()
            .any(|r| r.host == Host::CarddavPartition && r.label() == "REPORT sync-collection")
    );

    let objs = p
        .fetch(
            &cx,
            &[
                rref(&book, a.clone()),
                rref(&book, g.clone()),
                rref(&cal, format!("{cal}e.ics")),
            ],
        )
        .await
        .unwrap();
    let kind = |k: &str| objs.iter().find(|o| o.key.0 == k).unwrap().kind;
    assert_eq!(kind(&a), EntityKind::Contact);
    assert_eq!(kind(&g), EntityKind::ContactGroup);
    assert_eq!(kind(&format!("{cal}e.ics")), EntityKind::Event);
    assert_eq!(
        objs.iter().find(|o| o.key.0 == g).unwrap().raw.body,
        group("g", "a").as_bytes()
    );
}

#[tokio::test]
async fn partition_hosts_outside_the_allowed_origins_are_refused() {
    let dav = fake().await;
    dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let e = icloud()
        .connect(&cx(&setup(&dav, false), "secret"))
        .await
        .unwrap_err();
    let ConnectError::CrossOriginRedirect(target) = e else {
        panic!("{e:?}")
    };
    assert!(
        target.starts_with(&dav.url(Host::CaldavPartition)),
        "{target}"
    );
    assert!(
        dav.requests()
            .iter()
            .all(|r| !matches!(r.host, Host::CaldavPartition | Host::CarddavPartition)),
        "nothing went to the partition hosts"
    );
}

#[tokio::test]
async fn invalid_tokens_are_cursor_expired_on_both() {
    let dav = fake().await;
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let book = dav.add_collection(Kind::AddressBook, "card", "Contacts");
    dav.put(&cal, "e.ics", &event("e"));
    dav.put(&book, "a.vcf", &contact("a"));
    let p = icloud();
    let (cx, _) = connected(&p, &dav).await;
    for col in [&cal, &book] {
        let pages = read(&p, &cx, col, None).await.unwrap();
        let cursor = pages.last().unwrap().cursor.clone().unwrap();
        dav.expire_sync_tokens(col);
        dav.clear_requests();
        let e = read(&p, &cx, col, Some(cursor)).await.unwrap_err();
        assert!(matches!(e, SyncError::CursorExpired), "{col}: {e:?}");
    }
}

#[tokio::test]
async fn limits_and_multiget_batches_of_20() {
    let dav = fake().await;
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let book = dav.add_collection(Kind::AddressBook, "card", "Contacts");
    for i in 0..45 {
        dav.put(&cal, &format!("{i}.ics"), &event(&format!("e{i}")));
    }
    dav.put(&book, "a.vcf", &contact("a"));
    let p = icloud();
    let d = p.descriptor();
    assert_eq!(d.id.0, "icloud");
    assert_eq!(d.limits.max_batch, Some(20));
    assert_eq!(d.limits.max_concurrency_per_account, Some(2));
    let kinds: Vec<_> = d.kinds.iter().map(|k| k.kind).collect();
    assert_eq!(
        kinds,
        [
            EntityKind::Event,
            EntityKind::Contact,
            EntityKind::ContactGroup
        ],
        "iCloud tasks are not in v1"
    );

    let (cx, _) = connected(&p, &dav).await;
    let mut refs: Vec<RemoteRef> = (0..45)
        .map(|i| rref(&cal, format!("{cal}{i}.ics")))
        .collect();
    refs.push(rref(&book, format!("{book}a.vcf")));
    dav.clear_requests();
    let objs = p.fetch(&cx, &refs).await.unwrap();
    assert_eq!(objs.len(), 46);
    assert_eq!(dav.count("REPORT calendar-multiget"), 3);
    // The fake answers 400 to an address book multiget naming the collection.
    assert_eq!(dav.count("REPORT addressbook-multiget"), 1);
}

#[tokio::test]
async fn rejected_password_asks_for_a_new_app_specific_password() {
    let dav = fake().await;
    dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let p = icloud();
    let c = cx(&setup(&dav, true), "wrong");
    let e = p.connect(&c).await.unwrap_err();
    assert!(matches!(e, ConnectError::Unauthorized), "{e:?}");
    let e = p.discover(&c).await.unwrap_err();
    assert!(matches!(
        e,
        SyncError::ReauthRequired {
            reason: ReauthReason::Rejected
        }
    ));
    let hint = Profile::Icloud.reauth_hint().unwrap();
    assert!(hint.contains("app-specific password"), "{hint}");
    assert_eq!(Profile::Generic.reauth_hint(), None);
}

#[tokio::test]
async fn an_instance_reads_only_endpoints_of_its_profile() {
    let dav = fake().await;
    let generic = DavProvider::new(DavConfig {
        allow_private: true,
    })
    .unwrap();
    let e = generic
        .connect(&cx(&setup(&dav, true), "secret"))
        .await
        .unwrap_err();
    assert!(matches!(e, ConnectError::InvalidConfig(_)), "{e:?}");
    let mut c = cx(&setup(&dav, true), "secret");
    c.endpoint = Some(setup(&dav, true).endpoint().unwrap());
    let e = icloud().connect(&c).await.unwrap_err();
    assert!(matches!(e, ConnectError::InvalidConfig(_)), "{e:?}");

    // The fixed entries.
    let s = DavSetup::icloud();
    assert_eq!(s.caldav_url.as_deref(), Some("https://caldav.icloud.com/"));
    assert_eq!(
        s.carddav_url.as_deref(),
        Some("https://contacts.icloud.com/")
    );
    assert!(s.endpoint_for(Profile::Icloud).is_ok());
}
