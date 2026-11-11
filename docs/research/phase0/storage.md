# Fáze 0: úložiště – SQLite na tenanta vs sdílený PostgreSQL

Spike: [`spikes/storage-bench/`](../../../spikes/storage-bench/). Navazuje na návrh, kapitoly 4.3, 4.4, 5.1 a 12.
Měřeno 11. 11. 2026 na jednom stroji. Výsledky jsou ze single runu, viz rizika.

## 1. Otázka

Kapitola 4.3 návrhu nechává volbu mezi SQLite na tenanta (WAL) a sdíleným PostgreSQL otevřenou: má rozhodnout měření. Cíle z kapitoly 5.1:

| Cíl | Hodnota |
|---|---|
| Dotaz na rozsah, položku nebo změny od kurzoru | p99 < 10 ms na serveru |
| Přijetí zápisu (trvale v outboxu) | p99 < 10 ms |
| Vlastní režie jádra na inkrementální sync | < 20 ms |

Měří se:

- propustnost a latence příjmu,
- p50/p99 dotazu na rozsah a změn od kurzoru,
- čtení pod souběžným příjmem,
- cena transakce s plným fsync,
- velikost na disku,
- cena otevírání souborů u SQLite,
- záloha a obnova.

## 2. Krátká odpověď

**Obě varianty splní cíle pro běžného tenanta (5 000 položek) s velkou rezervou. Výkon tedy mezi nimi nerozhoduje a platí dvouvětvové doporučení z kapitoly 4.3.** Data ho upřesňují takto:

- **SQLite je na čtení 2–4× rychlejší.**
  - Rozsah měsíc přes 20 kolekcí: p99 1,07 ms proti 3,72 ms.
  - 8 souběžných čtenářů: 7 436 dotazů/s proti 2 929 dotazům/s.
  - Pod souběžným zápisem: p99 6,2 ms proti 8,1 ms.
  - PG přitom běžel přes localhost, bez reálného síťového skoku.
- **PostgreSQL je lepší na trvanlivé zápisy pod souběhem** (group commit).
  - Outbox s plným fsync, 16 tenantů naráz: SQLite p99 **12,2 ms (cíl nesplněn)**, PG p99 7,8 ms.
  - Jeden zapisovatel: obě varianty pod 5 ms.
- **Velký tenant (100 000 položek) s hustým rozsahem cíl nesplní ani jedna varianta.**
  - Měsíc přes 20 kolekcí vrací 11 761 výskytů s payloadem: SQLite p99 28,6 ms, PG 64,1 ms.
  - Jde o tvar API, ne o volbu backendu. Řešením je stránkování nebo dotaz bez payloadu (ids-only: SQLite 8,2 ms, PG 10,6 ms).
- **Inkrementální sync** (stránka 25 položek včetně žurnálu a kurzoru): SQLite p99 3,0 ms, PG 2,3 ms. Obě varianty jsou hluboko pod rozpočtem 20 ms.
- **Příjem dat** je u obou řádově desítky tisíc položek/s na tenanta a 170–217 tisíc položek/s souhrnně. Úzkým hrdlem bude poskytovatel.

**Doporučení:**

- Datovou rovinu stavět na **SQLite na tenanta** jako na jediném backendu fáze 1, s rozhraním úložiště, které PG nevylučuje.
- PG implementovat až ve chvíli, kdy padne rozhodnutí o hostované službě se závazkem dostupnosti. Jeho výhody jsou provozní (HA, PITR, spravovaná služba), ne výkonové.
- Podmínky pro SQLite viz sekce 6.

## 3. Metoda

### 3.1 Schéma (stejné na obou stranách)

Tabulky:

- `raw(hash → body)`: surová těla adresovaná hashem.
- `items(collection, remote_key, validator, raw_hash, observed, rev, seq, tombstone)`, unikátní podle (kolekce, vzdálený klíč).
- `entities(kind, canonical, rev, seq, tombstone)` a `placements(entity, item)` podle kapitoly 12.
- `occurrences(entity, item, collection, recurrence_id, start_utc, end_utc)` s indexem `(collection, start_utc)`. Je to rozvinutý index výskytů.
- `changes(entity → seq, op)`: žurnál „poslední změna na entitu“ s monotónním `seq` per tenant.
- `mutations(entity, base_rev, patch, idem_key)` a `outbox(placement, mutation, state, payload)`.

