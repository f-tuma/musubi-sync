# Recorded Google responses (K8)

K8 fills this directory with real Google Calendar responses, captured once
against a test account and scrubbed of personal data. The test
`tests/google_fixtures.rs` sends the matching request to the fake server and
fails if a recorded body has a key, at any depth, that the fake's response
lacks (arrays compare their first elements). Paths the fake knowingly lacks
are in the test's `DOCUMENTED` allow-list, each with a `FIDELITY.md` line.

One file per response, named by endpoint:

| File | Request |
| --- | --- |
| `calendarList.json` | `GET users/me/calendarList` |
| `events.list.json` | `GET calendars/primary/events` (full read, first page) |
| `events.list.410.json` | `GET calendars/primary/events?syncToken=<expired>` |
| `token.json` | `POST https://oauth2.googleapis.com/token` (`refresh_token` grant) |

A new file name needs a matching arm in `tests/google_fixtures.rs`. Keys the
fake has and Google does not, and any value that differs, go in
`FIDELITY.md`.
