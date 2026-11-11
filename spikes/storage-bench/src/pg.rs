//! Shared PostgreSQL: every table keyed by tenant_id, one writer connection per tenant,
//! multi-row upserts via unnest(), statements pipelined inside one transaction.

use crate::datagen::{MAX_DUR, NCOLL, Page};
use futures_util::future::try_join_all;
use std::collections::HashMap;
use tokio_postgres::{Client, NoTls, Statement, types::ToSql};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tenants(id bigint PRIMARY KEY, seq bigint NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS collections(tenant_id bigint NOT NULL, id bigint NOT NULL, remote_ref text NOT NULL, cursor text,
  PRIMARY KEY(tenant_id, id));
CREATE TABLE IF NOT EXISTS raw(tenant_id bigint NOT NULL, hash bytea NOT NULL, body text NOT NULL, PRIMARY KEY(tenant_id, hash));
CREATE TABLE IF NOT EXISTS items(
  tenant_id bigint NOT NULL, id bigint NOT NULL, collection_id bigint NOT NULL, remote_key text NOT NULL,
  validator text NOT NULL, raw_ref bytea NOT NULL, observed text NOT NULL, entity_id bigint NOT NULL,
  rev bigint NOT NULL, seq bigint NOT NULL, tombstone boolean NOT NULL DEFAULT false,
  PRIMARY KEY(tenant_id, id), UNIQUE(tenant_id, collection_id, remote_key)) WITH (fillfactor = 90);
CREATE TABLE IF NOT EXISTS entities(
  tenant_id bigint NOT NULL, id bigint NOT NULL, kind smallint NOT NULL, canonical text, rev bigint NOT NULL,
  seq bigint NOT NULL, tombstone boolean NOT NULL DEFAULT false, PRIMARY KEY(tenant_id, id)) WITH (fillfactor = 90);
CREATE TABLE IF NOT EXISTS placements(
  tenant_id bigint NOT NULL, id bigint NOT NULL, entity_id bigint NOT NULL, item_id bigint NOT NULL,
  role smallint NOT NULL, direction smallint NOT NULL DEFAULT 0, projection smallint NOT NULL DEFAULT 0,
  origin smallint NOT NULL DEFAULT 0, delivered text, local text, delivery smallint NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant_id, id), UNIQUE(tenant_id, item_id));
CREATE INDEX IF NOT EXISTS placements_entity ON placements(tenant_id, entity_id);
CREATE TABLE IF NOT EXISTS occurrences(
  tenant_id bigint NOT NULL, collection_id bigint NOT NULL, start_utc bigint NOT NULL, end_utc bigint NOT NULL,
  entity_id bigint NOT NULL, item_id bigint NOT NULL, recurrence_id bigint NOT NULL);
CREATE INDEX IF NOT EXISTS occ_range ON occurrences(tenant_id, collection_id, start_utc);
CREATE INDEX IF NOT EXISTS occ_item ON occurrences(tenant_id, item_id);
CREATE TABLE IF NOT EXISTS changes(tenant_id bigint NOT NULL, entity_id bigint NOT NULL, seq bigint NOT NULL, op smallint NOT NULL,
  PRIMARY KEY(tenant_id, entity_id), UNIQUE(tenant_id, seq));
CREATE TABLE IF NOT EXISTS mutations(
  tenant_id bigint NOT NULL, id bigint GENERATED ALWAYS AS IDENTITY, entity_id bigint NOT NULL, base_rev bigint NOT NULL,
  patch text NOT NULL, idem_key text NOT NULL, PRIMARY KEY(tenant_id, id), UNIQUE(tenant_id, idem_key));
CREATE TABLE IF NOT EXISTS outbox(
  tenant_id bigint NOT NULL, id bigint GENERATED ALWAYS AS IDENTITY, placement_id bigint NOT NULL,
  mutation_id bigint NOT NULL, state smallint NOT NULL, payload text NOT NULL, PRIMARY KEY(tenant_id, id));
"#;

pub type Res<T> = Result<T, tokio_postgres::Error>;