Rozdíly mezi backendy:

- PG má navíc sloupec `tenant_id` na začátku každého klíče a indexu a `fillfactor = 90` na tabulkách s update.
- SQLite má soubor na tenanta.
- Kanonická i surová data jsou na obou stranách text (JSON / iCalendar). Nikde neběží komprese ani binární formát.

Dotaz na rozsah:

- Čte `occurrences` přes `collection_id IN (…)`, `start_utc` v okně `[from − 2 dny, to)` a `end_utc > from`.
- Join na `entities` a `items` vrací kanonický payload.
- Deduplikuje přes `GROUP BY entity_id, recurrence_id`, protože jedna entita může mít víc umístění.
- Varianta „ids-only“ vrací jen identifikátory a časy, bez joinu na payload.

Změny od kurzoru: `WHERE seq > ? ORDER BY seq LIMIT 500`, s joinem na entitu.

### 3.2 Data

Generátor `src/datagen.rs` je deterministický (splitmix64), takže obě strany dostanou stejné položky.

**Předpoklady o mixu** (odhad, nevychází z reálných účtů):

- 10 % opakovaných řad:
  - DAILY 5–30 výskytů,
  - WEEKLY 8–105,
  - MONTHLY 6–37,
  - YEARLY 10.
- 8 % celodenních položek.
- 2 % položek jako druhé umístění entity z kolekce 0 (kapitola 12).
- 20 kolekcí na tenanta s vahou ~1/(c+1): několik velkých kalendářů a dlouhý ocas malých.
- Popis má 250–650 znaků. Surové tělo má ~918 B, kanonické ~1 013 B.

Výsledný objem:

| Sada | Obsah |
|---|---|
| Běžný tenant | 5 000 položek, ~23 tisíc výskytů (4,6 na položku), ~530 řad, 62–74 materializovaných entit s více umístěními |
| Celkem | 200 běžných tenantů (1 M položek) + 16 běžných tenantů pro sólo příjem (80 tisíc) + 2 velcí tenanti po 100 000 položkách |
| Velký tenant `t100001` | 448 768 výskytů, 10 085 řad, 1 419 materializovaných entit |

Velký tenant `t100000` byl nahrán stránkou 250 a `t100001` stránkou 2500. Čtení se měří na `t100001`.

### 3.3 Trvanlivost a ladění (fér na obě strany)

Mapování dvou úrovní trvanlivosti z kapitoly 4.4:

| | Stažená data (stránky příjmu) | Autorská data (mutace + outbox) |
|---|---|---|
| SQLite | `synchronous=NORMAL` (WAL: commit bez fsync, fsync při checkpointu) | `synchronous=FULL` jen pro danou transakci (fsync WAL při commitu) |
| PG | spojení příjmu s `synchronous_commit=off` | spojení outboxu s výchozím `synchronous_commit=on` (fsync WAL, group commit) |

**SQLite** (rusqlite 0.41, bundled SQLite):

- Pragmy:
  - `journal_mode=WAL`, `synchronous=NORMAL`,
  - `wal_autocheckpoint=0` a vlastní checkpointer na pozadí (viz níže),
  - `temp_store=MEMORY`, `cache_size=-32768` (32 MB), `mmap_size=256 MB`, `busy_timeout=5000`,
  - cache připravených dotazů 64.
- Spojení: jedno zapisovací spojení na tenanta a čtecí spojení zvlášť.
- Zapisovač vkládá po řádcích přes `prepare_cached`.

**PostgreSQL 17.11** (oficiální image `postgres:17`, host network):

- Paměť: `shared_buffers=8GB`, `effective_cache_size=32GB`, `work_mem=32MB`, `maintenance_work_mem=1GB`.
- WAL a checkpointy: `max_wal_size=16GB`, `min_wal_size=2GB`, `checkpoint_timeout=15min`, `checkpoint_completion_target=0.9`, `wal_buffers=64MB`, `wal_compression=lz4`.
- Planner a IO: `random_page_cost=1.1`, `effective_io_concurrency=200`, `max_connections=200`.
- Klient: tokio-postgres s pipeliningem, 3 round-tripy na stránku.
  - Multi-row upserty přes `unnest`.
  - ID přiděluje aplikace z jednoho čítače na tenanta, takže se nečeká na `RETURNING`.
  - Stránku se 2 500 položkami proto PG zapíše stejným počtem round-tripů jako stránku s 250 položkami.

