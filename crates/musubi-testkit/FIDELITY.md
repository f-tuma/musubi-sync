# Fake Google server: fidelity

`src/google.rs` mimics Google Calendar API v3 and Google's OAuth token
endpoint closely enough for the Google adapter's transport tests (K4) and the
crash tests (plan 6.2). This file lists every place where it knowingly
behaves differently from Google's public documentation. Behaviour the
documentation does not settle is listed too, marked *undocumented*; K8
measures those against a real account.

Sources: the API reference for `calendarList.list`, `events.list`,
`events.get`, `events.watch`, `channels.stop`, the "Synchronize resources
efficiently" and "Push notifications" guides, and the OAuth 2.0
"Using OAuth 2.0 for Web Server Applications" guide.

## Scope

- **One account per server.** The primary calendar is always
  `fake.user@example.com`. Tests with several accounts start several servers.
- **Read and watch endpoints only.** No `events.insert/patch/delete`,
  `calendars.*`, `acl.*`, `colors`, `settings` or `freebusy`. Data changes go
  through the Rust API (`put_event`, `delete_event`, `apply`, `run_script`).
- **Plain HTTP/1.1 on 127.0.0.1**, no TLS, no gzip, no HTTP/2.
- **Dropping `FakeGoogle` does not close open keep-alive connections.** It
  stops accepting new ones and stops sending notifications; a connection a
  client already holds keeps being answered until the client closes it.
- **No permissions model.** A `reader` calendar answers like an `owner` one;
  `accessRole` is only reported.

## Query parameters

- Parameters the fake does not model are rejected with `400 badRequest`
  instead of being honoured: `singleEvents=true`, `q`, `orderBy`,
  `updatedMin`, `iCalUID`, `privateExtendedProperty`,
  `sharedExtendedProperty`, `maxAttendees`, `showHiddenInvitations`,
  `eventTypes`, and `syncToken` on `calendarList.list`. This keeps the
  adapter from relying on behaviour nobody tests.
- `alt`, `prettyPrint`, `fields`, `quotaUser` and `timeZone` are accepted
  and **ignored**. In particular `fields` does not trim the response, and
  `timeZone` does not change the zone of the output.
- `calendarList.list` accepts `showDeleted`, `showHidden` and
  `minAccessRole` but ignores them.
- `events.get` ignores all query parameters.

## Paging and sync tokens

- `maxResults`: the default is 250 and the cap is 2500 for events; the
  default is 100 and the cap is 250 for the calendar list. This matches the
  docs. `Config::max_page_size` lowers the events page further, which Google
  is documented to do at will.
- **Page tokens carry the original query.** Parameters sent next to
  `pageToken` are ignored. Google expects the same parameters to be repeated
  (*undocumented* what happens if they differ).
- **Page tokens never expire.** Google's can (*undocumented* lifetime).
- **Sync tokens expire only when scripted** (`Change::ExpireSyncTokens`).
  Google expires them after an unspecified time, or after ACL and other
  server-side changes.
- **An unreadable sync token, or one for another calendar, answers `410
  fullSyncRequired`**, the same as an expired one. Google's answer to a
  malformed token is *undocumented*. A malformed page token answers `400`.
- **Delta passes are snapshot-bounded.** An event changed after the first
  page of a delta pass is not served in that pass; it shows up in the next
  delta. Google gives no guarantee either way, so a correct client copes
  with both.
- **The `nextSyncToken` of a full read covers changes from the read's
  start.** A change made during paging can arrive twice: once in the read,
  once in the next delta. This is consistent with Google's documented
  at-least-once delta behaviour.
- `Config::empty_first_page` makes the first response of every pass (full,
  window or delta) an empty page with `nextPageToken`, and
  `Config::empty_page_every = n` adds one after every `n` pages with items,
  so empty pages also appear in the middle of a read. Google returns empty
  pages at arbitrary points; the fake's are at fixed ones.
- **A window read (`timeMin`/`timeMax`) ends with a `nextSyncToken`**, like
  a full read. Whether Google returns one there, and what a delta from it
  covers, is *undocumented*.
- **`calendarList.list` never returns `nextSyncToken`**, on its last page
  either; Google does. The fake also rejects `syncToken` there (above), so
  calendar-list deltas are not modelled. K8 records Google's last page.