/// `synced` connections run with synchronous_commit=off (the PostgreSQL analogue of
/// SQLite synchronous=NORMAL: a crash may lose the last commits, never corrupt them).
pub async fn connect(url: &str, synced: bool) -> Res<Client> {
    let url = if synced { format!("{url} options='-c synchronous_commit=off'") } else { url.to_string() };
    let (client, conn) = tokio_postgres::connect(&url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("pg connection error: {e}");
        }
    });
    Ok(client)
}

pub async fn create_tenant(c: &Client, tenant: i64) -> Res<()> {
    c.execute("INSERT INTO tenants(id) VALUES ($1) ON CONFLICT DO NOTHING", &[&tenant]).await?;
    let ids: Vec<i64> = (0..NCOLL).collect();
    let refs: Vec<String> = ids.iter().map(|c| format!("https://calendar.example.com/c/{c}")).collect();
    c.execute(
        "INSERT INTO collections(tenant_id, id, remote_ref) SELECT $1, * FROM unnest($2::bigint[], $3::text[]) ON CONFLICT DO NOTHING",
        &[&tenant, &ids, &refs],
    )
    .await?;
    Ok(())
}

pub struct Writer {
    pub c: Client,
    tenant: i64,
    pub seq: i64,
    next_id: i64,
    st: HashMap<&'static str, Statement>,
}

const STMTS: &[(&str, &str)] = &[
    ("links", "SELECT remote_key, entity_id FROM items WHERE tenant_id=$1 AND collection_id=0 AND remote_key = ANY($2::text[])"),
    (
        "items",
        "INSERT INTO items(tenant_id, id, collection_id, remote_key, validator, raw_ref, observed, entity_id, rev, seq)
         SELECT $1, u.id, $2, u.k, u.v, u.r, u.o, u.e, 1, u.s
         FROM unnest($3::bigint[], $4::text[], $5::text[], $6::bytea[], $7::text[], $8::bigint[], $9::bigint[]) AS u(id, k, v, r, o, e, s)
         ON CONFLICT (tenant_id, collection_id, remote_key) DO UPDATE
           SET validator=excluded.validator, raw_ref=excluded.raw_ref, observed=excluded.observed, rev=items.rev+1, seq=excluded.seq
           WHERE items.validator <> excluded.validator
         RETURNING id, entity_id, remote_key, (xmax = 0) AS inserted",
    ),
    ("raw", "INSERT INTO raw(tenant_id, hash, body) SELECT $1, * FROM unnest($2::bytea[], $3::text[]) ON CONFLICT DO NOTHING"),
    ("ins_ent", "INSERT INTO entities(tenant_id, id, kind, canonical, rev, seq) SELECT $1, u.id, 0, NULL, 1, u.s FROM unnest($2::bigint[], $3::bigint[]) AS u(id, s)"),
    (
        "upd_ent",
        "UPDATE entities e SET canonical=coalesce(u.c, e.canonical), rev=e.rev+1, seq=u.s
         FROM unnest($2::bigint[], $3::bigint[], $4::text[]) AS u(id, s, c) WHERE e.tenant_id=$1 AND e.id=u.id",
    ),
    (
        "ins_pl",
        "INSERT INTO placements(tenant_id, id, entity_id, item_id, role) SELECT $1, * FROM unnest($2::bigint[], $3::bigint[], $4::bigint[], $5::int2[])",
    ),
    ("del_occ", "DELETE FROM occurrences WHERE tenant_id=$1 AND item_id = ANY($2::bigint[])"),
    (
        "ins_occ",
        "INSERT INTO occurrences(tenant_id, collection_id, start_utc, end_utc, entity_id, item_id, recurrence_id)
         SELECT $1, $2, * FROM unnest($3::bigint[], $4::bigint[], $5::bigint[], $6::bigint[], $7::bigint[])",
    ),
    (
        "journal",
        "INSERT INTO changes(tenant_id, entity_id, seq, op) SELECT $1, * FROM unnest($2::bigint[], $3::bigint[], $4::int2[])
         ON CONFLICT (tenant_id, entity_id) DO UPDATE SET seq=excluded.seq, op=excluded.op",
    ),
    ("cursor", "UPDATE collections SET cursor=$3 WHERE tenant_id=$1 AND id=$2"),
    ("seq", "UPDATE tenants SET seq=$2 WHERE id=$1"),
    // Outbox path.
    ("m_rev", "SELECT rev FROM entities WHERE tenant_id=$1 AND id=$2"),
    (
        "m_ins",
        "WITH m AS (INSERT INTO mutations(tenant_id, entity_id, base_rev, patch, idem_key) VALUES ($1,$2,$3,$4,$5) RETURNING id)
         INSERT INTO outbox(tenant_id, placement_id, mutation_id, state, payload)
         SELECT $1, p.id, m.id, 0, $4 FROM m, placements p WHERE p.tenant_id=$1 AND p.entity_id=$2",
    ),
    ("m_ent", "UPDATE entities SET rev=rev+1, seq=$3 WHERE tenant_id=$1 AND id=$2"),
    (
        "m_journal",
        "INSERT INTO changes(tenant_id, entity_id, seq, op) VALUES ($1,$2,$3,2) ON CONFLICT (tenant_id, entity_id) DO UPDATE SET seq=excluded.seq, op=2",
    ),
];