Oba backendy:

- Leží na stejném btrfs svazku s `chattr +C` (nodatacow, bez komprese), aby CoW a zstd neovlivnily ani jednu stranu.
- Mají stejné indexy.
- Po nahrání dat proběhne `ANALYZE` (SQLite) nebo `VACUUM ANALYZE` (PG).

### 3.4 Dvě zjištění, bez kterých SQLite vychází špatně

Obě zjištění musí převzít produkční kód.

**1. `SQLITE_DEFAULT_MEMSTATUS=0`**

Bundled SQLite má ve výchozím stavu `MEMSTATUS=1`, tedy globální mutex kolem každého malloc. Ten serializuje čtenáře i na různých spojeních a v různých souborech.

| Build | 8 čtecích vláken (smoke data) |
|---|---|
| `MEMSTATUS=1` (výchozí) | ~4 000 dotazů/s, p50 1,7 ms |
| `MEMSTATUS=0` | ~31 000 dotazů/s, p50 0,10 ms |

Nastavuje se v `.cargo/config.toml` přes `LIBSQLITE3_FLAGS`. Bez toho by SQLite v testu souběžného čtení vyšel hůř než PG.

**2. Automatický checkpoint WAL v zapisovači**

Výchozí `wal_autocheckpoint=1000` spouští checkpoint uvnitř commitu, který přetečení způsobil. Na tomto disku (btrfs nodatacow) to stálo 25–40 ms na commit. Výsledky pro velkého tenanta, stránka 250:

| Nastavení | Propustnost | Latence stránky |
|---|---|---|
| Výchozí checkpoint | 6,4–9,6 tisíc položek/s | p50 23–39 ms, p99 až 463 ms |
| Práh 10 000 | – | p99 ~43 ms (špičky jen řidší) |
| `wal_autocheckpoint=0` + checkpointer na pozadí | 26,6 tisíc položek/s | p99 18,6 ms |

Checkpointer na pozadí je jedno vlákno na shard souborů. Každých 50 ms projde soubory a pustí `PRAGMA wal_checkpoint(PASSIVE)`.

Je to standardní produkční vzor, ale **je to náš kód**:

- musí hlídat růst WAL,
- musí se vypořádat s tenanty, jejichž checkpoint blokuje dlouhý čtenář,
- musí spolupracovat s odvozem WAL (sekce 7).

### 3.5 Reprodukce

```sh
cd spikes/storage-bench
./run.sh                 # celé měření (~25 min); výsledky v results/*.md; pak smaže kontejner i data
KEEP=1 ./run.sh          # ponechá kontejner musubi-storage-bench-pg a data/
SKIP_SQLITE=1 KEEP=1 ./run.sh   # jen PG část (SQLite výsledky zůstanou)
# rychlý smoke test:
TENANTS=20 EVENTS=500 LARGE=3000 CONC=4 COLD=200 QUERIES=50 MIXED_SECS=2 ./run.sh
```

Postup `run.sh`:

1. Spustí `cargo test --release` (determinismus generátoru).
2. Spustí `storage-bench sqlite load size read mixed outbox coldopen`.
3. Spustí `storage-bench pg load size read mixed outbox connect`.
4. Změří zálohu a obnovu.

Surové výsledky: `results/sqlite.md`, `results/pg.md`, `results/backup.md` a `results/env.md`.

Poznámka k zálohám: první běh měřil zálohu SQLite přes `cp -r`. Na btrfs udělá GNU cp 9 reflink, kopie tedy trvala 257 ms a nic nezkopírovala. `run.sh` teď používá `--reflink=never` a variantu `tar | zstd`. Čísla SQLite záloh v `backup.md` jsou z ručního spuštění těchto příkazů nad stejnými daty po hlavním běhu.

### 3.6 Prostředí

| Položka | Hodnota |
|---|---|
| CPU | Intel Core Ultra 7 265K (20 jader, max 6,5 GHz) |
| RAM | 62 GB, z toho ~23 GB obsazeno jinými procesy |
| Disk | NVMe Kingston KC3000 2 TB, btrfs (`compress=zstd:1`, `ssd`, `discard=async`); datový adresář `chattr +C` |
| Software | Linux 7.3.3-cachyos, Docker se 14 dalšími běžícími kontejnery (šum), Rust/cargo 1.99.0 |

