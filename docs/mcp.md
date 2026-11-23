# rayl as an MCP server

`rayl serve` speaks the Model Context Protocol over stdio. It reads mail and creates drafts.
**It cannot send**: there is no send path in the codebase, and nothing can delete or modify
existing mail.

## Install

```bash
cd 03-code/rayl
cargo build --release
./target/release/rayl doctor     # confirm access before wiring anything up
```

`doctor` must report `Verdict: direct read path available` before the server can return mail.
If it does not, fix that first — see [Permissions](#permissions).

## Configure a client

### Claude Code

```bash
claude mcp add rayl -- /absolute/path/to/rayl/target/release/rayl serve
```

### Claude Desktop

In `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "rayl": {
      "command": "/absolute/path/to/rayl/target/release/rayl",
      "args": ["serve"]
    }
  }
}
```

Use an absolute path: the client does not inherit your shell's `PATH`.

> **The permission follows the client, not rayl.** macOS attributes Full Disk Access to the
> application that launches the process. Granting it to your terminal does not help Claude
> Desktop — that app needs its own grant. `mail_status` names whichever application needs it.

## Other clients — Cursor, VS Code, Windsurf, Zed, Codex, Gemini CLI, Goose, Cline…

rayl speaks the standard protocol, so any MCP-compatible client works. Each client names
the server in its own config format: **[exact snippets for every client → docs/clients.md](clients.md)**.

## Tools

### `search_mail`

| parameter | type | meaning |
|---|---|---|
| `text` | string | matched against the subject, including any `Re:`/`Fwd:` prefix |
| `from` | string | matched against the sender's address **or** display name |
| `to` | string | matched against any recipient, To or Cc |
| `body` | string | matched against message body text — see below |
| `mailbox` | string | mailbox or label name; see `list_mailboxes` |
| `unread_only` | bool | |
| `flagged_only` | bool | |
| `has_attachment` | bool | |
| `days` | integer | only messages from the last N days |
| `limit` | integer | default 20, capped at 100 |

All parameters are optional; with none, it returns the most recent messages. Substring
matching, case-insensitive for ASCII. `%` and `_` are literal, not wildcards.

Each hit carries an `id` for `read_message`, plus sender, subject, date, flags, attachment
filenames, thread size, and the mailboxes the message is filed under.

#### Body search

Passing `body` searches message text, which matters because the useful detail often isn't in
the subject — an application acknowledgement titled "Your application" may carry the deadline,
the portal link or the rejection reason only in its body.

There is no index behind this. Candidates are narrowed by the other filters, then their local
message files are opened and scanned, which keeps the "nothing persisted" guarantee intact.
Two consequences worth knowing:

- **It is linear**, so combine it with `from`, `mailbox` or `days` on a large mailbox. A scan
  budget bounds it, and the response says when that budget was hit.
- **It covers only downloaded bodies.** Messages Mail holds as headers-only are reported as
  *unsearchable* rather than as non-matching, with a count. A match in those cannot be ruled
  out, and the response says so.

Without `body`, the response states that bodies were not searched — because an agent that
doesn't know what it couldn't see will report that an email does not exist.

### `read_message`

| parameter | type | meaning |
|---|---|---|
| `id` | integer | **required**; from `search_mail` |
| `max_bytes` | integer | body cap, default 4096, max 262144 |
| `raw_urls` | bool | keep tracking parameters in URLs (default: strip them) |

Returns headers and body. Plain text is preferred; HTML is converted only when no plain part
exists, and the response says when it did. Truncation is reported with the full size, so an
agent can ask again for more.

A message whose body Mail has not downloaded returns an explicit statement to that effect —
not an empty body. The distinction matters: one means "nothing was written", the other means
"it exists elsewhere".

### `list_mailboxes`

No parameters. Returns mailboxes and labels with message and unread counts. Call it before
guessing a mailbox name.

### `save_attachment`

Lists a message's attachments, or writes them to files.

| parameter | type | meaning |
|---|---|---|
| `id` | integer | **required**; from `search_mail` |
| `which` | string | a zero-based position, or part of a filename; all if omitted |
| `directory` | string | where to write; defaults to `~/Downloads` |
| `list_only` | bool | report names, types and sizes without writing |

Attachment filenames come from the message, so they are attacker-controlled: each is reduced
to a safe single path component before use, and a name like `../../.ssh/authorized_keys`
becomes `authorized_keys`. An existing file is never overwritten — `report.pdf` becomes
`report-2.pdf`.

Only works on messages whose bodies are downloaded locally; a headers-only message has no
attachment data on the machine.

### `draft_message`

Saves a **new draft** in Mail's Drafts folder. Never sends.

| parameter | type | meaning |
|---|---|---|
| `subject` | string | **required** |
| `body` | string | **required**; plain text |
| `to` | string[] | recipient addresses |
| `cc` | string[] | cc addresses |

At least one recipient is required. The draft appears in Mail for the user to review and
send themselves.

### `draft_reply`

Saves a **reply draft** to an existing message. Never sends.

| parameter | type | meaning |
|---|---|---|
| `id` | integer | **required**; from `search_mail` |
| `body` | string | **required**; goes above the quoted original |
| `reply_all` | bool | reply to everyone rather than only the sender |

Mail itself constructs the reply, so threading headers (`In-Reply-To`, `References`) and the
quoted original are Mail's work rather than ours — AppleScript cannot set those headers on a
message we build, so letting Mail do it is what makes the reply thread correctly in other
clients.

A small number of messages have no RFC Message-ID recorded; Mail cannot locate those, and the
tool says so and suggests `draft_message` instead.

### `mail_status`

No parameters. Reports which accounts are loaded in Mail, the active mode, how many messages
are indexed, and how many have bodies available locally. The account list matters: mail for an
address that is not listed is not on this machine at all, so an empty result for it means
"not here" rather than "does not exist". Call it when a search returns surprisingly little, to tell
"no such message" apart from "that part of the mailbox is not readable". When access is
blocked it returns the specific remedy, including which application needs permission.

## What it does not do

- **Body search has no index.** It opens message files per query, which is fine for a few
  thousand messages and bounded by a scan budget beyond that. Nothing is persisted.
- **Body search cannot see undownloaded bodies**, and reports how many it skipped.
- **Cannot send mail.** Drafting is supported; sending is not, and is not planned. There is no
  SMTP client and no `send` command anywhere in the codebase.
- **Does not delete, flag, move or modify existing mail.**
- **Does not reach the network.** Everything comes from files already on the Mac.
- **Does not cache.** `direct+live` reads Mail's own files per call and persists nothing. The
  envelope index is snapshotted to a temporary file per request and deleted immediately, so
  the server never holds Mail's files open or answers from a stale copy.

## Coverage, and why it is stated rather than implied

Two separate limits apply, and both are reported rather than hidden:

1. **Bodies are not searched.** `search_mail` sees subjects and senders.
2. **Not every body is on the Mac.** IMAP accounts configured not to keep full copies store a
   `.partial.emlx` with headers only. On the store this was developed against, 370 of 989
   messages were headers-only — 37%, not an edge case.

An agent told only "no results" will say the mail does not exist. An agent told "no results,
and bodies were not searched, and 37% of bodies are not local" can say something true.

## URL compaction

Marketing mail often spends more bytes on tracking than on content: one LinkedIn link
measured ~700 bytes, of which ~60 carried meaning. Left alone, an agent spends its context on
tracking tokens and `max_bytes` truncates real content to make room.

So `read_message` strips known tracking parameters by default — around 60 of them across
LinkedIn, Mailchimp, HubSpot, Marketo, the ad networks and UTM — plus any unrecognised
parameter whose value exceeds 200 bytes, since an opaque blob that long is a tracker whatever
it is named.

It is a **deny list, not an allow list**, deliberately. Query strings often carry meaning:
Cloudflare's invoice links put the destination in `?to=`, and dropping unrecognised
parameters would have broken a working link while "cleaning" it. The 200-byte threshold is
generous so that unsubscribe tokens and signed URLs survive.

Links stay navigable. The response says how many URLs were changed and how many bytes that
saved; `raw_urls` turns it off.

## Permissions

Two separate grants are involved, and they are unrelated to each other.

### Automation, for drafting

`draft_message` and `draft_reply` drive Mail.app through Apple Events, which needs Automation
access scoped to Mail — System Settings > Privacy & Security > Automation. macOS prompts once,
on first use, naming whichever application is running the server. Mail.app must also be
running, since Apple Events reach a running application only.

This is the narrow, right-sized permission: it grants control of Mail and nothing else. Both
draft tools report these conditions specifically rather than failing generically, so an agent
knows to ask rather than retry.

### Full Disk Access, for reading

`~/Library/Mail` is protected by macOS privacy controls (TCC). macOS has no narrow "read my
mail" permission — the directory sits in the catch-all protected set alongside Safari history
and Messages — so **Full Disk Access is the only key**, and it opens everything, not just
mail.

That is disproportionate, and it is Apple's design rather than a choice here. The planned
`script` mode will need only Automation access scoped to Mail.app, which is the right-sized
permission; it is not built yet.

rayl cannot request the grant. It belongs to the launching application, and that application
must be fully quit and reopened afterwards — closing a window is not enough.

Two wrinkles worth knowing:

- **Terminal multiplexers detach.** zellij, tmux and screen reparent to `launchd`, so the
  terminal that started them is not an ancestor and its grant does not reach you. `doctor`
  detects this and names the executable to add instead.
- **The grant is broad.** Granting it to a terminal gives everything you ever run there access
  to your whole disk. Granting it to Claude Desktop is narrower in practice, since only that
  app is covered.

## Modes

`serve` currently implements `direct+live` only.

| mode | permission | persists | search |
|---|---|---|---|
| `direct+live` (current) | Full Disk Access | nothing | metadata, fast |
| `direct+indexed` | Full Disk Access | metadata + index | full-text after import |
| `script+indexed` | Automation (Mail.app) | metadata + index | full-text after import |
| `script+live` | Automation (Mail.app) | nothing | slow |

No mode is narrow, fast and stores nothing — that is a real constraint, not a gap in the
implementation. `rayl doctor` reports which modes a given machine can serve.

## Troubleshooting

**The client says the server failed to start.** It should not: `serve` starts even when mail
is unreadable, so the reason arrives through `mail_status` instead of as a dead process. A
genuine start failure is a bad binary path in the client config.

**Every tool returns a permission message.** The launching application lacks Full Disk Access.
`mail_status` names it. Quit that application completely and reopen it after granting.

**Searches find nothing that should exist.** Likely the body-vs-metadata limit: the term is in
the body, which is not searched. Try the sender or a subject word instead.

**`read_message` says the body is not available.** Mail has only the headers. Change that
account's settings in Mail to keep full copies, then let it sync.

**A draft tool says it cannot control Mail.** That is the Automation permission, not Full Disk
Access — a different pane in System Settings. Grant it to the application running the server.

**A draft tool says Mail is not running.** Open Mail. Reading does not need it; drafting does.

**Dates look wrong by about 31 years.** Report it. The date epoch is detected at runtime
rather than assumed, and a response that cannot confirm it says so — but a mailbox whose
newest message genuinely predates 1996 is unresolvable in principle.