impl Writer {
    /// Prepares statements once per connection; `use_tenant` switches the tenant.
    pub async fn new(c: Client) -> Res<Self> {
        let mut st = HashMap::new();
        for (k, sql) in STMTS {
            st.insert(*k, c.prepare(sql).await?);
        }
        Ok(Writer { c, tenant: 0, seq: 0, next_id: 0, st })
    }

    pub async fn use_tenant(&mut self, tenant: i64) -> Res<()> {
        let row = self
            .c
            .query_one(
                "SELECT (SELECT seq FROM tenants WHERE id=$1),
                        greatest((SELECT coalesce(max(id),0) FROM items WHERE tenant_id=$1),
                                 (SELECT coalesce(max(id),0) FROM entities WHERE tenant_id=$1),
                                 (SELECT coalesce(max(id),0) FROM placements WHERE tenant_id=$1))",
                &[&tenant],
            )
            .await?;
        self.tenant = tenant;
        self.seq = row.get(0);
        self.next_id = row.get::<_, i64>(1) + 1;
        Ok(())
    }

    fn id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id - 1
    }

    /// Same transaction content as the SQLite writer, in 3 round trips:
    /// [BEGIN + link lookup] -> [items upsert RETURNING] -> [everything else + COMMIT].
    pub async fn apply_page(&mut self, page: &Page) -> Res<usize> {
        let t = &self.tenant.clone();
        let link_keys: Vec<&str> = page.items.iter().filter_map(|i| i.link.as_deref()).collect();
        let st = &self.st;
        let (_, links) = futures_util::try_join!(self.c.batch_execute("BEGIN"), async {
            if link_keys.is_empty() { Ok(vec![]) } else { self.c.query(&st["links"], &[t, &link_keys]).await }
        })?;
        let links: HashMap<String, i64> = links.iter().map(|r| (r.get(0), r.get(1))).collect();

        let n = page.items.len();
        let (mut ids, mut ents, mut seqs) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        for it in &page.items {
            ids.push(self.id());
            ents.push(match it.link.as_ref().and_then(|k| links.get(k)) {
                Some(e) => *e,
                None => self.id(),
            });
            self.seq += 1;
            seqs.push(self.seq);
        }
        let keys: Vec<&str> = page.items.iter().map(|i| i.key.as_str()).collect();
        let vals: Vec<&str> = page.items.iter().map(|i| i.validator.as_str()).collect();
        let raws: Vec<&[u8]> = page.items.iter().map(|i| i.raw_hash.as_slice()).collect();
        let obs: Vec<&str> = page.items.iter().map(|i| i.canonical.as_str()).collect();
        let st = &self.st;
        let rows = match self.c.query(&st["items"], &[t, &page.coll, &ids, &keys, &vals, &raws, &obs, &ents, &seqs]).await {
            Ok(r) => r,
            Err(e) => {
                let _ = self.c.batch_execute("ROLLBACK").await;
                return Err(e);
            }
        };

        let pos: HashMap<&str, usize> = keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        let (mut raw_h, mut raw_b) = (vec![], vec![]);
        let (mut ne_id, mut ne_seq) = (vec![], vec![]);
        let (mut ue_id, mut ue_seq, mut ue_c) = (vec![], vec![], vec![]);
        let (mut pl_id, mut pl_e, mut pl_i, mut pl_r) = (vec![], vec![], vec![], vec![]);
        let mut del = vec![];
        let (mut o_s, mut o_e, mut o_ent, mut o_item, mut o_rid) = (vec![], vec![], vec![], vec![], vec![]);
        let (mut j_e, mut j_s, mut j_op) = (vec![], vec![], vec![]);
        let pl_ids: Vec<i64> = (self.next_id..self.next_id + rows.len() as i64).collect();
        self.next_id += rows.len() as i64;
        for (k, r) in rows.iter().enumerate() {
            let (item_id, ent, key, inserted): (i64, i64, &str, bool) = (r.get(0), r.get(1), r.get(2), r.get(3));
            let i = pos[key];
            let it = &page.items[i];
            let linked = it.link.as_ref().is_some_and(|l| links.contains_key(l));
            raw_h.push(raws[i]);
            raw_b.push(it.raw.as_str());
            if inserted {
                if linked {
                    ue_id.push(ent);
                    ue_seq.push(seqs[i]);
                    ue_c.push(Some(obs[i]));
                } else {
                    ne_id.push(ent);
                    ne_seq.push(seqs[i]);
                }
                pl_id.push(pl_ids[k]);
                pl_e.push(ent);
                pl_i.push(item_id);
                pl_r.push(linked as i16);
            } else {
                ue_id.push(ent);
                ue_seq.push(seqs[i]);
                ue_c.push(it.link.as_ref().map(|_| obs[i]));
                del.push(item_id);
            }
            for o in &it.occ {
                o_s.push(o.start);
                o_e.push(o.end);
                o_ent.push(ent);
                o_item.push(item_id);
                o_rid.push(o.rid);
            }
            j_e.push(ent);
            j_s.push(seqs[i]);
            j_op.push(if inserted { 1i16 } else { 2 });
        }

        // Two items of one page can be placements of the same entity; keep its last change only,
        // as the row-by-row SQLite writer does (ON CONFLICT DO UPDATE cannot touch a row twice).
        let mut seen = std::collections::HashSet::new();
        let keep: Vec<bool> = j_e.iter().rev().map(|e| seen.insert(*e)).collect::<Vec<_>>().into_iter().rev().collect();
        let pick = |v: &[i64]| v.iter().zip(&keep).filter(|(_, k)| **k).map(|(x, _)| *x).collect::<Vec<i64>>();
        let (j_e, j_s) = (pick(&j_e), pick(&j_s));
        let j_op: Vec<i16> = j_op.iter().zip(&keep).filter(|(_, k)| **k).map(|(x, _)| *x).collect();
        let c = &self.c;
        let mut batch: Vec<(&Statement, Vec<&(dyn ToSql + Sync)>)> = vec![];
        if !raw_h.is_empty() {
            batch.push((&st["raw"], vec![t, &raw_h, &raw_b]));
        }
        if !ne_id.is_empty() {
            batch.push((&st["ins_ent"], vec![t, &ne_id, &ne_seq]));
        }
        if !ue_id.is_empty() {
            batch.push((&st["upd_ent"], vec![t, &ue_id, &ue_seq, &ue_c]));
        }
        if !pl_id.is_empty() {
            batch.push((&st["ins_pl"], vec![t, &pl_id, &pl_e, &pl_i, &pl_r]));
        }
        if !del.is_empty() {
            batch.push((&st["del_occ"], vec![t, &del]));
        }
        if !o_s.is_empty() {
            batch.push((&st["ins_occ"], vec![t, &page.coll, &o_s, &o_e, &o_ent, &o_item, &o_rid]));
        }
        if !j_e.is_empty() {
            batch.push((&st["journal"], vec![t, &j_e, &j_s, &j_op]));
        }
        batch.push((&st["cursor"], vec![t, &page.coll, &page.cursor]));
        let seq = self.seq;
        batch.push((&st["seq"], vec![t, &seq]));
        // try_join_all polls in order, so the pipeline keeps statement order; COMMIT goes last.
        let res = futures_util::try_join!(
            try_join_all(batch.iter().map(|(s, p)| c.execute(*s, p))),
            c.batch_execute("COMMIT")
        );
        if let Err(e) = res {
            let _ = c.batch_execute("ROLLBACK").await;
            return Err(e);
        }
        Ok(rows.len())
    }

    /// Fully durable mutation accept (the connection must not be a `synced` one).
    pub async fn accept_mutation(&mut self, entity: i64, idem: &str, patch: &str) -> Res<()> {
        self.seq += 1;
        let (t, s, st) = (&self.tenant, self.seq, &self.st);
        let c = &self.c;
        let p_rev: [&(dyn ToSql + Sync); 2] = [t, &entity];
        let (_, base) = futures_util::try_join!(c.batch_execute("BEGIN"), c.query_one(&st["m_rev"], &p_rev))?;
        let base: i64 = base.get(0);
        let p_ins: [&(dyn ToSql + Sync); 5] = [t, &entity, &base, &patch, &idem];
        let p_es: [&(dyn ToSql + Sync); 3] = [t, &entity, &s];
        let p_seq: [&(dyn ToSql + Sync); 2] = [t, &s];
        futures_util::try_join!(
            c.execute(&st["m_ins"], &p_ins),
            c.execute(&st["m_ent"], &p_es),
            c.execute(&st["m_journal"], &p_es),
            c.execute(&st["seq"], &p_seq),
            c.batch_execute("COMMIT"),
        )?;
        Ok(())
    }
}

