# Postmortem — "Server list never loads" (May 2026)

**Status:** RESOLVED on 2026‑05‑13.
**Verified against:** TERA 31.04 EU retail client, server endpoint
`http://sd-5967132-W.dattaweb.com:8090`.

## 1. Symptoms

For months the new Rust launcher (`teralaunch.exe`) suffered from a
reproducible regression:

- Login worked.
- The launcher spawned `TERA.exe` successfully.
- The game intro played, the "Fate of Arun" splash appeared.
- The server selection dropdown stayed **empty forever**.
- The legacy `LAUNCHER PROD.exe` (BHS‑based binary distributed with the
  client) worked perfectly against the exact same `tera-api` backend.

`Binaries/tera_launch.log` (the game's own log) consistently complained that
the protobuf `ServerList` message it received had **"missing required fields"**.

## 2. What we thought was happening (and was wrong)

Over several iterations the launcher had been "fixed" multiple times based on
plausible‑sounding but incorrect assumptions:

1. **"It's a JSON vs XML issue."** Try sending the raw XML body of
   `/tera/ServerList` in event `0x6`. → Rejected by TERA.
2. **"BHS docs say event 6 carries a URL string."** Send the server‑list URL
   as a UTF‑16 LE blob. → TERA acknowledged the message but kept re‑asking
   event `0x5` in a tight loop.
3. **"There must be an `event 24` `\"Tera\"` handshake to wake the launcher
   up."** Send `WM_COPYDATA(dwData=24, "Tera")` from the launcher to TERA
   right after spawn. → No measurable effect.
4. **"Maybe it's a UIPI / integrity level issue."** Call
   `ChangeWindowMessageFilterEx(WM_COPYDATA, MSGFLT_ALLOW)`. → Necessary, but
   not sufficient.
5. **"Maybe it's a tokio runtime‑in‑runtime panic."** Pre‑fetch the server
   list off the message‑pump thread and serve from a cache. → Necessary, but
   still not sufficient.

Each fix removed *a* problem, but the dropdown stayed empty.

## 3. How we found the real answer

We built `fake-tera`, a Rust program that impersonates `TERA.exe` by:

1. Replacing `Binaries/TERA.exe` with itself (32‑bit, classes
   `LaunchUnrealUWindowsClient` + companions, visible off‑screen).
2. Capturing **every** Win32 message that arrived at its IPC window with
   timestamp + full hex dump, flushed line‑by‑line to `C:\fake_tera_ipc.txt`
   so a force‑kill cannot lose data.
3. Driving the `WM_COPYDATA` conversation from the TERA side and recording
   what the **real** classic launcher actually answered.

Two passive runs and three active runs were enough. The decisive capture is
saved in this repository for posterity at `docs/captures/2026-05-13-handshake.log`
(in spirit — see the [protocol doc](LAUNCHER-CLIENT-PROTOCOL.md#42-reference-dump)
for the actual bytes).

Combined with the public [Novadrop reference][novadrop], it produced a
complete and unambiguous description of the protocol.

## 4. Root causes (multiple, layered)

The "empty server list" was the convergence of four independent bugs:

### 4.1 Wrong wire format for `0x6`

TERA expects a **proto2 `ServerList` protobuf**, not XML, not a URL.

The launcher was sending the URL UTF‑16 blob because the old BHS integration
docs (a different doc, not Novadrop) describe a higher‑level abstraction
where the launcher returns "Server list static url" — but that's a different
layer; the wire protocol that `TERA.exe` actually speaks is protobuf.

### 4.2 proto3 instead of proto2

Even after switching to protobuf, the message was generated with `prost`
using **proto3 semantics**. proto3 has no concept of `required` and omits
default values from the wire output. The retail client uses **proto2** with
**every field marked `required`** and refuses any message that omits a
required field — exactly the "missing required fields" message in
`tera_launch.log`.

Fix:

- `teralib/src/serverlist.proto`: `syntax = "proto2"`, every field marked
  `required` (except `host`, which is `optional` because it's exclusive with
  `address`).
- `teralib/src/_serverlist_proto.rs`: re‑generated prost code with the
  matching attributes (`#[prost(fixed32, required, tag = "…")]`, …, and
  `host: Option<Vec<u8>>` instead of `Vec<u8>`).

### 4.3 Imaginary `event 24` handshake

A legacy comment in the codebase asserted that the launcher had to send
`event 24` with payload `"Tera"` (UTF‑16) to "wake" the game. The
spy captured the live classic launcher and proved that:

- The classic launcher never sends `event 24` to TERA.
- The classic launcher never sends *anything* until TERA sends `0x3e8`
  first.

The `"Tera"` token does exist in the binary, but it's not used in
`WM_COPYDATA` at the protocol level.

Fix: removed `send_tera_handshake()` and the watcher task that called it.
The launcher now sits passively after spawn and lets TERA initiate, exactly
like the classic launcher does.

### 4.4 Game‑start handling inverted

`handle_game_start()` (the handler for `0x3e8`) used to *push* the account
name, ticket and a synthetic `event 24` to TERA, on the theory that `0x3e8`
was an event the **launcher** sent to the game. The protocol is the
opposite: `0x3e8` is the very first message **TERA sends to the launcher**
to announce it's alive, and the launcher does not reply.

Pushing data on receipt of `0x3e8` confused TERA's state machine on some
runs.

Fix: `handle_game_start()` now just logs the event and returns. The actual
data lives in the responses to `0x1`, `0x3`, `0x5`.

## 5. What the fix looks like

Summary of the diff that ships the fix (commit `f79607d`):

```
 teralib/Cargo.toml               |   4 +-   (added quick-xml + serde derive)
 teralib/src/serverlist.proto     |  28 +--   (proto3 → proto2, required fields)
 teralib/src/_serverlist_proto.rs |  28 +--   (matching prost attrs)
 teralib/src/config/config.json   |  10 +-   (URLs → sd-5967132-W.dattaweb.com:8090)
 teralib/src/game/mod.rs          | 415 +++   (XML→protobuf, drop event 24, rewire handlers)
 teralaunch/src-tauri/Cargo.lock  |  98 ±    (transitive deps)
 teralaunch/src-tauri/src/main.rs |  23 +-   (file logging integration)
 teralaunch/src-tauri/src/file_log.rs | new   (persistent log to %APPDATA%)
 teralib/Cargo.lock               |  36 ±    (transitive deps)
```

Key code landmarks in `teralib/src/game/mod.rs`:

- `prefetch_server_list()` — downloads `SERVER_LIST_URL`, auto‑detects JSON
  vs XML (`Content-Type` + first non‑whitespace byte), routes to
  `parse_server_list_json` or `parse_server_list_xml`, encodes the resulting
  `ServerList` to protobuf bytes, returns them.
- `parse_server_list_xml()` — quick‑xml `Deserialize`r for the retail XML
  format documented in [`LAUNCHER-CLIENT-PROTOCOL.md`](LAUNCHER-CLIENT-PROTOCOL.md#43-mapping-the-tera-api-xml-feed).
- `handle_account_name_request()`, `handle_session_ticket_request()`,
  `handle_server_list_request()` — straightforward request/response, no
  more side‑effects that try to "kickstart" the conversation.

## 6. Verification

After deploying the rebuilt `teralaunch.exe`:

- Game launches with the Rust launcher.
- Server dropdown populates with the correct name, category and population.
- Player can connect to the arbiter server and enter the lobby.
- `tera_launch.log` no longer logs the "missing required fields" error.
- The user, who has been chasing this bug for months, confirmed the fix with
  the universally‑recognised happy‑path message: *"FUNCIONO TE AMO"*.

## 7. How to prevent regressions

1. Treat `teralib/src/serverlist.proto` as **load‑bearing**. Any edit must
   preserve `syntax = "proto2"` and the `required` qualifier on every field
   except `host`.
2. Treat `teralib/src/_serverlist_proto.rs` as part of the protocol surface,
   not as throw‑away generated code. The hand‑edited `required`/`optional`
   prost attributes are intentional.
3. If you change the XML/JSON ingestion, run the fake‑tera spy against the
   classic launcher and confirm the protobuf bytes you produce are
   byte‑equivalent to the ones the classic launcher produces for the same
   server config.
4. Never re‑introduce a launcher → TERA "kick" message after spawn. Tera is
   the initiator. Period.

## 8. Tools left behind

- **`fake-tera/`** — the spy that captures the live IPC conversation. See
  `fake-tera/README.md`. Useful for any future protocol question.
- **`fake-tera/tera_spy.ps1`** — install/restore/view helper script around
  the spy.
- **`teralaunch/src-tauri/src/file_log.rs`** — persistent logging shipped
  alongside this fix so the next bug is easier to debug.

## 9. References

- [`docs/LAUNCHER-CLIENT-PROTOCOL.md`](LAUNCHER-CLIENT-PROTOCOL.md) — full
  protocol spec.
- [Novadrop — Launcher/Client Protocol][novadrop] — third‑party RE doc.
- [`tera-launcher`][tera-launcher] / [`tera-api`][tera-api] — upstream
  ecosystem this client targets.

[novadrop]: https://docs.vezel.dev/novadrop/game/launcher-client-protocol
[tera-launcher]: https://github.com/justkeepquiet/tera-launcher
[tera-api]: https://github.com/justkeepquiet/tera-api