Celá sada se vejde do RAM. Čtení proto probíhá s teplou page cache, kromě testu studeného otevírání. PG běží přes TCP na localhostu, takže reálné nasazení přidá síťový RTT na každý round-trip.

## 4. Výsledky

Latence jsou v ms. Výsledky pocházejí z jednoho běhu. Tučně je hodnota, která nesplní cíl 10 ms tam, kde se cíl vztahuje.

### 4.1 Příjem (ingest)

Latence je na jednu transakci, tedy na stránku včetně položek, surových těl, výskytů, entit, žurnálu a kurzoru kolekce.

| Test | SQLite p50 / p99 / max | SQLite položek/s | PG p50 / p99 / max | PG položek/s |
|---|---|---|---|---|
| Velký tenant sólo, stránka 250 | 8,8 / 18,6 / 25,2 | 26 612 | 6,8 / 9,8 / 10,7 | 34 920 |
| Velký tenant sólo, stránka 2500 | 75,6 / 114,6 / 114,6 | 28 211 | 66,7 / 84,6 / 84,6 | 35 723 |
| 100 tenantů, 16 zapisovatelů, stránka 250 | 11,8 / 25,2 / 36,4 | **194 293** souhrnně | 11,9 / 62,6 / 109,1 | 173 206 souhrnně |
| 100 tenantů, 16 zapisovatelů, stránka 2500 | 13,4 / 47,5 / 53,8 | **217 186** souhrnně | 11,8 / 174,4 / 259,2 | 170 284 souhrnně |
| Běžní tenanti sólo, stránka 250 | 3,2 / 9,8 / 10,4 | 45 544 | 5,6 / 7,8 / 8,5 | 32 151 |
| Běžní tenanti sólo, stránka 2500 | 3,4 / 16,6 / 16,9 | 48 634 | 5,5 / 39,5 / 40,4 | 30 749 |
| **Inkrementální sync**, stránka 25 (jen transakce) | 1,51 / **3,01** / 3,15 | – | 1,28 / **2,26** / 2,45 | – |

Poznámky:

- U běžných tenantů stránka 2500 v praxi znamená celou kolekci (největší má ~1 400 položek), protože stránky se dělí po kolekcích.
- Sólo jeden zapisovatel: PG je na velkém tenantovi o ~30 % rychlejší a má užší chvost. Dávkový `unnest` dělá méně práce na řádek než SQLite po řádcích. Na běžných tenantech je naopak SQLite rychlejší (45–49 tisíc proti 31–32 tisícům položek/s).
- Souběh 16 zapisovatelů: SQLite škáluje lépe, protože soubory jsou nezávislé a nesdílí žádný zámek. Souhrnná propustnost je o 12–28 % vyšší a p99 je 2,5–3,7× nižší.
- Chvost PG pod souběhem (p99 174 ms u stránky 2500) pochází ze sdíleného WAL a checkpointů. Dál jsme ho nerozebírali.
- Na poskytovatelích to nezáleží: Google i Graph vracejí řádově stovky položek na požadavek za stovky ms.

### 4.2 Dotaz na rozsah

Měří se 2 000 dotazů s náhodným tenantem a náhodným oknem. Běžní tenanti jsou 0..199, velký tenant je `t100001`.

| Test | Průměr řádků | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|---|
| Běžný, týden, 5 kolekcí | 35 | 0,13 / 0,36 / 0,46 | 0,31 / 1,07 / 2,13 |
| Běžný, týden, 20 kolekcí | 140 | 0,36 / 0,46 / 0,58 | 0,92 / 1,82 / 2,49 |
| Běžný, měsíc, 5 kolekcí | 150 | 0,22 / 0,56 / 6,71 | 0,61 / 1,62 / 2,99 |
| Běžný, měsíc, 20 kolekcí | 591 | 0,86 / 1,07 / 1,17 | 2,52 / 3,72 / 4,56 |
| Běžný, měsíc, 20 kolekcí, ids-only | 590 | 0,34 / 0,41 / 0,45 | 0,57 / 1,14 / 1,47 |
| Velký, týden, 5 kolekcí | 705 | 1,47 / 4,30 / 6,06 | 3,21 / 8,69 / 10,22 |
| Velký, týden, 20 kolekcí | 2 783 | 7,09 / 9,19 / 12,14 | 13,69 / **17,72** / 20,66 |
| Velký, měsíc, 5 kolekcí | 2 920 | 4,66 / **14,52** / 41,09 | 10,36 / **30,52** / 36,37 |
| Velký, měsíc, 20 kolekcí | 11 761 | 23,11 / **28,64** / 54,49 | 52,74 / **64,09** / 91,45 |
| Velký, měsíc, 20 kolekcí, ids-only | 11 759 | 6,81 / 8,22 / 9,08 | 8,64 / **10,61** / 13,71 |