## Time windows

- `timeMin` bounds an event's end (exclusive) and `timeMax` bounds its start
  (exclusive). Both must be RFC 3339 timestamps with an offset. This matches
  the docs.
- **Recurring masters are treated as unbounded** (they match any window
  that ends after their first start). Google matches a master only if some
  instance falls in the window, which needs RRULE expansion.
- **All-day dates are read as UTC midnight** for windowing. Google uses the
  calendar's time zone.
- A `dateTime` without an offset is resolved in the event's `timeZone`
  (IANA name, bundled tz database), as Google documents. A non-cancelled
  event whose start is not a `date`, a `dateTime` with an offset, or a
  local `dateTime` with a known `timeZone` is a test bug: `put_event` panics.
- A cancelled instance spans one second at its `originalStartTime`.

## Events and deletions

- `etag`, `created` and `updated` come from a logical clock (`Config::epoch`
  plus one millisecond per change), not the wall clock, so they are
  reproducible from the seed. Etags have Google's shape, a quoted decimal
  number, but no relation to Google's values.
- Server-filled defaults: `status`, `htmlLink`, `iCalUID`, `sequence`,
  `creator`, `organizer` (both the account's primary address on every
  calendar), `reminders`, `eventType`. `sequence` never increments.
- **Deleted events keep only `kind`, `etag`, `id` and `status:"cancelled"`**,
  plus `recurringEventId` and `originalStartTime` for an instance. The docs
  guarantee only `id`; real responses often carry more.
- **Tombstones are kept forever.** Google drops them after an *undocumented*
  time (about 30 days observed), after which a sync token from before the
  deletion must get `410`.
- With `showDeleted=false`, cancelled instances of a series are still
  listed. This matches the documented behaviour for `singleEvents=false`.
- `events.get` returns `200` with `status:"cancelled"` for a deleted event
  and `404 notFound` for an unknown one. Google may also answer `410` for an
  event deleted long ago.
- **Deleting a recurring master leaves its exceptions (moved or changed
  instances) `confirmed`.** Only the master becomes a tombstone. Whether
  Google also cancels the exceptions in a delta is *undocumented*.
- The events list `description` is always `""` and `updated` is the time of
  the calendar's last change.
- Seeded timed events alternate between `UTC`, `Europe/Prague` and
  `America/New_York`: their `dateTime` carries that zone's offset (`Z`,
  `+01:00`/`+02:00`, `-05:00`/`-04:00`), and every tenth event is a local time
  without an offset, resolved through `timeZone`.

## Push channels

- **`http://` callback addresses are accepted and delivered to.** Google
  requires HTTPS with a valid certificate. Other schemes fail delivery.
- **Deliveries go only to loopback hosts** (`localhost`, `127.0.0.0/8`,
  `[::1]`), so a test can never reach the network. A watch with any other
  address is accepted, but each of its deliveries is recorded as failed
  without connecting. Google delivers to any public HTTPS host.
- Expiration is `min(requested expiration, now + params.ttl or
  default_watch_ttl, now + max_watch_ttl)`. Both default to 604 800 s
  (7 days). Google's actual maximum is *undocumented* (T8 measures it).
  Channel expiration uses the wall clock, not the logical clock.
- `type` accepts both `web_hook` and `webhook`. A repeated channel id
  answers `400`.
- Messages: `sync` (number 1) right after the watch, then one `exists` per
  change, **never coalesced, delayed or reordered**. Google may coalesce or
  delay. Messages go to one channel after another, in change order, from a
  single sender.
- **One delivery attempt, no retry.** Google retries some failures with
  exponential backoff. Every attempt is recorded in `FakeGoogle::deliveries`.
- Expired channels are forgotten, as by Google: they leave
  `FakeGoogle::channels`, their id can be reused, and stopping one answers
  `404 notFound`.
- `channels.stop` with a wrong `id`/`resourceId` pair answers `404
  notFound`. Stopping does not need the token.
- Headers sent: `X-Goog-Channel-ID`, `X-Goog-Channel-Token` (when set),
  `X-Goog-Channel-Expiration` (RFC 1123), `X-Goog-Resource-ID`,
  `X-Goog-Resource-URI`, `X-Goog-Resource-State`, `X-Goog-Message-Number`.
  `X-Goog-Changed` is never sent; Calendar does not send it either.
