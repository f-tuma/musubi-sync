# Fáze 0: ověření faktů o poskytovatelích

Stav k 2026-11-11. Podklad pro `docs/architecture/proposal.md`, revize 5. Návrh se tímto dokumentem nemění.

## Otázka

Návrh opírá několik rozhodnutí o fakta o poskytovatelích, která sám označuje jako **(neověřeno)** nebo „z paměti". U každého z nich hledám odpověď na tři otázky:

- co o něm říká oficiální dokumentace, případně důvěryhodný sekundární zdroj;
- co z toho plyne pro návrh;
- které fakty jde rozhodnout jen živým testem, a jak přesně takový test vypadá.

## Metoda

- Čtení oficiální dokumentace. Microsoft Learn v Markdown podobě (`?accept=text/markdown`), u každé stránky je uvedeno její pole `updated_at`. U Google for Developers je uvedeno „Last updated" ze stránky. Apple pro CalDAV a CardDAV žádnou vývojářskou dokumentaci nemá.
- Sekundární zdroje jen tam, kde oficiální chybí: Microsoft Q&A, blog Exchange MVP, issues a README open-source projektů. U každého je to výslovně označeno.
- Metadata crates z crates.io API a stav repozitářů z GitHub API. Bezpečnostní advisories z adresáře `rustsec/advisory-db`.
- Pro srovnání jsem jen četl Musubi (`/home/flamendrin/workspace/musubi`, HEAD b7890ab): `apps/api/src/sync/provider_profiles.ts`, adaptéry a audity. Jeho zaznamenané živé sondy cituji jako nepřímý důkaz.
- **Žádné volání API poskytovatele ani přístup k reálnému účtu.** Všechno níže pochází z veřejných stránek. Co jde rozhodnout jen živě, má u sebe testovací plán.

### Reprodukovatelné příkazy

```sh
# Microsoft Learn jako Markdown (pole updated_at je v hlavičce)
curl -sL "https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0&accept=text/markdown" | grep -n -i "cancelledOccurrences\|exceptionOccurrences\|iCalUId\|transactionId"
curl -sL "https://learn.microsoft.com/en-us/graph/api/event-delta?view=graph-rest-1.0&accept=text/markdown" | grep -n -i "maxpagesize\|calendarView/delta\|\$select"
curl -sL "https://learn.microsoft.com/en-us/graph/change-notifications-overview?accept=text/markdown" | grep -n "1,000 active"
curl -sL "https://learn.microsoft.com/en-us/graph/api/contact-update?view=graph-rest-1.0&accept=text/markdown" | grep -n -A6 "Request headers"

# Google for Developers jako text
g(){ curl -sL "$1" | python3 -I -c 'import sys,re,html;t=re.sub(r"(?s)<(script|style).*?</\1>","",sys.stdin.read());print(html.unescape(re.sub(r"<[^>]+>"," ",t)))'; }
g https://developers.google.com/workspace/calendar/api/v3/reference/events/list   | grep -i "2500"
g https://developers.google.com/workspace/calendar/api/v3/reference/events/watch  | grep -i "604800"
g https://developers.google.com/workspace/calendar/api/guides/batch               | grep -i "1000 calls"
g https://developers.google.com/workspace/calendar/api/v3/reference/events        | grep -i "base32hex\|responseStatus"
g https://developers.google.com/workspace/tasks/reference/rest/v1/tasks/list      | grep -i "max allowed"
g https://developers.google.com/people/api/rest/v1/people.connections/list        | grep -i "7 days\|between 1 and 1000"
g https://developers.google.com/people/api/rest/v1/people/updateContact           | grep -i "failedPrecondition"

# Crates a advisories
for c in oauth2 openidconnect jsonwebtoken jwt-simple josekit biscuit; do
  curl -s -A "musubi-phase0-research" "https://crates.io/api/v1/crates/$c/versions?per_page=1" ; done
curl -s https://api.github.com/repos/rustsec/advisory-db/contents/crates/rsa

# WebDAV-Push
curl -s -o /dev/null -w '%{http_code}\n' https://datatracker.ietf.org/doc/draft-bitfire-webdav-push/   # 404
curl -s https://raw.githubusercontent.com/bitfireAT/webdav-push/main/webdav-push.mkd | head -20
```

Zdroje jsou uvedeny u každého faktu. Pokud není u data uvedeno jinak, znamená „aktualizace" datum úpravy stránky u poskytovatele. Všechny stránky jsem četl 2026-11-11.

---

## Výsledky: Microsoft Graph v1.0

### G1. Hlavní událost série, `exceptionOccurrences`, `cancelledOccurrences` a `$expand`