Poznámky:

- Čas škáluje s počtem vrácených řádků, a hlavně s payloadem: 11 761 řádků po ~1 KB je ~12 MB na jeden dotaz.
- SQLite je 2–3× rychlejší, protože čte v procesu bez serializace přes drát.
- Cíl p99 < 10 ms SQLite drží do ~2 800 výskytů s payloadem a i bez payloadu při 11 760 výskytech. PG drží do ~700 výskytů s payloadem a bez payloadu je při 11 760 výskytech těsně nad cílem (10,6 ms).
- Měsíc přes 20 kalendářů se ~12 tisíci výskyty je extrém. Běžný uživatel má pro stejný dotaz ~590 výskytů.
- Řešení nezávislé na backendu: rozsah stránkovat (např. limit 2 000 výskytů) nebo vracet ids + časy a payload dotáhnout zvlášť pro viditelné položky.

### 4.3 Změny od kurzoru (limit 500)

| Test | Průměr řádků | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|---|
| Běžný, od head − 100 | 92 | 0,10 / 0,32 / 0,78 | 0,48 / 1,15 / 1,72 |
| Běžný, od náhodného kurzoru | 474 | 0,40 / 0,84 / 1,20 | 2,14 / 2,52 / 3,26 |
| Velký, od head − 100 | 100 | 0,04 / 0,05 / 4,90 | 0,40 / 0,58 / 1,16 |
| Velký, od náhodného kurzoru | 498 | 0,45 / 5,54 / 12,05 | 2,15 / 2,54 / 4,89 |

Obě varianty cíl splňují. U velkého tenanta má SQLite širší chvost (p99 5,5 ms proti 2,5 ms u PG). Pravděpodobně jde o čtení stránek payloadu rozházených po souboru. **(neověřeno)**

### 4.4 Souběžné čtení a čtení pod zátěží zápisu

| Test | SQLite p50 / p99 / max | PG p50 / p99 / max |
|---|---|---|
| 8 čtenářů, běžný měsíc / 20 kolekcí | 0,85 / 1,51 / 4,45 (7 436 dotazů/s) | 2,68 / 3,71 / 5,32 (2 929 dotazů/s) |
| Mix čtení (měsíc + změny), bez zapisovatelů | 0,82 / 1,06 / 3,79 | 3,09 / 3,97 / 11,40 |
| Mix čtení se **8 zapisovateli** na stejných 8 tenantech | 2,43 / **6,17** / 38,61 | 4,38 / **8,13** / 36,88 |
| Zápis během mixu (update stránek 250) | 3,16 / 10,57 / 39,15; **393 668 položek/s** | 7,70 / 19,19 / 46,40; 166 562 položek/s |

Test je záměrně nepříznivý: zapisovatelé přepisují celé tenanty (verze 1→2→3) ve stejných souborech nebo řádcích, které se čtou.

- Obě varianty drží čtení pod 10 ms na p99.
- Maximum je u obou ~37–39 ms.
- SQLite při tom zapsal 2,4× víc dat.

### 4.5 Přijetí zápisu s plnou trvanlivostí (outbox)

Jedna transakce obsahuje:

1. vložení mutace s idempotentním klíčem,
2. zvýšení `rev` entity,
3. zápis do žurnálu,
4. řádky outboxu pro každé umístění.

Transakce se commituje s fsync.

| Test | SQLite (`synchronous=FULL`) p50 / p99 / max | PG (`synchronous_commit=on`) p50 / p99 / max |
|---|---|---|
| 1 zapisovatel, 2 000 mutací | 1,07 / 4,74 / 24,84 | 1,28 / 3,27 / 32,69 |
| 16 tenantů současně, 4 800 mutací | 2,75 / **12,18** / 20,54 | 2,04 / 7,79 / 11,26 |

