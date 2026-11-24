# Fáze 2: živá kontrola K10 přes `musubi-syncd`

## 1. Otázka

Plán, kapitola 9, K10: projde celý `musubi-syncd` proti skutečnému Googlu (Tasks, People) a iCloudu? Jak rychle se změna u poskytovatele objeví v deníku změn při aktivní a běžné úrovni dotazování?

## 2. Metoda

- 2026-11-22, `musubi-syncd` z `main` (`d567035`), samostatná instance s prázdným datovým adresářem, veřejně přes Tailscale Funnel (návrat OAuth).
- Google: testovací účet `musubi.test.01`, připojený přes `POST /v1/accounts` a souhlas v prohlížeči. iCloud: Apple ID uživatele s heslem pro aplikace, přes `POST /v1/accounts/dav` s profilem `icloud`. Uživatel povolil zápis (T11 byla jen pro čtení).
- Změny dělal skript přímo u poskytovatele: Tasks API (seznam `musubi-t10`), People API (`createContact`/`deleteContact`), CalDAV `PUT`/`DELETE` v kalendáři „Domácí“ a CardDAV v adresáři „card“. Vždy vytvoření a za 20–90 s smazání, v náhodnou chvíli intervalu.
- Měří se čas od odpovědi poskytovatele po první nový záznam v `/v1/changes` (dotaz každých 250 ms). SSE přidává nejvýš 150 ms slučování (B5), proto se neměřilo zvlášť.
- Aktivní úroveň: skript posílal `POST /v1/interest` nejvýš jednou za 60 s, jako klient s otevřenými daty (kapitola 7.3). Běžná úroveň: bez něj.

## 3. První data

Všechny kolekce byly načtené do 2 s od souhlasu (Google: Kontakty, 2 kalendáře, 2 seznamy úkolů) a do 8 s od připojení iCloudu (adresář, 2 kalendáře), bez chyb, s úplným pokrytím.

## 4. Zpoždění změny

Sekundy od změny u poskytovatele do záznamu v deníku. Vzorků je málo, proto bez percentilů.

| poskytovatel a druh, úroveň | vzorků | min | medián | max |
|---|---|---|---|---|
| Google úkoly, aktivní | 8 | 2,8 | 13,3 | 62,3 |
| Google kontakty, aktivní | 8 | 0,6 | 10,0 | 57,6 |
| iCloud události, aktivní | 8 | 1,3 | 13,5 | 51,0 |
| iCloud vizitky, aktivní | 8 | 1,3 | 18,3 | 54,0 |
| Google úkoly, běžná | 10 | 24,9 | 61,6 | 103,9 |
| Google kontakty, běžná | 6 | 40,0 | 75,8 | 94,2 |
| iCloud události, běžná | 6 | 51,4 | 118,2 | 487,0 |
| iCloud vizitky, běžná | 6 | 39,2 | 107,0 | 213,0 |