- **Tvrzení (kap. 11.2, body 1–2):** výjimky a zrušené výskyty jde přečíst z hlavní události série.
- **Ověřeno, dokumentace.**
  - Zdroj: [event resource v1.0](https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0), aktualizace 2026-04-20.
  - `cancelledOccurrences`: kolekce hodnot `occurrenceId`. Vrací se jen s `$select` a jen při `GET` hlavní události podle ID.
  - `exceptionOccurrences`: vyžaduje `$select` i `$expand`, vrací se také jen při `GET` hlavní události podle ID a zrušené výskyty neobsahuje.
  - `instances` zrušené výskyty také neobsahuje.
  - Zdroj: [event-get, příklad 3](https://learn.microsoft.com/en-us/graph/api/event-get?view=graph-rest-1.0), aktualizace 2025-07-23. Volání `GET /me/events/{id}?$select=subject,start,end,occurrenceId,exceptionOccurrences,cancelledOccurrences&$expand=exceptionOccurrences` vrací:
    - výjimky s `id` a jen vybranými vlastnostmi;
    - zrušené výskyty jako řetězce `OID.{seriesMasterId}.{yyyy-mm-dd}`.
  - Vlastnosti `occurrenceId` a `uid` popisuje jen **beta** stránka resource ([event beta](https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-beta), aktualizace 2025-12-03). Příklad ve v1.0 je přesto používá.
  - Datum v `occurrenceId` je podle bety v zóně `recurrenceTimeZone` pravidla.
- **Dopad (kap. 11.2, 2.5).**
  - Hlavní model „jedna položka na sérii" je dokumentací podložený.
  - Zrušený výskyt se pozná jen podle data, ne času. Původní začátek se tedy musí dopočítat z pravidla a zóny série (bod 2 spiku).
  - O stránkování výjimek ani o úplnosti polí přes nested `$select` dokumentace nic neříká.
- **Živý test potřeba.** Viz plán T1.

### G2. Stálost `iCalUId` u výskytu a u série

- **Tvrzení (kap. 11.2, bod 4):** `iCalUId` hlavní události je stálý.
- **Ověřeno, dokumentace.**
  - Zdroj: [event v1.0](https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0), 2026-04-20.
  - `iCalUId` je „unique identifier for an event across calendars. This ID is different for each occurrence in a recurring series."
  - `uid`, tedy hodnota společná hlavní události a všem výskytům (ekvivalent iCalendar UID), je dokumentovaná jen v betě.
  - Stálost při přesunu mezi kalendáři ani shodu u kopií schůzky v jiných schránkách dokumentace netvrdí.
- **Dopad (kap. 2.4, 12.7, 14.7).**
  - Korelace výskytů podle `iCalUId` nefunguje.
  - Pro párování série s UID jiných poskytovatelů je potřeba `uid`, a ta je jen v betě. Ve v1.0 zbývá `iCalUId` hlavní události. Zda se rovná iCalendar UID série, je **neověřené**.
- **Živý test potřeba** (T1, krok 5).

### G3. `events/delta` (beta) a `calendarView/delta` (v1.0)

- **Ověřeno, dokumentace.**
  - Zdroj: [event-delta v1.0](https://learn.microsoft.com/en-us/graph/api/event-delta?view=graph-rest-1.0), aktualizace 2026-05-14.
    - Ve v1.0 je jen `GET /me/calendarView/delta?startDateTime&endDateTime` (a varianta `/users/{id}`), a to jen pro **primární kalendář**.
    - Nepodporuje `$select`, `$expand`, `$filter`, `$orderby` ani `$search`.
    - `@removed` zahrnuje i události, které vypadly z okna.
  - Zdroj: [event-delta beta](https://learn.microsoft.com/en-us/graph/api/event-delta?view=graph-rest-beta), aktualizace 2025-07-23.
    - Beta má `/me/events/delta` a `/me/calendars/{id}/events/delta` s volitelným `startDateTime`.
    - Vrací jen `id`, `type`, `start` a `end` jednotlivých událostí a hlavních událostí sérií.
    - Stránka výslovně říká „Currently available only in the beta version".
    - Beta umí `calendarView/delta` i nad konkrétním kalendářem.
- **Dopad (kap. 2.6, 11.2, bod 3).**
  - Ve v1.0 je delta jen pro primární kalendář. Sekundární kalendáře potřebují subscription a cílené čtení, nebo pojistné dotazování nad `calendarView`.
  - Rozhodnutí „beta sledovat, nestavět na ní" platí.
- **Živý test potřeba** (T1, krok 6): co přinese `calendarView/delta` po změně jedné výjimky série.

### G4. Neměnná ID (immutable IDs)

- **Ověřeno, dokumentace.**
  - Zdroj: [Outlook immutable IDs](https://learn.microsoft.com/en-us/graph/outlook-immutable-id), aktualizace 2025-08-06.
  - Hlavička `Prefer: IdType="ImmutableId"` se posílá u **každého** požadavku. U subscriptions musí být už při jejich založení a platí i pro delta.
  - ID se změní jen při přesunu do archivní schránky nebo při exportu a novém importu.
  - Podporované typy: event, contact, outlookTask, message, attachment a eventMessage.
  - `translateExchangeIds` převede až 1000 ID najednou.
  - Známý problém: delta nad zprávami s neměnnými ID může změny vynechat. U událostí to dokumentace neuvádí.
  - Bez hlavičky se `id` při přesunu mezi kontejnery mění (event resource).
- **Dopad (kap. 2.4, 12.7, bod 3).**
  - Adaptér musí posílat hlavičku vždy, i u subscriptions. Jinak se přesun mezi kalendáři projeví jako smazání a vytvoření s novým ID.
  - S hlavičkou by ID přesun téže schránky přežilo. To je dobrý základ pro párování přesunu.
- **Živý test potřeba** (T3): jak přesun hlásí delta a subscription.

### G5. Životnost subscriptions a jejich počet

- **Ověřeno, dokumentace.**
  - Zdroj: [subscription resource v1.0](https://learn.microsoft.com/en-us/graph/api/resources/subscription?view=graph-rest-1.0), aktualizace 2026-09-17.
    - Outlook message, event a contact: nejvýš **10 080 min** (7 dní); s rich notifications **1 440 min**.
    - todoTask: **4 230 min**, jen globální cloud.
    - Hodnoty pod 45 min server zvýší na 45 min.
    - Latence: u event „Unknown"; contact a calendar v průměru pod 1 min, nejvýš 3 min; todoTask v průměru pod 2 min, nejvýš 15 min.
  - Zdroj: [change notifications overview](https://learn.microsoft.com/en-us/graph/change-notifications-overview), aktualizace 2026-04-07. Nejvýš **1 000 aktivních subscriptions na schránku** pro všechny aplikace dohromady (message, event, contact).
- **Dopad (kap. 2.6, 5.4).**
  - Fakta v kap. 2.6 sedí.
  - U událostí Microsoft latenci nezaručuje. Pojistné dotazování (kap. 3.2) je nutnost, ne jen pojistka.
  - Obnovovat po kalendáři znamená až N subscriptions na schránku. Limit 1 000 je sdílený s jinými aplikacemi, ale pro naše účely dostatečný.
- **Živý test:** ne. Latenci změří metriky v provozu (kap. 5.7).

### G6. Limity `$batch` a throttling

- **Ověřeno, dokumentace.**
  - Zdroj: [JSON batching](https://learn.microsoft.com/en-us/graph/json-batching), aktualizace 2025-02-26.
    - Nejvýš **20 požadavků** v dávce.
    - Každý požadavek se throttluje zvlášť, takže `429` může přijít jen u části dávky.
  - Zdroj: [throttling limits](https://learn.microsoft.com/en-us/graph/throttling-limits), aktualizace 2026-09-17. Outlook na kombinaci aplikace a schránky:
    - **10 000 požadavků za 10 min**;
    - **4 souběžné požadavky**;
    - 150 MB uploadu za 5 min.
  - Dávka bez `dependsOn` jde do Outlooku po čtyřech, s `dependsOn` sekvenčně.
- **Dopad (kap. 5.2, 5.3).**
  - `$batch` šetří kola sítě, ne kvótu. Souběžnost na schránku se musí počítat včetně požadavků uvnitř dávky.
  - Plánovač (kap. 5.3) má mít pro Graph na schránku semafor 4 společný pro dávky i jednotlivé požadavky.
- **Živý test:** ne.

### G7. Strop `odata.maxpagesize`

- **Neověřeno.**
  - Zdroj: [paging](https://learn.microsoft.com/en-us/graph/paging), aktualizace 2025-08-06. Stránka říká jen, že API mají různé výchozí a maximální velikosti stránky a že hodnotu nad maximem API může ignorovat, oříznout nebo odmítnout.
  - Stránka `event-delta` hlavičku uvádí bez stropu.
  - Konkrétní číslo pro Outlook jsem v oficiální dokumentaci nenašel. Často citovaných 1000 u `/messages` platí pro `$top`, ne pro delta **(neověřeno)**.
- **Dopad (kap. 5.2):** dokud není strop změřený, posílat `Prefer: odata.maxpagesize` a číst skutečnou velikost stránky z odpovědi. Kód nesmí předpokládat, že ji server dodrží.
- **Živý test potřeba** (T4).

### G8. Potlačení pozvánek

- **Ověřeno, negativně.**
  - Zdroj: [user-post-events](https://learn.microsoft.com/en-us/graph/api/user-post-events?view=graph-rest-1.0), aktualizace 2026-08-03. „When you create an event that includes attendees, the server sends invitations to all attendees" a chování nejde nastavit.
  - Zdroj: [event-update](https://learn.microsoft.com/en-us/graph/api/event-update?view=graph-rest-1.0), aktualizace 2026-05-19.
    - PATCH, který mění jen `attendees`, pošle aktualizaci jen změněným účastníkům.
    - Úprava hlavní události se samostatně upravenými výskyty pošle víc e-mailů.
  - Microsoft Q&A (vlákna z let 2022 a 2025) opakovaně potvrzuje, že `sendInvitationMessage: false` nefunguje.
  - Neoficiální obchvat: [Glen Scales, Exchange MVP](https://glenscales.substack.com/p/workarounds-for-sendinvitationsmode), 2026-09-28.
    - Vytvořit událost s `isDraft=true` a pak přes rozšířené MAPI vlastnosti (MessageFlags, PidLidFInvited) přepnout příznaky.
    - Nepodporované a křehké.
  - EWS jako náhradní cesta nepřipadá v úvahu ([Exchange Team](https://techcommunity.microsoft.com/blog/exchange/retirement-of-exchange-web-services-in-exchange-online/3924440), MC1227454, 2026-02-05). EWS v Exchange Online je od **2026-10-01** blokované, pokud správce výslovně nepovolí AppID, a **2027-04-01** se definitivně vypíná.
- **Dopad (kap. 12.9, 14.6).**
  - Riziko 6 (kap. 11.3) platí pro Graph v plné míře.
  - Kopie schůzky bez účastníků je v Graphu jediná podporovaná bezpečná cesta. Bod 6 spiku je tím z dokumentace zodpovězen.
  - Obchvat přes `isDraft` do návrhu nedávat. Nanejvýš ho jednou živě vyzkoušet a zapsat jako zamítnutý.
- **Živý test:** volitelný (T5, krok 3), jen pro záznam.

### G9. Smí organizátor zapsat odpověď účastníka?

- **Neověřeno.**
  - Zdroje: [attendee](https://learn.microsoft.com/en-us/graph/api/resources/attendee?view=graph-rest-1.0) (aktualizace 2024-11-21) a [responseStatus](https://learn.microsoft.com/en-us/graph/api/resources/responsestatus?view=graph-rest-1.0) (aktualizace 2025-12-03).
  - Pole `attendees[].status` je popsané jen jako „The attendee's response". Dokumentace neříká, že by ho mohl zapsat organizátor, ani že by ho server přepsal.
  - Odpověď na pozvánku má v Graphu vlastní akce `accept`/`decline`/`tentativelyAccept`, které volá pozvaný nad svou kopií.
- **Dopad (kap. 11.3 riziko 6, kap. 14.6).**
  - Dokud test neprojde, adaptér Graphu musí hlásit schopnost „zápis odpovědi účastníka organizátorem" jako `false`.
  - Následek, který návrh popisuje: vnější účastníci uvidí „čeká".
- **Živý test potřeba** (T5).

### G10. Okno `transactionId`

- **Ověřeno, negativně.**
  - Zdroj: [event v1.0](https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0). `transactionId` „avoids redundant POST operations if the client retries" a nejde později změnit.
  - **Délku okna deduplikace dokumentace neuvádí.**
  - Musubi z vlastní sondy došel ke stejnému závěru (`provider_profiles.ts:143–144`):
    - „No infinite deduplication window is assumed";
    - obnova je jen pozorovací, výpisem s hledáním `transactionId`.
- **Dopad (kap. 12.5).** `transactionId` je pomocník pro rychlé opakování, ne idempotency klíč. Nejistý výsledek po delší době řeší pozorování, ne nový POST. Kap. 12.5 by to měla říct výslovně.
- **Živý test potřeba** (T6), jen pro změření praktického okna. Návrh na výsledku stát nesmí.

### G11. Podmíněné zápisy u To Do

- **Neověřeno, dokumentace mlčí.**
  - Zdroj: [todoTask-update](https://learn.microsoft.com/en-us/graph/api/todotask-update?view=graph-rest-1.0), aktualizace 2026-05-06. Tabulka hlaviček uvádí jen `Authorization` a `Content-Type`. Odpovědi nesou `@odata.etag`.
  - Zdroj: [todoTask-delta](https://learn.microsoft.com/en-us/graph/api/todotask-delta?view=graph-rest-1.0), aktualizace 2026-06-19. Delta je per seznam (`/me/todo/lists/{id}/tasks/delta`) a podporuje `Prefer: odata.maxpagesize`.
  - Musubi (`provider_profiles.ts`, profil `microsoft-task-update/delete-v1`): „A response ETag or preflight comparison cannot prove To Do conditional-write enforcement". Přenositelné UPDATE a DELETE jsou vypnuté.
- **Dopad (kap. 3.5, 6.2, 12.6).** Do důkazu hlásit pro To Do `conditional_write: none`. Konflikty se pak řeší čtením před zápisem s vědomým oknem souběhu.
- **Živý test potřeba** (T7).

### G12. Delta kontaktů se složkou a bez ní

- **Částečně ověřeno.**
  - Zdroje: [contact-delta v1.0](https://learn.microsoft.com/en-us/graph/api/contact-delta?view=graph-rest-1.0) (aktualizace 2026-05-14) a [beta](https://learn.microsoft.com/en-us/graph/api/contact-delta?view=graph-rest-beta) (aktualizace 2026-05-11).
    - Obě dokumentují jen `/me/contactFolders/{id}/contacts/delta`.
    - Existuje i `/me/contactFolders/delta` pro samotné složky.
  - `/me/contacts/delta` není dokumentované. Sekundární zdroje:
    - [Microsoft Q&A 1634378](https://learn.microsoft.com/en-us/answers/questions/1634378/) (2024): pracovník Microsoftu píše, že funguje jako obecná funkce `delta`, i když není dokumentovaná.
    - [Q&A 1130161](https://learn.microsoft.com/en-us/answers/questions/1130161/): může pokrývat jen výchozí složku.
- **Dopad (kap. 2.7).** Stavět na deltě po složkách:
  1. projít `/me/contactFolders/delta`;
  2. pro každou složku spustit `contactFolders/{id}/contacts/delta`.

  Nedokumentovaná cesta bez složky do návrhu nepatří.
- **Živý test:** volitelný (T8), jen jestli chceme mít pojistku.

### G13. `changeKey` a `If-Match` u kontaktů a událostí

- **Neověřeno, dokumentace mlčí.**
  - Tabulky hlaviček u [contact-update](https://learn.microsoft.com/en-us/graph/api/contact-update?view=graph-rest-1.0) (2026-06-06) a [event-update](https://learn.microsoft.com/en-us/graph/api/event-update?view=graph-rest-1.0) (2026-05-19) uvádějí jen `Authorization` a `Content-Type`.
  - `If-Match` a `412` Graph dokumentuje jinde (OneNote, driveItem, listItem), ne u Outlooku.
  - Nepřímé důkazy:
    - Microsoft Q&A (zh-cn, [5621966](https://learn.microsoft.com/zh-cn/answers/questions/5621966/)): `412` při zastaralém `changeKey` v `If-Match` u událostí. Serverové úpravy `changeKey` mění.
    - Musubi, sonda na osobním účtu: PATCH události s `If-Match: W/"…"` (`@odata.etag`) je podmíněný; „Native changeKey is not an ETag substitute".
    - Musubi: DELETE zastaralý `If-Match` **ignoroval**, „best effort, not CAS" (`provider_profiles.ts:153, 162`).
  - Počet e-mailových adres kontaktu: [contact](https://learn.microsoft.com/en-us/graph/api/resources/contact?view=graph-rest-1.0) (2026-03-12) ho neuvádí. Obvykle se uvádějí 3 (Email1–3 v Outlooku) **(neověřeno)**.
- **Dopad (kap. 2.7, 3.5).**
  - Používat `@odata.etag` (ne `changeKey`) v `If-Match` a ověřit to zvlášť pro každý endpoint a typ účtu.
  - DELETE v Graphu považovat za nepodmíněný. Návrh by měl mít u Graphu schopnost `conditional_delete: false`.
  - Ztrátový počet e-mailů patří do profilu adaptéru.
- **Živý test potřeba** (T7).

### G14. Jak Graph hlásí přesun mezi kalendáři

- **Neověřeno.**
  - Graph nemá API pro přesun události: `api/event-move` vrací 404, existuje jen pro zprávy.
  - Přesun tedy dělá uživatel v klientu Outlook.
  - Podle G4 si s `ImmutableId` ID přesun ve stejné schránce drží. `calendarView/delta` je jen nad primárním kalendářem a přesun ven z něj ukáže jako `@removed`.
- **Dopad (kap. 12.7, bod 3).** Párování přesunu podle neměnného ID je u Graphu reálné, ale jen když adaptér sleduje všechny kalendáře schránky.
- **Živý test potřeba** (T3).

---

## Výsledky: Google

### O1. Maximální velikost stránky u událostí

- **Ověřeno, dokumentace.**
  - Zdroj: [events.list](https://developers.google.com/workspace/calendar/api/v3/reference/events/list), aktualizace 2026-07-29.
  - Výchozí `maxResults` je 250 a „the page size can never be larger than **2500** events".
  - Stránka může být kratší nebo i prázdná, a přesto mít `nextPageToken`.
- **Dopad (kap. 5.2).** Údaj „z paměti" platí. Smyčka čtení musí končit podle chybějícího `nextPageToken`, ne podle délky stránky.
- **Živý test:** ne.

### O2. Životnost watch kanálu

- **Částečně ověřeno.**
  - Zdroj: [events.watch](https://developers.google.com/workspace/calendar/api/v3/reference/events/watch), aktualizace 2026-05-12. `params.ttl`: „Default is **604800 seconds**", tedy 7 dní.
  - Zdroj: [push guide](https://developers.google.com/workspace/calendar/api/guides/push), aktualizace 2026-09-11.
    - Platí přísnější z požadované hodnoty a „Google Calendar API internal limits or defaults".
    - Kanál se neobnovuje sám a skutečná expirace přijde v odpovědi a v hlavičce `X-Goog-Channel-Expiration`.
  - **Maximum dokumentace neuvádí.** Sekundární zdroje uvádějí 7 dní jako praktický strop. Jeden blog (CodeWords) uvádí „až asi 30 dní" bez důkazu **(neověřeno)**.
- **Dopad (kap. 2.6, 5.4).**
  - Plánovat obnovu podle `expiration` vrácené serverem, ne podle konstanty.
  - Výchozí plán: obnova po 6 dnech.
- **Živý test potřeba** (T9), jen požádat o 30 dní a přečíst `expiration`.

### O3. Limity batch endpointu

- **Ověřeno, dokumentace.**
  - Zdroj: [Calendar batch](https://developers.google.com/workspace/calendar/api/guides/batch), aktualizace 2026-09-03.
    - Nejvýš **1000 volání** v jedné dávce.
    - N volání se počítá jako N požadavků do kvóty.
  - People API má stejné pravidlo ([batch](https://developers.google.com/people/v1/batch), 2026-03-02) a navíc metody `batchCreateContacts`/`batchUpdateContacts` s nejvýš **200 kontakty** ([batchCreateContacts](https://developers.google.com/people/api/rest/v1/people/batchCreateContacts)).
  - Zdroj: [Calendar quota](https://developers.google.com/workspace/calendar/api/guides/quota), aktualizace 2026-11-11.
    - **600 požadavků za minutu** na uživatele a projekt, **10 000 za minutu** na projekt, klouzavé okno.
    - Nově **1 000 000 požadavků za den** na projekt jako práh, nad kterým se bude účtovat. Platí pro projekty založené od 2026-05-01; podrobnosti účtování Google oznámí později.
  - Zdroj: [Tasks limits](https://developers.google.com/workspace/tasks/limits), aktualizace 2026-09-03. Tasks má „courtesy limit" 50 000 dotazů za den.
- **Dopad (kap. 2.6, 5.2, 5.3).**
  - Batch šetří kola, ne kvótu. Prakticky ho omezuje minutový limit 600 na uživatele, takže dávky přes ~100 volání nemají smysl.
  - Nový denní práh 1 000 000 na projekt doplnit do kap. 2.6 jako omezení provozovatele (self-host).
- **Živý test:** ne.

### O4. Klientská ID událostí

- **Ověřeno, dokumentace.**
  - Zdroje: [events resource](https://developers.google.com/workspace/calendar/api/v3/reference/events) a [insert](https://developers.google.com/workspace/calendar/api/v3/reference/events/insert), aktualizace 2026-07-07.
  - Znaky base32hex (`a-v`, `0-9`), délka 5–1024, unikátní v kalendáři.
  - Google kolize nemusí odhalit hned při vytvoření a doporučuje UUID podle RFC 4122.
  - `id` a `iCalUID` se zadávají jen jedno z nich.
- **Dopad (kap. 12.5).**
  - Rezervace vzdáleného ID v transakci platí.
  - ID je třeba odvodit z UUID umístění převodem do base32hex malými písmeny: 128 bitů dá 26 znaků.
  - Kvůli „collision not guaranteed to be detected" po nejistém vytvoření ověřit obsah, ne jen existenci ID.
- **Živý test:** ne. Musubi tuto cestu už provozuje.

### O5. `sendUpdates` a potlačení pro jednotlivého účastníka

- **Ověřeno, dokumentace.**
  - Zdroj: [events.insert](https://developers.google.com/workspace/calendar/api/v3/reference/events/insert), 2026-07-07.
  - Hodnoty `all`, `externalOnly` a `none` platí pro celý požadavek. Parametr pro jednotlivého účastníka neexistuje.
  - „Note that some emails might still be sent."
  - U `none` Google varuje: „can have significant adverse effects, including events not syncing to external calendars or events being lost altogether for some users".
  - Zdroj: [events resource](https://developers.google.com/workspace/calendar/api/v3/reference/events), pole `attendees[].responseStatus`. To, zda se událost účastníkovi s účtem Google objeví v kalendáři, řídí jeho nastavení „Add invitations to my calendar".
- **Dopad (kap. 12.9, 14.6).**
  - Kap. 14.6 platí. Zkoušku „přidat místního účastníka samostatným PATCH se `sendUpdates=none`" provést (T10).
  - Výslovné varování o ztrátě událostí u `none` je důvod nepoužívat `none` u schůzek s vnějšími účastníky. Doplnit do kap. 14.6.
  - Tvrzení „Google vloží událost účastníkovi s účtem Google i bez e-mailu" je ověřené jen nepřímo: závisí na jeho nastavení.
- **Živý test potřeba** (T10).

### O6. Smí organizátor zapsat `attendees[].responseStatus`?

- **Ověřeno, dokumentace, s výhradou.**
  - Pole je označené jako **writable**. Google doporučuje `needsAction` pro nové události.
  - Varování: když se zapíše `declined`, `tentative` nebo `accepted`, účastníkům s nastavením „When I respond to invitation in email" nebo „Only if the sender is known" se odpověď může vrátit na `needsAction` a událost neuvidí.
  - Nad 200 pozvaných se stav nepropaguje.
- **Dopad (kap. 11.3 riziko 6, 14.6).**
  - U Googlu jde odpověď místního účastníka do položky organizátora zapsat, takže ji vnější účastníci uvidí.
  - Riziko je ve vedlejších účincích u účastníků s Google účtem. Zda se zápis drží i u účastníka, který je zároveň uživatelem Google, je potřeba vyzkoušet.
- **Živý test potřeba** (T10, krok 4).

### O7. Google Tasks: klientská ID, skryté pole pro značku, sync tokeny

- **Ověřeno, dokumentace.**
  - Zdroj: [tasks resource](https://developers.google.com/workspace/tasks/reference/rest/v1/tasks), aktualizace 2026-10-19.
    - Pole: `id`, `etag`, `title`, `updated`, `parent`, `position`, `notes`, `status`, `due`, `completed`, `deleted`, `hidden`, `links[]` (jen pro čtení), `webViewLink` a `assignmentInfo`.
    - **Žádné rozšířené ani skryté vlastnosti.** `hidden` je jen pro čtení a znamená „vyčištěno po dokončení".
  - `id` je popsané jen jako „Task identifier". Že ho při `insert` jde zadat, dokumentace neuvádí.
  - Zdroj: [tasks.list](https://developers.google.com/workspace/tasks/reference/rest/v1/tasks/list).
    - Stránka nejvýš **100**, výchozí 20.
    - Inkrement jen přes `updatedMin` se `showDeleted` a `showHidden`. **Sync token neexistuje.**
  - Zdroj: [REST index](https://developers.google.com/workspace/tasks/reference/rest). Metody `delete`, `get`, `insert`, `list`, `move`, `patch` a `update`. **Žádné `watch`, žádný push.**
- **Dopad (kap. 2.6, 12.5, 12.10, 13.4).**
  - „Google Tasks značku neunese" platí. Jediná možnost je viditelný text v `notes`, a to návrh správně nechce.
  - Nejistý výsledek vytvoření se řeší pozorováním podle obsahu.
  - Inkrement přes `updatedMin` je závislý na hodinách serveru. Brát ho s přesahem, například `updatedMin = poslední_updated − 1 min`, a deduplikovat podle `etag`.
- **Živý test potřeba** (T11). Ověřit, že klientské `id` v `insert` server ignoruje nebo odmítne, a že `updatedMin` vrací smazané a skryté úkoly.

### O8. People API: kvóty, `clientData`, etag, sync token, push

- **Ověřeno, dokumentace (kromě kvót).**
  - `updateContact` ([odkaz](https://developers.google.com/people/api/rest/v1/people/updateContact), aktualizace 2025-05-13):
    - když `person.metadata.sources.etag` nesouhlasí, server vrátí `400` s reason `failedPrecondition`;
    - při po sobě jdoucích úpravách se má použít etag z odpovědi;
    - jde o podmíněný zápis s chybou 400, ne 412.
  - [connections.list](https://developers.google.com/people/api/rest/v1/people.connections/list) (2024-08-06):
    - sync token vyprší **7 dní** po plném syncu, chyba `EXPIRED_SYNC_TOKEN`;
    - první stránka plného syncu má zvláštní, nenavýšitelnou kvótu (`429`);
    - smazané kontakty přijdou s `PersonMetadata.deleted`;
    - zápisy se v deltě projeví se zpožděním „několika minut";
    - `pageSize` 1–1000, výchozí 100.
  - [Person](https://developers.google.com/people/api/rest/v1/people): `clientData` je „Arbitrary client data that is populated by clients" (klíč, hodnota, metadata). Vhodné pro značku entity.
  - [contacts guide](https://developers.google.com/people/v1/contacts) (2026-03-02): „Mutate requests for the same user should be sent sequentially".
  - Push: referenční index People API žádnou metodu `watch` nemá.
  - **Minutové kvóty:** veřejná stránka s čísly neexistuje (`/people/v1/limits` vrací 404) a průvodce uvádí jen kategorie „Critical read/write requests" a „Daily Contact Writes" bez čísel. Čísla jsou vidět jen v Cloud Console projektu **(neověřeno)**.
- **Dopad (kap. 2.7, 3.3, 5.3, 12.10).**
  - Etag „z paměti" platí. Adaptér mapuje `400 failedPrecondition` na konflikt, ne na trvalou chybu.
  - Zápisy kontaktů jednoho uživatele serializovat: souběžnost 1 na účet.
  - Kvóty číst z konzole provozovatele a adaptivně reagovat na `429`.
- **Živý test:** ne. Kvóty jsou vidět v konzoli bez volání API.

### O9. Omezení neověřené aplikace (kap. 11.3, riziko 12)

- **Ověřeno, dokumentace.** Zdroj: [Google Cloud Help 7454865](https://support.google.com/cloud/answer/7454865?hl=en). Neověřená aplikace s citlivými nebo omezenými scope ukazuje obrazovku „unverified app" a má strop **100 nových uživatelů celkem**.
- **Dopad (kap. 11.3, riziko 12).** Číslo doplnit. Pro self-host s vlastním projektem a malým počtem uživatelů strop nevadí, pro hostovanou službu ano.
- **Živý test:** ne.

### O10. Jak Google hlásí přesun mezi kalendáři

- **Neověřeno.**
  - [events.move](https://developers.google.com/workspace/calendar/api/v3/reference/events/move) (2026-05-12): přesun „changes an event's organizer" a jde jen u událostí typu `default`.
  - Jak to vypadá v inkrementálním syncu zdrojového a cílového kalendáře, dokumentace neříká. Očekává se, že ve zdroji přijde `status: cancelled` a v cíli nová položka se **stejným** `id`.
- **Dopad (kap. 12.7, bod 3):** pokud se očekávání potvrdí, párovat podle `id` a `iCalUID`.
- **Živý test potřeba** (T3).

### O11. Značka entity u událostí Google

- **Ověřeno, dokumentace.** `extendedProperties.private` je klíč–hodnota viditelná jen v kopii daného kalendáře. Proto je to správné místo pro značku.
- **Dopad (kap. 12.10):** potvrzuje návrh.

---

## Výsledky: iCloud

### I1. `sync-collection` pro CalDAV a CardDAV

- **CalDAV: ověřeno sekundárně.**
  - Apple nic nedokumentuje.
  - [Aurinko](https://www.aurinko.io/blog/caldav-apple-calendar-integration/) (2023, živý průvodce): `sync-collection` bez tokenu a s tokenem a stránkování po 10 záznamech.
  - [python-caldav](https://github.com/python-caldav/caldav/releases) od verze 0.8 testuje sync tokeny proti iCloudu.
  - `tsdav` a `go-icloud-caldav` sync tokeny proti iCloudu podporují.
  - Musubi používá `sync-collection` s úplným záložním čtením (`docs/audits/calendar-core-audit.md:28`).
- **CardDAV: neověřeno.**
  - Žádný zdroj neukázal `sync-collection` přímo nad `contacts.icloud.com`.
  - [DAVx⁵](https://manual.davx5.com/technical_information.html) ho použije, když ho server inzeruje, jinak spadne na PROPFIND. Je to tedy jen podmíněný důkaz.
  - [vdirsyncer #1145](https://github.com/pimutils/vdirsyncer/issues/1145) hlásí `403` při PUT s `If-Match` na iCloud CardDAV. To je samostatné riziko pro zápis.
- **Expirace tokenu na iCloudu: neověřeno.**
- **Dopad (kap. 2.7, 3.3).** Profil serveru musí `sync-collection` zjistit z `supported-report-set`, nepředpokládat ho, a mít záložní ctag s PROPFIND ETagů. Tak to návrh už popisuje.
- **Živý test potřeba** (T12).

### I2. Dodržuje iCloud `SCHEDULE-AGENT=CLIENT`?

- **Ověřeno sekundárně, negativně.**
  - [pv-icloud #24](https://github.com/HappyCynic/pv-icloud/issues/24), přibližně 2026-10:
    - iCloud inzeruje `calendar-auto-schedule` a `SCHEDULE-AGENT=CLIENT` **ignoruje**;
    - událost s vnějším účastníkem poslala pozvánku s parametrem i bez něj;
    - iCloud nastavil `SCHEDULE-STATUS=1.1`;
    - odpovědi účastníků server do kopie organizátora zapíše sám během minut.
  - Opačný zdroj ([compass-calendar #3260](https://github.com/KeepSoftwareSimple/compass-calendar/issues/3260)) je jen nevyzkoušený plán.
  - Podle RFC 6638 §3.1 server plánuje jen tehdy, když `ORGANIZER` patří vlastníkovi kalendáře. Zda iCloud nepošle pozvánku, když `ORGANIZER` chybí nebo je cizí, je **neověřené**.
- **Dopad (kap. 12.9, 14.6).**
  - Na iCloudu nejde pozvánku potlačit parametrem. Kap. 14.6 by měla iCloud vést stejně jako Graph: kopie bez účastníků, nebo e-mail navíc.
  - Návrh to snese, protože selhání znamená e-mail navíc, ne druhou pozvánku v kalendáři. Riziko 6 se ale rozšíří i na iCloud.
- **Živý test potřeba** (T13). Jde o jedinou zkoušku z jednoho zdroje a test zároveň pošle skutečnou pozvánku, takže jen na vlastní testovací adresy.

### I3. `calendar-user-address-set`

- **Neověřeno pro iCloud.**
  - Musubi vlastnost čte (`apps/api/src/sync/caldav_scheduling.ts:82`) s `schedule-outbox-URL`, ale v testech jen proti falešnému serveru.
  - Veřejný zdroj s obsahem odpovědi iCloudu jsem nenašel.
  - Podle RFC 6638 obsahuje `mailto:` adresy a případně URN. Očekávají se e-maily Apple Account, tedy `@icloud.com` a aliasy.
- **Dopad (kap. 14.2).** Do potvrzení nebrat adresy z vlastnosti jako ověřené pro mezitenantové plánování bez dalšího kroku. Návrh ověřuje identitu účtu jen přes přihlášení.
- **Živý test potřeba** (T13, krok 1).

### I4. Plánování na iCloudu

- **Částečně ověřeno sekundárně (z I2).**
  - Implicitní plánování na serveru: pozvánky rozesílá server a odpovědi aplikuje do kopie organizátora.
  - `freebusy` na iCloudu nefunguje ([python-caldav #3](https://github.com/python-caldav/caldav/issues/3)).
- **Dopad (kap. 2.6, 14):** profil iCloudu nastavit takto:
  - `server_scheduling: implicit`;
  - `suppress_per_attendee: false`;
  - `freebusy: false`.

### I5. Limity rychlosti

- **Neověřeno.** Apple žádné limity nezveřejňuje. Sekundární zdroje hlásí `503` při nárazové zátěži, bez čísel.
- **Dopad (kap. 5.3).** Konzervativní výchozí souběžnost (návrh: 2 na účet), respektovat `Retry-After` a adaptivně snižovat při `503` nebo `429`.
- **Živý test:** jen pasivně, metrikami v provozu. Zátěžový test proti Applu nedělat.

### I6. Limity hesel pro aplikace (app-specific passwords)

- **Ověřeno sekundárně.**
  - Text článku Apple „Using app-specific passwords" (support.apple.com/102654, dříve HT204397) cituje [Apple Community](https://discussions.apple.com/thread/253531567) a [AppleInsider](https://appleinsider.com/articles/18/09/10/what-app-specific-icloud-passwords-are-and-when-apple-forces-you-to-use-them):
    - nejvýš **25 aktivních** hesel;
    - **změna nebo reset hesla účtu všechna zneplatní**;
    - lze je rušit jednotlivě i všechna najednou;
    - vyžadují dvoufázové ověření.
  - Přímo na support.apple.com jsem aktuální znění nečetl (stránka se renderuje skriptem) **(neověřeno přímo)**.
- **Dopad (kap. 2.6, 3.1, 7).**
  - Stav účtu „přihlášení neplatné" musí rozlišit zneplatněné heslo pro aplikaci od výpadku: `401` znamená výzvu uživateli, ne opakování.
  - Návrh má nabádat k jednomu heslu na instalaci.
- **Živý test:** ne.

---

## Výsledky: obecné

### W1. Stav WebDAV-Push (2026-11-11)

- **Ověřeno.**
  - Specifikace je `draft-bitfire-webdav-push-00` jen v repozitáři [bitfireAT/webdav-push](https://github.com/bitfireAT/webdav-push): poslední push 2026-10-29, kategorie `exp`.
  - **V IETF Datatrackeru není** (stránka dokumentu vrací 404) a žádná pracovní skupina ho nepřijala.
  - Financování: NLnet NGI0 Entrust, 2022-12 až 2026-01.
  - Implementace:
    - DAVx⁵ od 4.4.x / 4.5 (klient, GPL-3.0);
    - [nc_ext_dav_push](https://github.com/bitfireAT/nc_ext_dav_push) (rozšíření Nextcloudu, AGPL-3.0);
    - [RustiCal](https://github.com/lennart-k/rustical) (AGPL-3.0);
    - [Xandikos](https://github.com/jelmer/xandikos) (GPL-3.0, přepínač `--webdav-push`, Web Push RFC 8030 s VAPID);
    - podle diskuse Stalwartu z 2026-08 také desktopový klient Chiri a rozpracované KDE.
  - Radicale ani Stalwart ho zatím nemají. Apple ani Google ho nepodporují.
- **Dopad (kap. 5.4, 11.3 riziko 11).**
  - Pro iCloud a Google nic nemění, push u nich chybí dál.
  - Pro obecné CalDAV servery je to volitelná schopnost, kterou má smysl hlásit v profilu serveru (`push: webdav-push`) a implementovat až po 1.0.
  - Všechny serverové implementace jsou copyleft. Jádro je ale klient, takže se ho to netýká.
- **Živý test:** ne.

---

## Výsledky: Rust crates

Data z crates.io API a GitHub API k 2026-11-11.

| Crate | Poslední verze | Datum | Licence | Repo naposledy | Poznámka |
| --- | --- | --- | --- | --- | --- |
| `oauth2` | 5.0.0 | 2025-01-21 | MIT OR Apache-2.0 | 2026-02-22 | stabilní, ~19 mil. stažení za 90 dní; od vydání 5.0 žádná nová verze |
| `openidconnect` | 4.1.0 | 2026-10-31 | MIT | 2026-10-31 | aktivní; staví na `oauth2` 5; discovery stáhne JWKS z `jwks_uri` |
| `jsonwebtoken` | 11.1.0 | 2026-09-16 | MIT | 2026-10-04 | aktivní; od 10.x je nutné zvolit kryptografický backend `aws_lc_rs` nebo `rust_crypto` |
| `jwt-simple` | 0.15.0 | 2026-09-25 | ISC | — | aktivní; vlastní typy klíčů, menší ekosystém |
| `josekit` | 0.10.3 | 2025-05-20 | MIT OR Apache-2.0 | — | závisí na OpenSSL |
| `biscuit` | 0.8.0 | 2026-04-05 | MIT | — | dlouhé pauzy mezi verzemi |
| `jose-jwk` (RustCrypto) | 0.1.2 | 2023-08-21 | Apache-2.0 OR MIT | — | neaktivní |

JWKS v `jsonwebtoken` (zdrojový kód `src/jwk.rs`, `src/decoding.rs`):

- `JwkSet` se deserializuje ze JSON;
- `JwkSet::find(kid)` najde klíč podle `kid`;
- `DecodingKey::from_jwk(&Jwk)` z něj vytvoří klíč;
- k dispozici je i výpočet thumbprintu.

**Stažení, cache a rotaci JWKS knihovna nedělá.** Návrh to správně předpokládá: „vlastní malá cache JWKS". Ta má umět dvě věci:

- při neznámém `kid` jednou JWKS obnovit, s rate limitem;
- respektovat `Cache-Control`.

Advisories:

- `rustsec/advisory-db` nemá žádný záznam pro `oauth2`, `openidconnect` ani `jsonwebtoken`.
- `openidconnect` ale závisí na `rsa` 0.9 s **RUSTSEC-2023-0071** (Marvin, časový postranní kanál při operacích se soukromým klíčem). Ověřování podpisů veřejným klíčem, jediné, co děláme, to nezasahuje.
- `cargo-deny` ho přesto nahlásí a výjimku je potřeba vědomě zapsat.

- **Dopad (kap. 8).**
  - Hodnocení „zralé" platí pro `oauth2` a `jsonwebtoken`.
  - `openidconnect` je aktivní a bere se jen pro discovery, jak návrh říká.
  - Z kryptografických backendů `jsonwebtoken` doporučuji `rust_crypto` (čistý Rust, sladěné s RustCrypto v kap. 8). `aws_lc_rs` přináší C a CMake do buildu.
  - Žádná z doporučených knihoven není copyleft.
- **Živý test:** ne.

### Licence zmíněné v kap. 1 („z paměti")

- DAVx⁵ OSE: **GPL-3.0** (GitHub API).
- Nango: **Elastic License 2.0** (soubor `LICENSE`).

Obojí platí tak, jak to návrh uvádí. ELv2 není open source licence podle OSI.

---

## Plány živých testů

Společná pravidla pro všechny testy:

- Pouze vyhrazené testovací účty: pracovní účet M365 Developer tenant a osobní účet outlook.com, testovací Google účet s testovacím Cloud projektem, testovací Apple Account. Žádné produkční nebo osobní účty.
- Účastníci jsou jen adresy, které testovací tým vlastní.
- Každý krok zaznamenává request ID, časy a surové odpovědi do `spikes/` podle pravidel fáze 0.

**T1. Série v Graphu (G1, G2, G3). Účty: pracovní i osobní.**

1. `POST /me/events` vytvoří denní sérii s 30 výskyty v zóně Europe/Prague.
2. Přes `PATCH /me/events/{occurrenceId}` změnit 3 výskyty (čas, předmět, místo) a 2 zrušit (`DELETE`). Přidat výskyt po změně DST a celodenní sérii.
3. `GET /me/events/{masterId}?$select=subject,start,end,recurrence,cancelledOccurrences,exceptionOccurrences&$expand=exceptionOccurrences($select=subject,start,end,location,originalStart,body)`.
   - Očekávání: 3 výjimky se všemi vybranými poli, 2 položky v `cancelledOccurrences`.
   - Pokud nested `$select` nevrátí `originalStart`, je potřeba záložní model A.
4. Opakovat se sérií, která má 300 výjimek (vytvořit skriptem v rámci limitu 4 souběžných požadavků). Zjistit, jestli odpověď obsahuje `@odata.nextLink` uvnitř `exceptionOccurrences`, nebo je oříznutá.
5. Porovnat `iCalUId` hlavní události s UID v `.ics` exportu a s `iCalUId` téže schůzky ve schránce pozvaného. Zkontrolovat i `uid` z bety.
6. Pak změnit jednu výjimku a zavolat `GET /me/calendarView/delta` s předchozím delta tokenem. Očekává se položka výskytu typu `exception`. Zaznamenat, zda nese `seriesMasterId`.

**T3. Přesun mezi kalendáři (G4, G14, O10).**

- Graph:
  1. Ve dvou kalendářích téže schránky a s `Prefer: IdType="ImmutableId"` přesunout událost v klientu Outlook.
  2. Sledovat subscription na `/me/events`, `calendarView/delta` nad primárním kalendářem a `GET` podle starého ID.
  - Očekávání: ID se nemění, delta primárního kalendáře hlásí `@removed`.
- Google:
  1. `events.move` z kalendáře A do B.
  2. Inkrementální `events.list` se `syncToken` v A i v B.
  - Očekávání: v A `status: cancelled` se stejným `id`, v B nová položka se stejným `id` a `iCalUID`.

**T4. Strop `maxpagesize` (G7).** `GET /me/calendarView/delta` s `Prefer: odata.maxpagesize=1000`, potom 5000, nad kalendářem s více než 6000 událostmi. Zaznamenat skutečnou velikost stránek a hlavičku `Preference-Applied`.

**T5. Odpověď účastníka a pozvánky v Graphu (G8, G9).**

1. Organizátor A vytvoří schůzku s účastníkem B, testovací adresou mimo tenant.
2. `PATCH` události u A s `attendees: [{emailAddress: B, type: required, status: {response: "accepted"}}]`.
   - Číst zpět: drží se `accepted`?
   - Dostal B e-mail?
3. Volitelně zkusit obchvat přes `isDraft=true` a MAPI (G8) a zapsat výsledek jako zamítnutou cestu.

**T6. Okno `transactionId` (G10).**

- Dvakrát `POST /me/calendars/{id}/events` se stejným `transactionId` s odstupem 1 s, 1 min, 10 min a 1 h, vždy na novou událost.
- Zaznamenat, kdy začne server vytvářet duplikát.
- Výsledek slouží jen pro informaci.

**T7. Podmíněné zápisy v Graphu (G11, G13). Pro event, contact a todoTask, na pracovním i osobním účtu:**

1. `GET` uloží `@odata.etag`.
2. Upravit v jiném klientovi.
3. `PATCH` s `If-Match` na starý etag. Očekává se `412`.
4. `DELETE` se starým etagem. Musubi hlásí ignorování.
5. Totéž s `changeKey` v `If-Match`.
6. Kontakt: zapsat 4 e-maily a zjistit, kolik jich server uloží.

**T8. Delta kontaktů bez složky (G12, volitelné).** Kontakty ve výchozí a ve vlastní složce. `GET /me/contacts/delta` a porovnat s deltami po složkách.

**T9. Životnost watch kanálu Google (O2).** `events.watch` s `expiration` nastaveným na teď + 30 dní a druhý požadavek bez `expiration`. Zapsat vrácené `expiration`.

**T10. Pozvánky a odpovědi v Googlu (O5, O6).**

1. Organizátor A vytvoří událost se `sendUpdates=none` a bez účastníků.
2. `PATCH` přidá účastníka C (účet Google) se `sendUpdates=none`.
   - Dostal C e-mail?
   - Objevila se mu událost v kalendáři? Zkusit obě hodnoty nastavení „Add invitations to my calendar".
3. Změnit čas se `sendUpdates=none`. Dostal C oznámení?
4. A zapíše `attendees[C].responseStatus=accepted`. Drží se, nebo se vrátí na `needsAction`? Opakovat s účastníkem mimo Google.

**T11. Google Tasks (O7).**

- `tasks.insert` s tělem obsahujícím `id`. Očekává se, že server ID ignoruje nebo odmítne.
- Smazat a dokončit úkol, vyčistit seznam.
- `tasks.list` s `updatedMin`, `showDeleted=true` a `showHidden=true`. Očekává se, že vrátí smazané i skryté úkoly.

**T12. iCloud `sync-collection` (I1).**

1. PROPFIND `supported-report-set` na kolekci kalendáře i adresáře.
2. `REPORT sync-collection` s prázdným tokenem, pak s vráceným tokenem po jedné změně.
3. Token starý 30 dní: zaznamenat chybu, očekává se `valid-sync-token` precondition.
4. PUT s `If-Match` na kontakt, kvůli `403` z vdirsyncer #1145.

**T13. Plánování na iCloudu (I2, I3).**

1. PROPFIND `calendar-user-address-set` a `schedule-outbox-URL` na principalu.
2. PUT události s `ORGANIZER` vlastníka a jedním `ATTENDEE;SCHEDULE-AGENT=CLIENT` na vlastní testovací adresu. Přišel e-mail? Jakou hodnotu má `SCHEDULE-STATUS`?
3. Totéž bez `ORGANIZER` a s cizím `ORGANIZER`.

---

## Verdikt a doporučení

1. **Model jedné položky na sérii v Graphu má oporu v dokumentaci.** Úplnost výjimek a stránkování je ale jen živá otázka (T1). Spike z kap. 11.2 zůstává podmínkou, rozhodovací tabulka platí.
2. **Pozvánky.** Graph potlačení nemá (ověřeno) a EWS jako náhrada končí 2027-04-01. iCloud `SCHEDULE-AGENT=CLIENT` podle jediného veřejného testu ignoruje. Kap. 14.6 doporučuji upravit: potlačení je podporované jen u Googlu (celý požadavek) a u obecného CalDAV podle profilu. Pro Graph a iCloud je bezpečnou cestou kopie bez účastníků.
3. **Odpověď účastníka u organizátora.** U Googlu je pole zapisovatelné, s vedlejšími účinky. U Graphu je to neověřené a výchozí schopnost je `false`.
4. **Podmíněné zápisy nejsou u Graphu jednotné.** Používat `@odata.etag`, ne `changeKey`. DELETE brát jako nepodmíněný a To Do jako bez CAS. Schopnosti v kap. 6.2 rozdělit na `conditional_update` a `conditional_delete` po typu položky.
5. **`transactionId` není idempotency klíč.** Okno není dokumentované, obnova je pozorovací.
6. **Google Tasks** nemá push, sync token, skryté pole ani klientské ID (dokumentace, ID ještě živě). People API vyžaduje etag (chyba `400 failedPrecondition`), sync token platí 7 dní a push nemá. Rizika návrhu pro duplicity po ztrátě DB (kap. 13.4) platí.
7. **Čísla do návrhu:**
   - Google `maxResults` 2500;
   - batch 1000 volání (People `batch*Contacts` 200);
   - watch TTL výchozí 7 dní, maximum nedokumentované;
   - Graph `$batch` 20, 4 souběžné a 10 000 za 10 min na schránku;
   - 1 000 subscriptions na schránku;
   - neověřená aplikace Google 100 uživatelů;
   - iCloud 25 hesel pro aplikace;
   - Google Calendar nově s denním prahem 1 000 000 požadavků na projekt.
8. **Rust:** `oauth2`, `openidconnect` a `jsonwebtoken` jsou použitelné a s permisivní licencí. Cache JWKS je vlastní kód. U `openidconnect` zapsat výjimku pro RUSTSEC-2023-0071 v `cargo-deny`.
9. **WebDAV-Push** je experimentální soukromý draft mimo IETF se čtyřmi serverovými implementacemi. Jen jako volitelná schopnost po 1.0.

## Otevřené riziko

- **Pozvánky navíc u Graphu a iCloudu jsou nevyhnutelné**, pokud má položka u poskytovatele účastníky. Riziko 6 je tím širší, než návrh uvádí.
- **iCloud bez dokumentace:** chování se může změnit bez ohlášení (I1–I5). Jediný zdroj k `SCHEDULE-AGENT` je jeden issue.
- **`maxpagesize` a stránkování výjimek v Graphu** nejsou známé. Úvodní sync může být pomalejší než odhad v kap. 5.2.
- **Watch kanál Google** má maximum nedokumentované. Obnova se musí řídit vrácenou expirací.
- **Kvóty People API** nejsou veřejné, takže kapacitu pro kontakty nejde předem spočítat.
- **Nový denní práh Google Calendar** (1 000 000 na projekt, od 2026-05-01) s ohlášeným budoucím účtováním může změnit ekonomiku hostované služby.
- **Sekundární zdroje** (Q&A, blogy, issues) jsou označené a mohou být zastaralé. Musubi sondy platí jen pro zaznamenané typy účtů.

## Souhrnná tabulka

| Tvrzení | Ověřeno? | Dopad na návrh | Živý test |
| --- | --- | --- | --- |
| Graph: `exceptionOccurrences`/`cancelledOccurrences` přes `$select`+`$expand` jen na GET hlavní události | ano (dok.) | model sérií má oporu (11.2) | ano, T1: úplnost polí, stovky výjimek |
| Graph: `iCalUId` se liší pro každý výskyt; `uid` jen v betě | ano (dok.) | korelace sérií přes `uid` (beta) nebo `iCalUId` hlavní události (2.4) | ano, T1 krok 5 |
| Graph: `events/delta` jen v betě; v1.0 `calendarView/delta` jen primární kalendář, bez `$select` | ano (dok.) | sekundární kalendáře přes subscription a dotazování (2.6, 11.2) | ano, T1 krok 6 |
| Graph: immutable ID přes `Prefer` u každého požadavku i subscription | ano (dok.) | povinná hlavička v adaptéru (2.4, 12.7) | ano, T3 (přesun) |
| Graph: subscription 10 080 min, todoTask 4 230 min, 1 000 na schránku | ano (dok.) | potvrzuje 2.6 | ne |
| Graph: `$batch` 20, 4 souběžné, 10 000 / 10 min | ano (dok.) | semafor 4 i pro dávky (5.3) | ne |
| Graph: strop `odata.maxpagesize` | ne | nepředpokládat velikost stránky (5.2) | ano, T4 |
| Graph: potlačení pozvánek | ano, neexistuje | kopie bez účastníků; EWS končí 2027-04-01 (12.9, 14.6) | volitelný, T5 krok 3 |
| Graph: organizátor zapíše odpověď účastníka | ne | výchozí schopnost `false` (11.3 r. 6, 14.6) | ano, T5 |
| Graph: okno `transactionId` | ano, nedokumentováno | jen pomocník, obnova pozorováním (12.5) | informativně, T6 |
| Graph: To Do podmíněný zápis | ne | `conditional_write: none` (3.5, 6.2) | ano, T7 |
| Graph: delta kontaktů bez složky | ne (jen Q&A) | stavět na deltě po složkách (2.7) | volitelný, T8 |
| Graph: `If-Match` u kontaktů a událostí | ne (Q&A + Musubi sonda) | `@odata.etag`, DELETE nepodmíněný (2.7, 3.5) | ano, T7 |
| Graph: počet e-mailů kontaktu | ne | ztrátovost v profilu (2.7) | ano, T7 krok 6 |
| Graph: hlášení přesunu mezi kalendáři | ne | párování podle immutable ID (12.7) | ano, T3 |
| Google: events `maxResults` 2500, výchozí 250 | ano (dok.) | potvrzuje 5.2 | ne |
| Google: watch TTL výchozí 604 800 s, maximum nedokumentované | částečně | obnova podle vrácené expirace (2.6, 5.4) | ano, T9 |
| Google: batch 1000 volání, počítá se N× | ano (dok.) | batch šetří kola, ne kvótu (5.2) | ne |
| Google: klientské ID base32hex 5–1024 | ano (dok.) | potvrzuje 12.5; UUID → base32hex | ne |
| Google: `sendUpdates` jen na celý požadavek; `none` varování o ztrátě | ano (dok.) | potvrzuje 14.6; nepoužívat `none` u vnějších | ano, T10 |
| Google: organizátor zapíše `responseStatus` | ano (writable) s výhradou | odpověď viditelná, riziko resetu (14.6) | ano, T10 krok 4 |
| Google Tasks: bez skrytého pole, bez sync tokenu, bez push, stránka 100 | ano (dok.) | potvrzuje 12.10, 13.4 | ne |
| Google Tasks: klientské ID | ne (nedokumentováno) | pozorování podle obsahu (12.5) | ano, T11 |
| People: etag povinný, `400 failedPrecondition` | ano (dok.) | mapovat na konflikt (2.7) | ne |
| People: sync token 7 dní, bez push, stránka 1000, `clientData` | ano (dok.) | potvrzuje 2.7, 12.10 | ne |
| People: minutové kvóty | ne (jen konzole) | adaptivně na `429`, souběžnost 1 (5.3) | ne (konzole) |
| Google: neověřená aplikace 100 uživatelů | ano (dok.) | doplnit do 11.3 r. 12 | ne |
| Google: hlášení `events.move` v syncu | ne | párování podle `id` (12.7) | ano, T3 |
| iCloud: CalDAV `sync-collection` | ano (sekundárně) | potvrzuje 3.3 | ano, T12 |
| iCloud: CardDAV `sync-collection` | ne | zjišťovat z `supported-report-set` (2.7) | ano, T12 |
| iCloud: `SCHEDULE-AGENT=CLIENT` dodržen | ne, sekundárně ignorován | iCloud jako Graph, kopie bez účastníků (14.6) | ano, T13 |
| iCloud: `calendar-user-address-set` | ne | adresy neověřené bez dalšího kroku (14.2) | ano, T13 krok 1 |
| iCloud: limity rychlosti | ne (nezveřejněno) | konzervativní souběžnost, `Retry-After` (5.3) | ne (jen metriky) |
| iCloud: 25 hesel pro aplikace, zrušení při změně hesla | ano (sekundárně) | `401` znamená výzvu uživateli (3.1, 7) | ne |
| WebDAV-Push: soukromý draft -00, mimo IETF, 4 serverové implementace | ano | volitelná schopnost po 1.0 (5.4) | ne |
| `oauth2` 5.0.0, `openidconnect` 4.1.0, `jsonwebtoken` 11.1.0: aktivní, MIT/Apache | ano | potvrzuje kap. 8; cache JWKS vlastní | ne |
| `openidconnect` → `rsa` RUSTSEC-2023-0071 | ano | výjimka v `cargo-deny`, ověřování nezasaženo | ne |
| DAVx⁵ GPL-3.0, Nango ELv2 | ano | potvrzuje kap. 1 | ne |