Toto je jediné místo, kde SQLite pro běžného tenanta cíl nesplnil.

Příčina:

- 16 souborů znamená 16 nezávislých fsync WAL.
- PG sloučí souběžné commity do jednoho fsync (group commit).
- Při stovkách aktivních tenantů na uzlu se rozdíl pravděpodobně zvětší. **(neověřeno)**

Možné cesty pro SQLite (žádná nebyla měřena):

- Krátké dávkování přijetí v rámci jednoho tenanta.
- Sdílený „accept log“ uzlu s group commitem: zápis do jednoho souboru s fsync, do tenantového souboru až potom.
- Disk s PLP (power-loss protection), kde je fsync levný.

### 4.6 Velikost na disku

| | SQLite | PG |
|---|---|---|
| Celá sada (1,24 M položek) | 5,40 GB | 4,79 GB (bez WAL) |
| Na položku | ~4,35 KB | ~3,86 KB |
| Běžný tenant (5 000) | 20,9 MB | – |
| Velký tenant (100 000) | 433 MB | – |

Rozpad PG:

| Tabulka | Velikost |
|---|---|
| items | 1 884 MB |
| raw | 1 418 MB |
| occurrences | 915 MB |
| placements | 238 MB |
| entities | 169 MB |
| changes | 160 MB |

Obě strany ukládají surová i kanonická data jako nekomprimovaný text. Odhad z kapitoly 4.3 (2 KB na událost, tedy ~1 TB pro 100 000 × 5 000) proto **podhodnocuje skutečnost zhruba 2×**:

- Bez komprese vychází ~4 KB na událost, tedy ~2 TB.
- S kompresí surové vrstvy (tar+zstd celé sady dává 808 MB, tj. 6,7×) by se odhad nejspíš potvrdil. **(neověřeno uvnitř databáze)**

### 4.7 SQLite: otevírání souborů, LRU a migrace

Data: 10 000 malých tenantů (50 položek, 2 705 MB). Vytvoření trvalo 19,2 s.

| Test | p50 | p99 | max | Pozn. |
|---|---|---|---|---|
| Otevření + pragmy + první dotaz, **studená** page cache (`fadvise DONTNEED`) | 0,52 | 0,96 | 2,86 | 10 000 souborů za 6,3 s |
| Zavření (studená) | 0,05 | 0,09 | 0,24 | |
| Otevření + první dotaz, teplá cache | 0,14 | 0,22 | 0,42 | 10 000 souborů za 1,8 s |
| Zavření (teplá) | 0,04 | 0,07 | 0,18 | |
| Požadavek přes LRU 1 000 otevřených z 10 000 (Zipf s = 1,1) | 0,013 | 0,33 | 3,40 | hit rate 77,8 %, RSS +164 MB při 1 000 otevřených |
| Migrace `ADD COLUMN` při otevření | 2,12 | 13,80 | 81,19 | 10 000 souborů ≈ 21 s sekvenčně |
| *Pro srovnání PG: nové spojení + prepare + první dotaz* | 4,01 | 5,24 | 5,61 | to, co ušetří pool |

Otevření databáze na miss LRU je levné: < 1 ms i se studenou cache. LRU s ~1 000 otevřenými soubory stojí ~164 KB RSS na soubor.

Líná migrace při otevření stojí p99 ~14 ms a max 81 ms na první požadavek po nasazení. Je to přijatelné, ale tuto latenci uvidí první požadavek každého tenanta. Alternativou je migrovat na pozadí po nasazení.

### 4.8 Záloha a obnova celé sady

| | Čas | Velikost |
|---|---|---|
| SQLite: kopie checkpointovaných souborů + `sync` (`cp --reflink=never`) | 2,7 s | 5,4 GB |
| SQLite: `tar \| zstd -1 -T0` + `sync` | 2,1 s | 808 MB |
| SQLite: obnova `zstd -d \| tar -x` + `sync` | 3,4 s | |
| PG: `pg_dump -Fc -Z lz4` | 6,2 s | 955 MB |
| PG: `pg_restore -j 8` do nové DB (včetně stavby indexů) | 9,7 s | |

