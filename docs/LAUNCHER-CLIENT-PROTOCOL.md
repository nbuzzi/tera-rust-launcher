# TERA Launcher ↔ Client IPC Protocol

Reverse‑engineered reference of the Win32 protocol the BHS retail launcher
uses to talk to `TERA.exe`. This is what the Rust launcher implements and
what enables the in‑game server‑list dropdown to populate.

The protocol was originally derived from the public Novadrop documentation
(<https://docs.vezel.dev/novadrop/game/launcher-client-protocol>) and then
**verified empirically against the real `LAUNCHER PROD.exe` shipped with the
TERA 31.04 EU retail client** using the `fake-tera` spy tool (see
[`/fake-tera/README.md`](../../fake-tera/README.md) in the parent repo).

> **TL;DR** — TERA is the active side. It opens a window, sends a single
> `0x3e8 GameStartNotification`, and then synchronously asks for the account
> name (`0x1`), session ticket (`0x3`) and server list (`0x5`). The launcher
> replies in lockstep with `0x2`, `0x4`, `0x6`. The server list **must** be a
> proto2 protobuf with all `required` fields populated — not XML, not a URL.

---

## 1. Roles

In the canonical 3‑process retail setup:

| Process          | Role                                                                                       |
| ---------------- | ------------------------------------------------------------------------------------------ |
| `launcher.exe`   | Publisher‑specific launcher. Handles auth and resolves the server‑list URL.                |
| `Tl.exe`         | Publisher‑agnostic intermediary that bridges binary IPC ↔ text IPC.                        |
| `TERA.exe`       | The game client. Always the initiator of the binary protocol.                              |

In this private server setup we ship a **single binary** (the Rust
`teralaunch.exe`, or the legacy `LAUNCHER PROD.exe`) that consolidates the
roles of both `launcher.exe` and `Tl.exe`. It must therefore implement the
binary side of the protocol that `TERA.exe` expects.

---

## 2. Transport

All inter‑process messages travel on top of standard Windows window
messages — specifically [`WM_COPYDATA`][wm-copydata] (`0x004A`).

The `COPYDATASTRUCT` is interpreted as:

```c
typedef struct tagCOPYDATASTRUCT {
    ULONG_PTR dwData;   // ← message ID (a.k.a. "event")
    DWORD     cbData;   // ← payload length in bytes
    PVOID     lpData;   // ← payload pointer
} COPYDATASTRUCT;
```

- `dwData` is the protocol message ID (called *event* in the legacy code).
- The reply travels back through a second `WM_COPYDATA` using a different
  message ID — there is no implicit `SendMessage` return value beyond
  Windows' own ack.
- Everything is little‑endian. Strings are either UTF‑8 or UTF‑16 LE
  depending on the message; details below.

[wm-copydata]: https://learn.microsoft.com/en-us/windows/win32/dataxchg/wm-copydata

### 2.1 Window classes and discovery

- `TERA.exe` registers its IPC window with class **`LaunchUnrealUWindowsClient`**.
  This is confirmed by both the live game and by the spy after impersonation.
- The legacy classic launcher registers its window with class **`LAUNCHER_CLASS`**.
- The Rust launcher uses its Tauri main window as the IPC endpoint.
- The launcher locates the game via `EnumWindows` filtering by PID, *not* by
  class name. The game locates the launcher via `FindWindowW("LAUNCHER_CLASS", NULL)`.

### 2.2 UIPI / message filter

When the launcher runs at a different integrity level than the game (e.g.
the launcher is elevated but `TERA.exe` is not), Windows silently drops
`WM_COPYDATA` between them. The launcher window must therefore allow the
message explicitly:

```rust
ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, NULL);
```

Failing to do this manifests as the classic "stuck on the Fate of Arun splash
with empty server list" symptom even though everything else is wired up
correctly.

---

## 3. Message catalogue

The legacy code calls every ID an *event*. The full list:

| ID (hex / dec) | Direction          | Name                              | Payload                                                                                                |
| -------------: | ------------------ | --------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `0x1` / 1      | TERA → launcher    | Account Name Request              | empty                                                                                                  |
| `0x2` / 2      | launcher → TERA    | Account Name Response             | `u16string` (UTF‑16 LE, **no** NUL)                                                                    |
| `0x3` / 3      | TERA → launcher    | Session Ticket Request            | empty                                                                                                  |
| `0x4` / 4      | launcher → TERA    | Session Ticket Response           | `u8string` (raw UTF‑8 bytes, **no** NUL — typically a 36‑byte UUID v4)                                 |
| `0x5` / 5      | TERA → launcher    | Server List Request               | `int32 sort_criterion` (LE)                                                                            |
| `0x6` / 6      | launcher → TERA    | Server List Response              | **proto2 protobuf `ServerList`** (see §4)                                                              |
| `0x7` / 7      | TERA → launcher    | Enter Lobby / Enter World         | empty = lobby; `u16string character_name` (NUL‑terminated) = world                                     |
| `0x8` / 8      | launcher → TERA    | Lobby/World Ack                   | echoes the payload of `0x7`                                                                            |
| `0x19` / 25    | TERA → launcher    | Open Website Command              | `uint32 id`                                                                                            |
| `0x1a` / 26    | TERA → launcher    | Web URL Request                   | `uint32 id` then `u16string arguments` (NUL‑terminated)                                                |
| `0x1b` / 27    | launcher → TERA    | Web URL Response                  | `uint32 id` then `u16string url` (NUL‑terminated; `""` or `"|"` = reject)                              |
| `0x3e8` / 1000 | TERA → launcher    | **Game Start Notification**       | `u32 source_revision` ‖ `u32 unknown_1` ‖ `u16string windows_account_name` (NUL‑terminated). Required! |
| `0x3e9..=0x3f8` (1001..1016) | TERA → launcher | Game Event Notification          | (see §5)                                                                                              |
| `0x3fc` / 1020 | TERA → launcher    | Game Exit Notification            | `u32 length=12` ‖ `u32 process_exit_code` ‖ `u32 reason` (see §6)                                      |
| `0x3fd` / 1021 | TERA → launcher    | Game Crash Notification           | `u16string details` (no NUL)                                                                           |
| `0x3fe..0x400` (1022..1024) | TERA → launcher | Anti‑Cheat Notifications          | starting / started / error (`u64 error_code` for the last one)                                         |
| `0x401` / 1025 | TERA → launcher    | Open Support Website              | empty                                                                                                  |

### 3.1 Required handshake order

`TERA.exe` always drives the conversation. The minimal flow for a successful
"reach the lobby" run is:

```text
TERA → launcher : 0x3e8  (Game Start Notification)
TERA → launcher : 0x1    (Account Name Request)
launcher → TERA : 0x2    (Account Name UTF-16)
TERA → launcher : 0x3    (Session Ticket Request)
launcher → TERA : 0x4    (Ticket raw UTF-8)
TERA → launcher : 0x5    (Server List Request, sort=-1|0|1|2|4)
launcher → TERA : 0x6    (Server List protobuf)
TERA → launcher : 0x7    (Enter Lobby on user "play")
launcher → TERA : 0x8    (Echo)
```

`0x3e8` is **not** acknowledged by the launcher. The classic launcher in
particular *does not* send anything proactively — if your IPC implementation
expects the launcher to initiate, it will wait forever. This was the root
cause of the long‑standing "empty server list" bug fixed in May 2026.

> Important: there is **no `event 24` handshake**. Earlier iterations of the
> Rust launcher sent `WM_COPYDATA(dwData=24, "Tera")` because that token
> appears in the launcher binary, but the live spy capture proved the real
> retail launcher never sends or expects it. The `0x3e8` notification is the
> "I'm here" signal.

### 3.2 Game Start Notification (`0x3e8`) payload

```text
offset  size  field                       notes
------  ----  --------------------------  ------------------------------------------------
0       4     source_revision (u32 LE)    "SrcRegVer" from Binaries/ReleaseRevision.txt
4       4     unknown_1 (u32 LE)          purpose unknown, typically 0
8       N*2   windows_account_name        UTF-16 LE, NUL-terminated (GetUserNameW)
```

For the 31.04 EU retail build shipped with this server, `source_revision =
290066` (`0x46D52`). The full file lives at
`Binaries/ReleaseRevision.txt`:

```text
SrcRegVer: 290066
ReleaseName: LIVE-31.04 EME #44 (Milestone build)
```

### 3.3 Server List Request (`0x5`) sort criterion

`sort_criterion` is a signed 32‑bit little‑endian integer:

| Value | Meaning                                              |
| ----: | ---------------------------------------------------- |
| `-1`  | `NONE` — launcher may pick any order                 |
| `0`   | By character count                                   |
| `1`   | By category                                          |
| `2`   | By name                                              |
| `4`   | By population                                        |

The launcher should:

- Sort stably.
- Maintain the previous order across requests and refine it on each call,
  unless `-1` is sent (which resets).
- If the same non‑`NONE` value is sent twice in a row, reverse the previous
  order instead of re‑sorting.
- Echo the received `sort_criterion` back in the protobuf response.

---

## 4. Server List wire format (proto2)

The response payload for `0x6` is **protocol‑buffers proto2** bytes — not
JSON, not XML, not a URL. The retail launcher emits `proto2` with `required`
fields, which is significant because `prost`/`proto3` will silently drop
zero‑valued required fields and the client will then refuse the response
with `"missing required fields"` in `tera_launch.log`.

### 4.1 Schema

```protobuf
syntax = "proto2";

message ServerList {
    message ServerInfo {
        required fixed32 id                  = 1;
        required bytes   name                = 2;   // u16string, no NUL
        required bytes   category            = 3;
        required bytes   title               = 4;
        required bytes   queue               = 5;
        required bytes   population          = 6;
        required fixed32 address             = 7;   // IPv4 BE-packed
        required fixed32 port                = 8;
        required fixed32 available           = 9;   // 0|1 (boolean)
        required bytes   unavailable_message = 10;
        optional bytes   host                = 11;  // exclusive with address
    }

    repeated ServerInfo servers         = 1;
    required fixed32    last_server_id  = 2;
    required fixed32    sort_criterion  = 3;
}
```

Field rules:

- `id` must be ≥ 1.
- `port` must fit in `uint16`.
- `available` is really a boolean (`0` or `1`).
- **Either** `address` **xor** `host` must be set. `address == 0` is treated
  as "not set". A non‑resolved hostname should be sent as `host`, never as
  `address`.
- All `bytes` fields are UTF‑16 LE strings without a NUL terminator. Yes,
  even though the field type is `bytes`.

### 4.2 Reference dump

Captured straight off the wire with the spy when the classic launcher served
a one‑server config (HEX, 227 bytes):

```text
0A D6 01 0D 0A 00 00 00 12 18 54 00 65 00 72 00
61 00 20 00 50 00 72 00 69 00 76 00 61 00 74 00
65 00 1A 06 50 00 76 00 50 00 22 20 54 00 65 00
72 00 61 00 20 00 50 00 72 00 69 00 76 00 61 00
74 00 65 00 20 00 28 00 31 00 29 00 2A 04 4E 00
6F 00 32 40 3C 00 66 00 6F 00 6E 00 74 00 20 00
63 00 6F 00 6C 00 6F 00 72 00 3D 00 22 00 23 00
30 00 30 00 66 00 66 00 30 00 30 00 22 00 3E 00
4C 00 6F 00 77 00 3C 00 2F 00 66 00 6F 00 6E 00
74 00 3E 00 3D C2 75 2B B3 45 79 1E 00 00 4D 01
00 00 00 52 34 57 00 65 00 6C 00 63 00 6F 00 6D
00 65 00 20 00 74 00 6F 00 20 00 54 00 45 00 52
00 41 00 20 00 44 00 69 00 6D 00 65 00 6E 00 73
00 69 00 6F 00 6E 00 21 00 15 0A 00 00 00 1D 00
00 00 00
```

Decoded:

- 1 × `ServerInfo`:
  - `id = 13`
  - `name = "Tera Private"` (UTF‑16)
  - `category = "PvP"`
  - `title = "Tera Private (1)"`
  - `queue = "No"`
  - `population = "<font color=\"#00ff00\">Low</font>"`
  - `address = 0x2B75C23D` (= `179.43.117.194`, IPv4 BE)
  - `port = 7801` (`0x1E79`)
  - `available = 1`
  - `unavailable_message = "Welcome to TERA Dimension!"`
- `last_server_id = 10`
- `sort_criterion = 0`

### 4.3 Mapping the tera‑api XML feed

The retail XML feed served at e.g.
`http://<api-host>:8090/tera/ServerList?lang=en&sort=3` looks like:

```xml
<?xml version="1.0" encoding="utf-8"?>
<serverlist>
  <server>
    <id>10</id>
    <ip>179.43.117.194</ip>
    <port>7801</port>
    <category sort="1">PvP</category>
    <name raw_name="Tera Private"><![CDATA[Tera Private]]></name>
    <crowdness sort="1">No</crowdness>
    <open sort="1"><![CDATA[<font color="#00ff00">Low</font>]]></open>
    <permission_mask>0x00000000</permission_mask>
    <server_stat>0x00000000</server_stat>
    <popup><![CDATA[Welcome to TERA Dimension!]]></popup>
    <language>en</language>
  </server>
</serverlist>
```

XML ↔ protobuf mapping used by `prefetch_server_list()`:

| XML element             | Protobuf field             | Encoding                       |
| ----------------------- | -------------------------- | ------------------------------ |
| `<id>`                  | `id`                       | parse as decimal `u32`         |
| `<ip>`                  | `address`                  | `Ipv4Addr` → `u32::from_be_bytes` |
| `<port>`                | `port`                     | decimal `u32`                  |
| `<category>`            | `category`                 | UTF‑16 of inner text           |
| `<name raw_name="…">`   | `name`                     | UTF‑16 of `raw_name`           |
| `<name>…</name>` + `(n)`| `title`                    | UTF‑16 of `"<raw_name> (n)"`   |
| `<crowdness>`           | `queue`                    | UTF‑16 of inner text           |
| `<open>`                | `population`               | UTF‑16 of inner text           |
| `<popup>`               | `unavailable_message`      | UTF‑16 of inner text           |
| `lastLoginServer`       | `ServerList.last_server_id`| from the credentials string    |
| derived                 | `sort_criterion`           | echo whatever TERA sent in `0x5` |

`available` is `1` when `<open>` is present and `address != 0`, else `0`.

---

## 5. Game event IDs (`0x3e9..0x3f8`)

```cpp
enum LauncherGameEvent : uint32_t {
    ENTERED_INTO_CINEMATIC           = 1001,
    ENTERED_SERVER_LIST              = 1002,
    ENTERING_LOBBY                   = 1003,
    ENTERED_LOBBY                    = 1004,
    ENTERING_CHARACTER_CREATION      = 1005,
    LEFT_LOBBY                       = 1006,
    DELETED_CHARACTER                = 1007,
    CANCELED_CHARACTER_CREATION      = 1008,
    ENTERED_CHARACTER_CREATION       = 1009,
    CREATED_CHARACTER                = 1010,
    ENTERED_WORLD                    = 1011,
    FINISHED_LOADING_SCREEN          = 1012,
    LEFT_WORLD                       = 1013,
    MOUNTED_PEGASUS                  = 1014,
    DISMOUNTED_PEGASUS               = 1015,
    CHANGED_CHANNEL                  = 1016,
};
```

These are pure notifications — the launcher does not respond.

---

## 6. Exit reason codes (`0x3fc.reason`)

Partial list of known values:

| Value    | Meaning                                |
| -------: | -------------------------------------- |
| `0x0`    | Success                                |
| `0x6`    | Invalid DataCenter                     |
| `0x8`    | Connection dropped                     |
| `0x9`    | Invalid authentication info            |
| `0xa`    | Out of memory                          |
| `0xc`    | Shader Model 3 unavailable             |
| `0x10`   | Speed hack detected                    |
| `0x13`   | Unsupported version                    |
| `0x106`  | Already online                         |

---

## 7. Rust launcher implementation map

Key files:

| File                                                      | Purpose                                                                    |
| --------------------------------------------------------- | -------------------------------------------------------------------------- |
| `teralib/src/serverlist.proto`                            | proto2 schema (must stay proto2 with required fields).                     |
| `teralib/src/_serverlist_proto.rs`                        | Hand‑maintained prost output. Re‑generated from `serverlist.proto`.        |
| `teralib/src/game/mod.rs::wnd_proc`                       | Single dispatch point for incoming `WM_COPYDATA` messages.                 |
| `teralib/src/game/mod.rs::handle_*`                       | Per‑event handlers (`0x1`, `0x3`, `0x5`, `0x7`, `0x3e8`, …).               |
| `teralib/src/game/mod.rs::prefetch_server_list`           | Downloads `SERVER_LIST_URL`, parses XML or JSON, serialises to protobuf.   |
| `teralib/src/game/mod.rs::parse_server_list_xml`          | Retail XML → `ServerList`.                                                 |
| `teralib/src/game/mod.rs::parse_server_list_json`         | tera‑api JSON → `ServerList`.                                              |
| `teralib/src/config/config.json::SERVER_LIST_URL`         | The HTTP endpoint that returns either flavor.                              |

Constraints worth knowing:

1. **Do not spawn a fresh tokio `Runtime` inside `wnd_proc`.** It runs on the
   Win32 message‑pump thread; `Runtime::new()` inside a `SendMessage` callback
   panics with `Cannot start a runtime from within a runtime`. The launcher
   pre‑fetches and caches the protobuf bytes in `SERVER_LIST_CACHE` before
   spawning `TERA.exe` precisely to keep `handle_server_list_request`
   synchronous.

2. **Don't try to bootstrap the conversation from the launcher side.** TERA is
   always the initiator. There is no `event 24 "Tera"` handshake.

3. **Use the `recipient` HWND from `wParam`, not a cached value.** Different
   incoming events may originate from different TERA windows during startup.
   Always reply to the sender of the request you're handling.

4. **Keep `Cargo.toml` ↔ `_serverlist_proto.rs` in sync.** If you edit the
   `.proto` file, regenerate the Rust output (or update the `_serverlist_proto.rs`
   by hand and verify it still compiles).

---

## 8. References

- [Novadrop — Launcher/Client Protocol][novadrop]
- [tera‑launcher][tera-launcher] — BHS‑based launcher distribution by justkeepquiet
- [tera‑api][tera-api] — Node.js server API that serves `/tera/ServerList`
- `Binaries/ReleaseRevision.txt` (local) — source of `source_revision`
- `fake-tera/` (parent repo) — IPC spy used to verify all of the above

[novadrop]: https://docs.vezel.dev/novadrop/game/launcher-client-protocol
[tera-launcher]: https://github.com/justkeepquiet/tera-launcher
[tera-api]: https://github.com/justkeepquiet/tera-api
