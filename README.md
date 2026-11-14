# rayl

**An MCP server for Apple Mail, written in Rust.** It reads the mail already sitting on your
Mac — no API, no OAuth, no rate limits, nothing leaving the machine.

```
you: which Cloudflare invoice came in this month?

  search_mail(text: "invoice", from: "cloudflare", days: 30)
  read_message(id: 72)

claude: IN-86687607, due 1 November 2026, $0.00.
```

## Why this exists

I was using an AI assistant with a Gmail connector. It worked — until the assistant made far
more API calls than I realised, Google flagged the account for abnormal automated activity,
and I lost access to my own email. I filed an appeal and got it back, which was a slow and
unpleasant way to learn that **an agent's enthusiasm is billed to your account's reputation.**

The thing is, I didn't need the Gmail API at all. Apple Mail had already downloaded that mail
to my laptop. It was sitting in a SQLite index and a pile of files, costing nothing, asking
no one's permission, with no quota to exceed.

That is what `rayl` reads. An agent can search and read as hard as it likes: there is no
service to anger, because there is no service. The mail is already here.

Most Mac users have Mail.app configured and then never open it. That idle local copy turns
out to be the most quota-proof mail API available.

## What you get

Four read-only tools over MCP:

| tool | what it does |
|---|---|
| `search_mail` | find mail by subject, sender, mailbox, flags or date |
| `read_message` | read one message's headers and body |
| `list_mailboxes` | the mailboxes and labels you can filter on |
| `mail_status` | what's readable, what isn't, and why |

**Read-only by construction.** Nothing here can send, delete, move or modify mail. There is
no code path that writes.

## Quick start

```bash
git clone https://github.com/oti-adjei/rayl
cd rayl
cargo build --release

./target/release/rayl doctor
```

`doctor` tells you whether this machine can serve mail, and exactly what to fix if not. It
must report `direct read path available` before the next step is worth doing.

Then wire it to a client:

```bash
# Claude Code
claude mcp add rayl --scope user -- "$PWD/target/release/rayl" serve
```

<details>
<summary>Claude Desktop</summary>

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

</details>

Full setup, every tool parameter, and troubleshooting: **[docs/mcp.md](docs/mcp.md)**.

## The one real catch: Full Disk Access

`~/Library/Mail` is protected by macOS privacy controls, and macOS offers **no narrow "read my
mail" permission** — the directory sits in the catch-all protected set alongside Safari
history and Messages. So Full Disk Access is the only key, and it opens everything, not just
mail.

That is disproportionate for an email reader, and it is Apple's design rather than a choice
made here. Two things worth knowing:

- **The permission belongs to whatever launches `rayl`** — your terminal, or Claude Desktop —
  not to `rayl`, which cannot request it. `doctor` names the exact application and tells you
  to quit it fully afterwards, because closing a window isn't enough.
- **Terminal multiplexers detach.** zellij, tmux and screen reparent to `launchd`, so your
  terminal's grant never reaches them. `doctor` detects this and names the executable to add
  instead, rather than giving you advice that silently won't work.