SQLite se zálohoval v klidu, po checkpointu, bez souběžného zápisu. Za provozu je potřeba `VACUUM INTO`, backup API nebo odvoz WAL (viz sekce 7). PG dump je logický a konzistentní za provozu, ale není to PITR. Obnova SQLite zálohy je ~3× rychlejší a obnovu jednoho tenanta lze dělat po souboru.

## 5. Srovnání s cíli

| Cíl | SQLite | PG |
|---|---|---|
| Rozsah p99 < 10 ms, běžný tenant | ✅ max p99 1,07 ms | ✅ max p99 3,72 ms |
| Rozsah p99 < 10 ms, velký tenant s payloadem | ✅ do ~2 800 výskytů; ❌ 14,5–28,6 ms nad 2 900 výskytů | ✅ do ~700 výskytů; ❌ 17,7–64,1 ms nad 2 700 výskytů |
| Rozsah ids-only, 11 760 výskytů | ✅ 8,2 ms | ❌ 10,6 ms |
| Změny od kurzoru p99 < 10 ms | ✅ ≤ 5,5 ms | ✅ ≤ 2,6 ms |
| Čtení p99 < 10 ms pod zátěží zápisu | ✅ 6,2 ms | ✅ 8,1 ms |
| Přijetí zápisu p99 < 10 ms, 1 zapisovatel | ✅ 4,7 ms | ✅ 3,3 ms |
| Přijetí zápisu p99 < 10 ms, 16 tenantů současně | ❌ 12,2 ms | ✅ 7,8 ms |
| Režie inkrementálního syncu < 20 ms | ✅ p99 3,0 ms | ✅ p99 2,3 ms (+ síť) |

## 6. Verdikt a doporučení

**Čísla nevedou k PostgreSQL.**

- Na hlavní čtecí cestě je SQLite 2–4× rychlejší.
- Pod souběžným zápisem čte s nižším chvostem.
- Pod souběhem tenantů má vyšší propustnost příjmu.
- Zálohuje a obnovuje rychleji.

PG je výkonově lepší ve dvou bodech:

- **trvanlivé přijetí zápisu pod souběhem** (p99 7,8 ms proti 12,2 ms),
- **chvost sólo příjmu velkého tenanta** (p99 9,8 ms proti 18,6 ms; na rozpočet to vliv nemá).

Ani jeden bod nemění odhad z kapitoly 4.3: výkon nerozhoduje, rozhoduje provoz.

**Doporučení:**

1. **Fáze 1 staví datovou rovinu na SQLite na tenanta** (WAL, jeden zapisovatel na tenanta, LRU otevřených souborů). Platí pro self-host i pro první hostovanou instanci bez smluvního SLA.
2. **Podmínky pro SQLite** (bez nich vychází hůř než PG):
   - build s `SQLITE_DEFAULT_MEMSTATUS=0`;
   - `wal_autocheckpoint=0` a checkpointer na pozadí s hlídáním velikosti WAL;
   - zápis stránky jako jedna transakce s `synchronous=NORMAL` a mutace s `FULL`;
   - čtecí a zapisovací spojení zvlášť.
3. **Vyřešit přijetí zápisu pod souběhem dřív, než se to stane problémem:**
   - změřit dávkování nebo sdílený accept log s group commitem ve fázi 1;
   - do té doby počítat s p99 ~12 ms při 16 souběžných tenantech na uzel na tomto HW.
4. **API rozsahu stránkovat** (limit výskytů, kurzor) a nabídnout variantu bez payloadu. Bez toho cíl 10 ms nesplní žádný backend u hustých kalendářů.
5. **PostgreSQL zůstává druhým backendem za úzkým rozhraním úložiště.** Implementovat ho, až padne rozhodnutí o hostované službě se závazkem dostupnosti. Spike ukazuje, že port je přímočarý (stejné schéma s `tenant_id`; pipelining a `unnest`). Provozní výhody PG (synchronní replika, PITR, spravovaná služba) jsou pak hlavní argument, ne výkon.
6. **Opravit odhad velikosti** v kapitole 4.3 na ~4 KB na událost bez komprese a rozhodnout o kompresi surové vrstvy.

## 7. Provozní trade-offy