- Removing a calendar drops its channels without a `not_exists` message.

## OAuth token endpoint

- Grants: `refresh_token` and `authorization_code`; anything else answers
  `unsupported_grant_type`. `client_secret` and `scope` are not validated;
  `client_id` is checked only against the authorization code.
- **The consent screen grants at once.** `GET /o/oauth2/v2/auth` answers
  `302` to `redirect_uri?code=…&state=…` without a login or consent page. It
  requires `response_type=code` and PKCE with `code_challenge_method=S256`
  (Google also accepts `plain` and no PKCE) and ignores `scope`,
  `access_type`, `prompt` and `login_hint`. Errors answer the token
  endpoint's JSON `400`; Google shows an HTML error page instead.
- **Authorization codes are spent by their first redemption**, failed ones
  included, and never expire by time (Google: about 10 minutes,
  *undocumented*). A wrong `redirect_uri` answers `redirect_uri_mismatch`, a
  wrong `code_verifier` `invalid_grant`.
- **The `id_token` is not signed** (the signature part is a placeholder).
  Its claims are `iss`, `aud`, `sub` (`Config::subject`), `iat` and `exp`;
  Google adds `azp`, `at_hash`, `email` and more depending on the scopes.
  The response always carries the current `refresh_token`; Google returns
  one only on first consent or with `prompt=consent`.
- **Access tokens never expire by time.** `expires_in` is always `3599`;
  `FakeGoogle::expire_access_tokens` invalidates all of them at once.
- `scope` in the response is always `calendar.readonly`.
- Errors, scripted faults included, use OAuth's shape
  `{"error","error_description"}`. A scripted fault's `reason` (for example
  `rateLimitExceeded`) becomes `error` and its message `error_description`;
  Google's OAuth server uses its own codes for overload, *undocumented*.
- Rotation (`Config::rotate_refresh_tokens`) returns a new `refresh_token`
  and revokes the old one at once. Google rarely rotates and keeps the old
  token valid for a while.

## Errors

- Error bodies use the classic shape `{"error":{"errors":[{"domain",
  "reason","message"}],"code","message"}}`. Google also sends
  `error.status` (for example `UNAUTHENTICATED`) and, for some errors,
  `location`/`locationType` in `errors[0]`. The fake omits them; they are
  the `DOCUMENTED` allow-list in `tests/google_fixtures.rs`.
- Unknown paths and wrong methods answer Google's JSON `404 notFound`. They
  are logged in `FakeGoogle::requests` but skip latency and faults.
- **Requests the HTTP layer cannot parse get axum's plain-text answers**,
  not Google's JSON: a query string that does not decode answers `400`, and
  a token request without `application/x-www-form-urlencoded` answers `415`.
  These are not logged and skip latency and faults. They mean a broken
  client, which Google would also reject.
- Rate limits, backend errors and their `Retry-After` happen **only when
  scripted** with `FakeGoogle::fail`. Organic quotas are not modelled.
- `Config::latency` is added before the fault check, so a scripted error
  is just as slow as a success.

## Quirks

A subscribed public holiday calendar (`accessRole: reader`) put the engine
into a full-read loop against real Google (12. 11. 2026). The response bodies
were not logged, so the cause is unknown. `FakeGoogle::quirk` makes one
calendar behave in each of the suspected ways. Each is **unverified; confirm
in K8**. The adapter now logs the status and the first 300 bytes of the body
for each case, so K8 can record Google's actual answer.

- `Quirk::RefuseWatch`: `events.watch` answers `400 push
  pushNotSupportedForRequestedResource`. Google refuses watches on some
  calendars it does not own; the status and reason are **unverified; confirm
  in K8**.
- `Quirk::ExpireEverySyncToken`: every `syncToken` answers `410
  fullSyncRequired`, even the one the full read just returned. That Google
  does this for any calendar is **unverified; confirm in K8**.
- `Quirk::NoSyncToken`: the last page of a read has no `nextSyncToken`.
  Google documents one on every last page; whether it is missing for some
  calendars is **unverified; confirm in K8**.
