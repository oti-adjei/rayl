# rayl

An MCP server for Apple Mail, written in Rust.

Cloud mail connectors burn API quota and can get an account rate-limited or banned. Apple
Mail already holds a full local copy of the same mail on any Mac that has it configured, and
reading that copy costs nothing and asks no provider for permission.

## Modes

macOS offers no narrow "read my mail" permission. Reading Mail's files requires Full Disk
Access — the same grant that opens your SSH keys — because `~/Library/Mail` sits in the
catch-all protected set. Driving Mail.app over Apple Events instead needs only Automation
access scoped to Mail, which is the right-sized permission but costs hundreds of milliseconds
per message.

Neither is correct for everyone, so `rayl` treats it as two independent choices.

**Source** — where message data comes from:

- `direct` — Mail's SQLite envelope index and `.emlx` bodies. Fast. Needs Full Disk Access.
- `script` — Mail.app over Apple Events. Slow per message. Needs Automation access only.

**Strategy** — how a search is answered:

- `live` — query the source every time. Nothing persists.
- `indexed` — maintain our own index, so Apple Events are paid once per message instead of
  once per search. Fast for either source.

|   | permission | persists | search |
|---|---|---|---|
| `direct+live` | Full Disk Access | nothing | fast |
| `direct+indexed` | Full Disk Access | metadata + index | fast after import |
| `script+indexed` | Automation (Mail.app) | metadata + index | fast after import |
| `script+live` | Automation (Mail.app) | nothing | slow |

No cell is narrow, fast and stores nothing — that is the actual constraint, not a gap in the
implementation.

### What indexing stores, and where

An index lives in `~/Library/Application Support/rayl` at mode `0700`. Body text is indexed
without being stored, so searches work while the bodies themselves are not written down; a
hit's body is fetched from the source on demand.

Be clear-eyed about the tradeoff: that directory is *outside* macOS's protected set, so
ordinary file permissions are all that guard it, and an inverted index with positions leaks a
meaningful amount of the text it covers. It is much less than a plaintext archive, not
nothing. Caching whole bodies is worse still and largely redundant, since Mail already holds
a complete local copy — it buys only the ability to work with Mail.app closed.

Index scope (mailboxes, date window, excluding attachments) affects size far more than the
mode does. Attachments dominate real mail stores.

## Layout

```
crates/rayl-core   mail store access: version detection, schema probing, reading
crates/rayl        the `rayl` binary: CLI and (later) the MCP server
```

`rayl-core` knows nothing about MCP, so the protocol layer and the transport stay swappable.

## Status

`direct+live` works end to end: discovery, diagnosis, metadata search, body reading, and an
MCP server over stdio. Read-only; nothing can send, delete or modify mail.

See [docs/mcp.md](docs/mcp.md) for the MCP server: client configuration, the four tools, and
what it deliberately does not do.

Roadmap:

1. ~~`direct+live` search and read over stdio.~~ Done.
2. The `indexed` strategy — full-text search over bodies, shared by both sources.
3. The `script` source, which opens the modes needing no Full Disk Access.
4. Drafting via Mail.app, then an HTTP transport for reaching the Mac from elsewhere.

The source abstraction is deliberately not written yet: the envelope index schema is
undocumented, so it gets designed against a real one rather than a guessed one.

## Commands

| command | does |
|---|---|
| `rayl serve` | MCP server over stdio — see [docs/mcp.md](docs/mcp.md) |
| `rayl doctor` | which modes this machine can serve, and how to fix the others |
| `rayl search` | metadata search from the terminal |
| `rayl read <id>` | one message body; `--coverage` for how many bodies exist |
| `rayl schema` | the envelope index's shape — tables, columns, row counts |
| `rayl verify` | resolves what the schema cannot say: date epoch, foreign keys, enum values |

`schema` and `verify` exist because the index is undocumented and changes between macOS
releases. They read no message content, so their output is safe to paste into a bug report.

## Verified, not assumed

Three assumptions that looked reasonable turned out to be wrong when checked against a real
store, each of which would have shipped as a quiet wrong answer rather than an error:

- `download_state` does not indicate body availability — every row is `0`. Body presence has
  to come from the filesystem.
- `message_body_indexed` means "donated to Spotlight", not "body present": 988 of 989 set,
  against 619 bodies actually on disk.
- Filtering on `messages.mailbox` silently returns nothing on Gmail, where nearly every
  message's primary mailbox is All Mail and the real filing lives in `labels`.

So the date epoch and the side-table foreign keys are resolved at runtime by counting real
matches rather than hardcoded. A future macOS release that moves them degrades or reports,
instead of answering confidently and wrongly. `02-Design/envelope-index-v10.md` records what
was observed.

## rayl doctor

Reports which modes this machine can serve and why the others are closed. Exits non-zero
unless the direct path is available.

```
cargo run -p rayl -- doctor
```

### Full Disk Access

`~/Library/Mail` is protected by macOS privacy controls (TCC). The grant belongs to the
process that launches `rayl` — a terminal, an editor, Claude Desktop — and `rayl` cannot
request it on its own. `doctor` names the responsible app bundle where there is one, and
otherwise prints the process chain and the executable to add by hand.

Note that a terminal multiplexer detaches from the terminal that started it, so the grant may
have to go to the multiplexer rather than the terminal.

## Development

```
cargo test
cargo clippy --all-targets
```