pub struct Reader {
    pub c: Client,
    range_p: Statement,
    range_i: Statement,
    changes: Statement,
}

impl Reader {
    pub async fn new(c: Client) -> Res<Self> {
        let w = format!("o.tenant_id=$1 AND o.collection_id = ANY($2::bigint[]) AND o.start_utc >= $3::bigint - {MAX_DUR} AND o.start_utc < $4::bigint AND o.end_utc > $3::bigint");
        let range_p = c
            .prepare(&format!(
                "SELECT DISTINCT ON (o.entity_id, o.recurrence_id) o.entity_id, o.recurrence_id, o.start_utc, o.end_utc,
                        coalesce(e.canonical, i.observed)
                 FROM occurrences o JOIN entities e ON e.tenant_id=o.tenant_id AND e.id=o.entity_id
                 JOIN items i ON i.tenant_id=o.tenant_id AND i.id=o.item_id WHERE {w}"
            ))
            .await?;
        let range_i = c
            .prepare(&format!(
                "SELECT DISTINCT ON (o.entity_id, o.recurrence_id) o.entity_id, o.recurrence_id, o.start_utc, o.end_utc
                 FROM occurrences o WHERE {w}"
            ))
            .await?;
        let changes = c
            .prepare(
                "SELECT c.seq, c.entity_id, c.op, coalesce(e.canonical, i.observed)
                 FROM changes c JOIN entities e ON e.tenant_id=c.tenant_id AND e.id=c.entity_id
                 JOIN placements p ON p.tenant_id=c.tenant_id AND p.entity_id=c.entity_id AND p.role=0
                 JOIN items i ON i.tenant_id=c.tenant_id AND i.id=p.item_id
                 WHERE c.tenant_id=$1 AND c.seq > $2 ORDER BY c.seq LIMIT $3",
            )
            .await?;
        Ok(Reader { c, range_p, range_i, changes })
    }

    pub async fn range(&self, tenant: i64, colls: &[i64], from: i64, to: i64, payload: bool) -> Res<usize> {
        let st = if payload { &self.range_p } else { &self.range_i };
        let colls = colls.to_vec();
        Ok(self.c.query(st, &[&tenant, &colls, &from, &to]).await?.len())
    }

    pub async fn changes(&self, tenant: i64, since: i64, limit: i64) -> Res<usize> {
        Ok(self.c.query(&self.changes, &[&tenant, &since, &limit]).await?.len())
    }

    pub async fn head_seq(&self, tenant: i64) -> Res<i64> {
        Ok(self.c.query_one("SELECT seq FROM tenants WHERE id=$1", &[&tenant]).await?.get(0))
    }
}