| | SQLite na tenanta | PostgreSQL |
|---|---|---|
| Záloha | soubor na tenanta; snímek přes `VACUUM INTO` / backup API; celý uzel 2–3 s na 5 GB (měřeno v klidu) | `pg_dump` 6,2 s; PITR přes `pg_basebackup` + archiv WAL (standardní, neměřeno) |
| Průběžný odvoz WAL | Litestream-like proces nebo vlastní kód, musí spolupracovat s naším checkpointerem **(neověřeno)**; na 10 000 souborů to je 10 000 proudů | jeden proud WAL (`archive_command`, pgBackRest, WAL-G) |
| HA | žádná; obnova tenanta na jiný uzel = stažení souboru (+ přehrání WAL); tenant je vázán na uzel, potřeba směrování | synchronní replika, failover (Patroni, spravované služby) |
| Migrace schématu | N souborů; líně při otevření (p99 ~14 ms na první požadavek) nebo na pozadí; ~21 s sekvenčně na 10 000 tenantů | jedna transakce; `ADD COLUMN` bez defaultu je okamžitý, velké `ALTER` zamykají sdílenou tabulku |
| Izolace tenantů | soubor = izolace; export / smazání = práce se souborem | hluční sousedé ve sdíleném WAL a buffer poolu (chvost příjmu p99 174 ms pod souběhem); RLS |
| Bloat | WAL se po checkpointu recykluje; `VACUUM` per soubor podle potřeby | MVCC bloat po masivních upsertech, autovacuum ladění (v testu nesledováno) |
| Spojení | otevření < 1 ms, žádný pool | nové spojení ~4 ms, nutný pool (PgBouncer / deadpool) |
| Self-host | jeden binární soubor | další služba k provozu a aktualizaci |

## 8. Otevřená rizika a co spike neměřil

- **Šum a jediný běh.** Na stroji běželo 14 dalších kontejnerů a ~23 GB RAM bylo obsazeno. Čísla jsou z jednoho běhu, bez opakování a intervalů spolehlivosti. Chvosty (p99, max) jsou nejméně stabilní část.
- **Teplá cache.** Celá sada se vejde do RAM. Chování při datech větších než RAM (reálný uzel s tisíci tenanty) jsme neměřili. **(neověřeno)**
- **Síť pro PG.** Měřeno přes localhost. V datacentru přidá každý round-trip ~0,1–0,5 ms **(neověřeno)**. Pipelined příjem má 3 round-tripy na stránku, dotaz 1.
- **Disk.** Spotřebitelské NVMe bez PLP, btrfs nodatacow. Cena fsync na serverovém disku s PLP, na ext4/XFS nebo na síťovém blokovém úložišti (EBS) bude jiná. Rozdíl v outboxu pod souběhem se může zmenšit i zvětšit. **(neověřeno)**
- **Group commit pro SQLite** (accept log uzlu, dávkování) je jen návrh. Bez něj SQLite cíl přijetí zápisu pod souběhem nesplnil.
- **Odvoz WAL pro SQLite** (Litestream nebo vlastní) a jeho souhra s checkpointerem nebyla testována. Totéž platí pro dobu obnovy z odvezeného WAL. **(neověřeno)**
- **Velký tenant.** Hustý rozsah nesplní cíl na žádném backendu. Stránkování rozsahu je nutné navrhnout v API.
- **Mix dat je syntetický** (řady, kolekce, velikosti těl). Reálné účty z Musubi mohou mít delší popisy, přílohy a víc výjimek. Velikost i čas dotazů škálují s payloadem.
- **SQLite zapisovač po řádcích.** Dávkové vkládání (multi-row `INSERT`, `raw` jako tabulka s rowid) by mohlo srovnat sólo chvost s PG. **(neověřeno)**
- **PG bloat a autovacuum** po opakovaných upsertech (mix 1→2→3) nebyly sledovány.
- **Migrace přes 10 000 souborů** je lineární (~21 s). Na 100 000 tenantů by to bylo ~3,5 min sekvenčně. Rozdělení na více vláken jsme neměřili.

## 9. Závislosti a licence

| Crate | Licence |
|---|---|
| rusqlite 0.41 (bundled) | MIT |
| SQLite | public domain |
| tokio, tokio-postgres, futures-util, libc | MIT / Apache-2.0 |

Nic copyleftového. PostgreSQL běží jen v Docker image (PostgreSQL License, permisivní).
