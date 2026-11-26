# BEP 3: HTTP tracker announce - field notes

Status: planned for v0.1 (HTTP and HTTPS announce; UDP is BEP 15).
Source: https://www.bittorrent.org/beps/bep_0003.html
Reviewed: 2026-11-26

This is a working summary in our own words, not a copy of the BEP. Each
statement is labelled "BEP says" (checked against the BEP 3 text) or
"Our decision" (an implementation choice that the BEP does not require).
`bep_0003.md` covers bencode and metainfo and deliberately leaves tracker
behaviour out of scope; this note covers the HTTP tracker fields.

## Request

- BEP says: an announce is an HTTP `GET` to the tracker URL, with the
  announce parameters in the query string.
- BEP says: `info_hash` is the 20-byte SHA-1 of the bencoded `info`
  dictionary, percent-encoded in the query.
- BEP says: `peer_id` is the 20-byte peer ID.
- BEP says: `port` is the client's TCP listen port.
- BEP says: `uploaded`, `downloaded` and `left` are base-ten byte counts.
- BEP says: `event` is `started`, `completed`, `stopped`, or omitted for a
  periodic announce.
- BEP says: `compact=1` asks for compact peers; BEP 23 defines the compact
  form and says `compact=0` asks for the list form. The key is advisory.
- BEP says: `numwant` is optional and may be zero.
- BEP says: `key` is an optional client key that is not shared with peers.
- BEP says: `trackerid` is optional and echoes a value the tracker returned.
- BEP says: `ip` is optional and normally not needed.
- Our decision: the query order is `info_hash`, `peer_id`, `port`,
  `uploaded`, `downloaded`, `left`, `compact=1`, `event` when not `none`,
  `numwant`, `key`, `trackerid` when the tracker returned one.
- Our decision: `info_hash` and `peer_id` are percent-encoded byte by byte
  with uppercase hex, never through a UTF-8 string. Every byte outside the
  unreserved set `A-Z a-z 0-9 - . _ ~` is escaped. A space is `%20`, never
  `+`; a literal `+` is `%2B` and a literal `%` is `%25`.
- Our decision: an existing query in the tracker URL is preserved and
  extended with `&`, never a second `?`. Existing `info_hash`, `peer_id`,
  `port`, `uploaded`, `downloaded`, `left`, `compact`, `event`, `numwant`,
  `key`, `trackerid`, `ip`, `ipv4` and `ipv6` parameters are removed first.
  Names are matched after percent-decoding and case-insensitively, so
  `%69nfo_hash` and `info%5Fhash` cannot compete with our values.
- Our decision: `numwant` is 50 for `started`, `completed` and periodic
  announces, and 0 for `stopped`.
- Our decision: `key` is a 32-bit value from OS randomness, generated once per
  torrent session and reused for HTTP and UDP.
- Our decision: `trackerid` is sent only to the HTTP tracker that returned it
  and is dropped when the active tracker changes.
- Our decision: `ip`, `ipv4` and `ipv6` are never sent.

## Response

- BEP says: the response body is a bencoded dictionary.
- BEP says: `failure reason` means the announce failed, and no other keys
  need to be processed.
- BEP says: `warning message` is advisory and the response is still
  processed normally.
- BEP says: `interval` is the number of seconds until the next regular
  announce.
- BEP says: `min interval` is the minimum announce interval; a client must
  not reannounce more often than this.
- BEP says: `tracker id` is an opaque string that the client should send back
  on its next announce.
- BEP says: `complete` is the number of seeders and `incomplete` is the
  number of leechers.
- BEP says: `peers` is either a list of dictionaries with `peer id`, `ip` and
  `port`, or a compact string; BEP 23 defines the compact form.
- BEP says (BEP 7): `peers6` is an optional compact IPv6 peer list.
- Our decision: the body must be a bencode dictionary and is capped at 1 MiB
  before decoding; the existing bounded decoder is used.
- Our decision: a missing `interval` means 1800 seconds. The effective
  interval is `clamp(max(interval, min_interval), 60, 86400)`. An interval of
  0 or a negative value is treated as missing.
- Our decision: `complete` and `incomplete` are optional non-negative
  counters. Unknown keys are ignored.
- Our decision: `tracker id` is ignored if longer than 256 bytes and held in a
  `TrackerId` newtype otherwise. It is echoed only to the tracker that issued
  it, is never displayed, and its `Debug` and `Display` forms print only its
  length.
- Our decision: `peers` and `peers6` are decoded per the BEP 23 and BEP 7
  notes. The merged peer set is capped at 200 per response.
- Our decision: the `Content-Type` header is ignored. Bencode validity is the
  only test. A `Content-Encoding` other than `identity` is a malformed
  response.

## Errors and redaction

- Our decision: `failure reason` and `warning message` are tracker-supplied
  text. They are sanitized best-effort before they may be shown in the UI:
  UTF-8 lossy, control characters stripped, 256-byte cap, and every path
  segment, query name, query value and fragment of the announce URL that is
  at least 4 bytes replaced with `<redacted>`, case-insensitively and after
  percent-decoding. Tokens shorter than 4 bytes are not replaced; this is a
  documented limit of the sanitizer.
- Our decision: logs and errors never contain tracker-supplied text. They
  record only a length and a classification (for example
  `tracker message, 42 bytes`). The UI calls an explicit `sanitized_text()`
  accessor; `Debug` and `Display` print only the length.
- Our decision: only the numeric HTTP status is retained. The reason phrase
  and all header values are discarded, except `Location`, which is parsed
  into a `TrackerUrl` before any diagnostic.
- Our decision: a malformed URL or `Location` produces a fixed placeholder,
  never the raw input.

## Tests

- Our decision: request vectors assert every field, the query order, the
  `?`/`&` rule, the removal of duplicate parameters by decoded name, and
  byte-wise percent-encoding.
- Our decision: response vectors cover `failure reason`, `warning message`,
  `interval`, `min interval`, `complete`, `incomplete`, `tracker id`,
  unknown keys, a missing interval, and the interval clamp.
- Our decision: sanitizer tests cover a passkey in the query, a path segment
  of 3 and 4 bytes, a fragment, a percent-encoded echo, and a passkey in the
  HTTP reason phrase.
- Our decision: `Debug` and `Display` of `TrackerId`, `TrackerMessage`,
  `AnnounceRequest` and `AnnounceResponse` never contain the passkey.