A planned mode drives Mail.app over Apple Events instead, needing only Automation access
scoped to Mail — the right-sized permission. See [Modes](#modes).

## What it honestly can't do yet

Stated plainly, because a mail tool that overstates its reach makes an agent confidently tell
you an email doesn't exist:

- **Bodies are not searched.** `search_mail` matches subjects and senders. Full-text search
  over message bodies needs the `indexed` mode, which isn't built.
- **Not every body is on your Mac.** IMAP accounts set not to keep full copies store headers
  only. On my own store that's 370 of 989 messages — 37%, not an edge case. `read_message`
  says so explicitly instead of returning an empty body.
- **No sending, replying or drafting.** Planned, via Mail.app.
- **macOS only**, and verified against Apple Mail V10. `rayl doctor` and `rayl schema` will
  tell you if your store differs.

Every tool response repeats the relevant limit. An agent told only "no results" will say the
mail doesn't exist; one told "no results, and bodies weren't searched" can say something true.

## Nice touches

**Tracking URLs are stripped by default.** Marketing mail spends more bytes on tracking than
on content — one LinkedIn link measured ~700 bytes, of which ~60 carried meaning. Left alone,
an agent burns its context window on tracking tokens and truncation discards real content to
make room.

So `read_message` removes known trackers (~60 parameters across LinkedIn, Mailchimp, HubSpot,
Marketo, the ad networks, UTM) plus any unrecognised parameter over 200 bytes. It's a **deny
list, not an allow list**, deliberately: query strings often carry meaning — Cloudflare puts
the invoice destination in `?to=` — and dropping unrecognised parameters would break working
links while "cleaning" them. Links stay navigable, the response says what it removed, and
`raw_urls` turns it off.

**Gmail labels are handled properly.** On Gmail nearly every message's primary mailbox is
*All Mail*, with the real filing in a separate labels table. Filtering the obvious column
returns nothing at all — so `rayl` matches labels too, and reports which mailboxes a message
is filed under rather than duplicating it once per label.

**Nothing is cached.** The default mode reads Mail's own files per request and persists
nothing, so there's no stale copy of your mail anywhere and no index to leak.

## Modes

Access to mail is two independent choices: where data comes from, and how a search is
answered. `rayl doctor` reports which combinations your machine can serve.

| mode | permission | persists | search |
|---|---|---|---|
| **`direct+live`** (current) | Full Disk Access | nothing | metadata, fast |
| `direct+indexed` | Full Disk Access | metadata + index | full-text after import |
| `script+indexed` | Automation (Mail.app) | metadata + index | full-text after import |
| `script+live` | Automation (Mail.app) | nothing | slow |

No mode is narrow in permission, fast, *and* stores nothing. That's a real constraint rather
than a gap in the implementation — the `script` modes trade speed for a much smaller
permission, and `indexed` trades on-disk state for body search.

Note what indexing would cost: an index lives in `~/Library/Application Support/rayl` at mode
`0700`, which is **outside** macOS's protected set. Avoiding the scary permission means moving
mail-derived data somewhere less guarded. Body text would be indexed without being stored,
but an inverted index still leaks more than nothing.

## How it works

Apple Mail keeps a SQLite envelope index plus one `.emlx` file per message. `rayl` reads
both directly, which turns search into a local query instead of hundreds of milliseconds of
Apple Events IPC per message. That's the whole performance argument, and it's why the project
is in Rust rather than a scripting language wrapping `osascript`.

```
crates/rayl-core   mail store access: discovery, schema probing, search, parsing
crates/rayl        the rayl binary: CLI and MCP server
```

`rayl-core` knows nothing about MCP, so the protocol and transport stay swappable — a future
HTTP transport is a new binary target, not a rewrite.

### Verified, not assumed

The envelope index is undocumented and changes between macOS releases. Three assumptions that
looked perfectly reasonable turned out wrong when checked against a real mailbox, each of
which would have shipped as a quiet wrong answer rather than an error:

- `download_state` does not indicate body availability — every row is `0`.
- `message_body_indexed` means "donated to Spotlight", not "body present": 988 of 989 set,
  against 619 bodies actually on disk.
- Filtering on `messages.mailbox` silently returns nothing on Gmail.

So the date epoch and the side-table foreign keys are **resolved at runtime** by counting real
matches rather than hardcoded. A future macOS release that moves them degrades or reports,
instead of answering confidently and wrongly.

The epoch detection is the clearest example: Unix time and Core Data's 2001 epoch are 31
years apart, so the wrong guess yields dates that look entirely plausible. `rayl` decides from
your newest message and flags the one case that's unresolvable in principle.

## Commands

| command | does |
|---|---|
| `rayl serve` | MCP server over stdio |
| `rayl doctor` | which modes this machine can serve, and how to fix the others |
| `rayl search` | metadata search from the terminal |
| `rayl read <id>` | one message body; `--coverage` for how many bodies exist |
| `rayl schema` | the envelope index's shape — tables, columns, row counts |
| `rayl verify` | resolves what the schema can't say: date epoch, foreign keys, enum values |

`schema` and `verify` read **no message content** — only table shapes and counts — so their
output is safe to paste into a bug report. If your macOS version has a different schema,
those two are what I'll ask for.

## Status

`direct+live` works end to end. Early but genuinely usable.

Roadmap:

1. ~~`direct+live`: search, read, MCP server~~ — done
2. `indexed` strategy — full-text search over bodies
3. `script` source — the modes needing no Full Disk Access
4. Drafting via Mail.app
5. HTTP transport, for reaching the Mac from elsewhere

## Development

```bash
cargo test
cargo clippy --all-targets
```

Bug reports are much more useful with `rayl doctor` and `rayl verify` output attached.

## Licence

MIT. See [LICENSE](LICENSE).