- Aktivní úroveň (15–60 s) drží očekávání „interval úrovně plus jedna delta“: maximum 62,3 s. Odpovídá K9 proti falešnému serveru (p50 22 s, max 47 s).
- Běžná úroveň (2–15 min) zůstala v mezích. Čtyři vzorky kontaktů Google v běžné úrovni chybí: skript čekal jen 300 s a kontakt vznikl i zmizel mezi dvěma dotazy (dotazy v 9:46:59, 9:54:53 a 10:10:22). Deník správně nezapsal nic. Chyba měření, ne serveru.
- Zpoždění propsání u People se od zpoždění dotazu oddělit nedá; minimum 0,6 s ukazuje, že je pod sekundou, ve shodě s T10 (0,1–0,2 s).
- iCloud přijal `PUT` s `If-None-Match: *` (vytvoření) i `DELETE` bez podmínky. `PUT` s `If-Match` (vdirsyncer #1145) zůstává neověřený, protože syncd do poskytovatele zatím nezapisuje.

## 5. Kvóta Tasks API (otevřená otázka 7)

Cloud Console projektu `musubi-sync-test`, 2026-11-22: **50 000 dotazů za den na projekt** (upravitelná, ten den využito 263) a 60 000 000 dotazů za minutu na uživatele. Podle D2 stojí seznam úkolů beze změn 60 dotazů za hodinu v aktivní úrovni, 5 v běžné a 1 ve spící, tedy 1 440, 120 a 24 za den. Denní kvóta tak pokryje zhruba 400 seznamů trvale v běžné úrovni, ale jen asi 35 trvale v aktivní. Pro jednoho uživatele i malé nasazení meze stačí. Větší nasazení potřebuje vyšší kvótu nebo delší `min` aktivní úrovně pro Tasks; kontrakt se tím nemění.

## 6. Sync token People (kapitola 6.3, otevřená otázka 3)

T10 vydala 2026-11-16 10:43:31 UTC dvěma úplnými čteními dva tokeny. Token A se od té doby nepoužil. Token B se posouval deltou jednou denně, naposledy 2026-11-22 11:00 UTC (8 delt). Další posun 23. 11. neproběhl, protože refresh token T10 vypršel po 7 dnech (aplikace OAuth v režimu Testing). 2026-11-24 09:24 UTC proběhla delta s oběma tokeny pod grantem instance K10 (stejný klient a účet):

| Token | Poslední vydání | Odpověď |
| --- | --- | --- |
| A | 2026-11-16 (úplné čtení) | `400` |
| B | 2026-11-22 (delta) | `400` |

Obě odpovědi měly stejné tělo:

```json
{"error": {"code": 400,
  "message": "Sync token is expired. Clear local cache and retry call without the sync token.",
  "status": "FAILED_PRECONDITION",
  "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
    "reason": "EXPIRED_SYNC_TOKEN", "domain": "people.googleapis.com"}]}}
```

Kontrola grantu: nové úplné čtení pod týmž grantem a hned po něm delta s jeho tokenem vrátily `200`.

- Vypršený token je `400 FAILED_PRECONDITION` s `ErrorInfo` `EXPIRED_SYNC_TOKEN`, ne `410`. Adaptér bere každý `400` i `410` na čtení s tokenem jako `CursorExpired`. Falešný Google (`Fault::expired_sync_token`) vrací přesně tento tvar.
- Delty platnost tokenu neprodlužují. Token B, vydaný deltou dva dny před pokusem, vypršel stejně jako A, tedy zhruba 7 dní po úplném čtení na začátku řetězu. Týdenní úplné čtení z kapitoly 6.3 proto nastává.
- Výhrada: oba tokeny vydal grant T10, pokus proběhl pod grantem K10. Google odpověděl „expired“, ne „invalid“. Že se token na grant neváže, kontrola přímo nedokazuje.

## 7. Kanál push kalendáře Google

Kanál z 2026-11-17 (instance K8) vypršel 24. 11. bez obnovy, protože instance K8 byla zastavená. Kanál instance K10 otevřela 2026-11-22 08:15 UTC s platností do 2026-11-29 08:15 UTC. Engine uložil job `renew_channel` na 2026-11-27 00:15 UTC, tedy po 2/3 platnosti. Job je v řídicí databázi, takže přežije restart. Obnovu včetně pushe po ní ověřil 17. 11. #38, K10 ji živě neopakovala: instance K10 se zastavila 2026-11-23 18:15 před termínem obnovy. Jeden kalendář (svátky) push odmítl (`400 pushNotSupportedForRequestedResource`). Engine ho pak jen dotazuje a odmítnutí zkouší znovu za 24 h, jak má.

## 8. Dopad

Žádná odchylka neměnila kontrakt; K10 nenašla nic, co by blokovalo K11. Meze dotazování zůstávají. D4 může u People nahradit odhad „několik minut“ naměřeným: propsání pod sekundu. Kapitola 6.3 a otevřené otázky 3 a 7 jsou ověřené. Položky K10 v iCloudu (`musubi-k10-*`) jsou smazané.

