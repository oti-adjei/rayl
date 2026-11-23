# rayl with any MCP client

rayl speaks the Model Context Protocol over stdio and nothing else, so it works with any
MCP-compatible client. No client-specific code exists in rayl and none is needed.

Only two things vary between clients: **the config file format** they read, and **which
application macOS holds responsible for permissions** (below).

## The common shape

Most clients want this JSON, with the path pointing at your built binary:

```json
{
  "mcpServers": {
    "rayl": {
      "command": "/absolute/path/to/rayl",
      "args": ["serve"]
    }
  }
}
```

Use an absolute path: clients do not inherit your shell's `PATH`.

## Per client

### Claude Code

```bash
claude mcp add rayl --scope user -- /absolute/path/to/rayl serve
```

### Claude Desktop

`~/Library/Application Support/Claude/claude_desktop_config.json` — the common shape above.

### Cursor

Global: `~/.cursor/mcp.json` · Project: `.cursor/mcp.json` in the repo — the common shape.

### VS Code (GitHub Copilot)

`.vscode/mcp.json` in the workspace, or Command Palette → **MCP: Add Server**. Note VS Code
uses `servers` and a `type`, not `mcpServers`:

```json
{
  "servers": {
    "rayl": {
      "type": "stdio",
      "command": "/absolute/path/to/rayl",
      "args": ["serve"]
    }
  }
}
```

### Windsurf

`~/.codeium/windsurf/mcp_config.json` — the common shape.

### Gemini CLI

`~/.gemini/settings.json` — the common shape under `mcpServers`.

### Codex CLI

`~/.codex/config.toml`:

```toml
[mcp_servers.rayl]
command = "/absolute/path/to/rayl"
args = ["serve"]
```

### Zed

`~/.zed/settings.json`:

```json
{
  "context_servers": {
    "rayl": {
      "source": "custom",
      "command": "/absolute/path/to/rayl",
      "args": ["serve"]
    }
  }
}
```

### Cline

`~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`
— the common shape.

### Goose

Interactive: `goose configure` → Extensions → Add → Standard IO, name `rayl`,
command `/absolute/path/to/rayl`, args `serve`.

### Continue

`~/.continue/mcp.json` — the common shape.

### opencode

`opencode.json` in the project:

```json
{
  "mcp": {
    "rayl": {
      "type": "local",
      "command": ["/absolute/path/to/rayl", "serve"]
    }
  }
}
```

### Anything else

Find where your client declares stdio MCP servers and paste the common shape. Client formats
drift between releases; if a snippet above is stale, the client's own MCP docs are
authoritative — rayl needs nothing client-specific.

## Which app gets Full Disk Access

macOS grants Full Disk Access to **the application that launches rayl**, so wiring a new
client means a grant for *that* client:

| Client | Grant Full Disk Access to |
|---|---|
| Claude Desktop | Claude Desktop |
| Claude Code, Codex, Gemini CLI, Goose | the terminal app you run them in |
| Cursor | Cursor |
| VS Code, Cline | Visual Studio Code |
| Windsurf | Windsurf |
| Zed | Zed |

If you launch the CLI from tmux, zellij or screen, the grant must go to the multiplexer's
executable instead — the terminal's grant does not survive the reparent. `rayl doctor`
detects this case and prints the exact path to add.

After wiring any new client, run `rayl doctor` from inside that client's environment once —
it names the exact application macOS will hold responsible, which removes the guessing.

## When a client can read but not draft

Drafting needs Automation access to Mail.app (System Settings → Privacy & Security →
Automation), prompted on first use, and Mail must be running. The prompt names the client
that launched rayl — that is the app being granted, not rayl.
