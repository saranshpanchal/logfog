# LogFog

Cut through the log fog. **One index, two faces:** an agent-first CLI and MCP server, and a browser log explorer that binds only to `127.0.0.1`.

LogFog is a fast, zero-setup, **read-only** viewer for logs you already have. It does not ship agents to remote hosts and it does not mutate the files it reads. Point it at local files, a pipe, an SSH tail, a Docker container, or a journald unit. Both faces share one in-memory index.

```text
files / stdin / ssh / docker / journald
                 │
                 ▼
        auto-detecting parser
        (JSON, logfmt, access, text, stacks)
                 │
                 ▼
     sparse block index  +  Drain templates
                 │
        ┌────────┴────────┐
        ▼                 ▼
  127.0.0.1:7474      logfog mcp / CLI
  browser explorer    summary, patterns, query
```

## Build

```bash
cargo build --release
# binary: target/release/logfog
```

Rust 1.85 or newer. The crate is `logfog` and the binary is `logfog`.

## Usage

```bash
# Browser explorer on http://127.0.0.1:7474
logfog ui tests/fixtures/demo.log
logfog /var/log/*.log ./logs/**/*.jsonl          # same thing; `ui` is the default

# Pipe a dev server straight in. The UI opens immediately and tails stdin.
pnpm dev 2>&1 | logfog

# Agent-sized answers
logfog summary app.log --json
logfog patterns app.log --sort spike
logfog query 'level:error status:>=500 -service:healthcheck "timeout"' app.log
logfog trace abc app.log

# Terminal view (j/k, /, t, q)
logfog tui app.log

# MCP server on stdio
logfog mcp /var/log/app/app.log
```

### More sources

```bash
# Nothing is installed on the remote host. The local ssh binary runs sh and tail.
logfog --ssh user@host:/var/log/app/*.log

# Local docker CLI. Defaults to the last 5000 lines (`--remote-lines`).
logfog --docker api

# Local journalctl, JSON export, same line cap.
logfog --journald app.service

# Follow new bytes (files, and with --follow also ssh/docker/journald).
logfog ui --follow /var/log/app/app.log

# Transparent gzip and zstd. Offsets refer to the decompressed bytes.
logfog summary app.log.gz
```

The explorer listens on `127.0.0.1` only. Change the port with `--port`. Bookmarks and saved views go to `logfog.sqlite` in the working directory (`--db` to move it).

## Query language

```text
level:error service:api status:>=500 -service:healthcheck "timeout" /disk full/ since:1h
```

| Form | Meaning |
| --- | --- |
| `field:value` | Facet equality. `level` compares the parsed severity. |
| `field:>=N` `>` `<` `<=` | Numeric compare. `status:500..599` is an inclusive range. |
| `-field:value` | Exclude rows whose facet matches. |
| `"substring"` | Case-insensitive substring over the raw event (ANSI stripped). |
| `/regex/i` | Regex. Add `i` for case-insensitive. |
| `since:1h` `from:` | Lower time bound. A duration is relative to the **newest event**, so historical files still match. An absolute RFC3339 timestamp is used as-is. |
| `until:` `to:` | Upper time bound. |
| `template:12` | Events assigned to Drain template id 12. |

## Browser explorer

`http://127.0.0.1:7474`

- Stacked INFO / WARN / ERROR histogram. Drag across it to brush a time window. The chart itself stays on the text query so the brush does not collapse the range.
- Facet sidebar with discovered fields and top counts. `+` includes a value, `–` excludes it.
- Virtualized table. The server pages with `offset` and `limit`.
- Row drawer: facets, folded stack, **Show context (±20 lines)**, and **Correlate** on `trace_id` / `request_id` / `correlation_id`.
- Patterns tab: Drain templates, severity mix, sparkline, spike note. Click a row to filter `template:<id>`.
- Live tail via SSE `GET /api/stream` (pause by toggling **Live**). Bookmarks are stored in SQLite.

HTTP:

| Method | Path |
| --- | --- |
| GET | `/api/summary` `/api/query` `/api/patterns` `/api/facets` `/api/histogram` `/api/context` `/api/trace` `/api/stream` |
| GET/POST | `/api/bookmarks` `/api/views` |

Context accepts `id`, or `file_id` plus `offset` or `line`, and `n` (default 20).

## CLI

| Command | What it prints |
| --- | --- |
| `summary` | Totals, span, severity, top error templates, anomaly lines. `--json` for the struct. |
| `patterns` | Templates. `--sort count\|spike`, `--level`, `--service`. |
| `query <EXPR>` | Matching lines. `--json` includes the page metadata. |
| `trace <ID>` | Chronological events for that trace, request, span, or correlation id. |
| `ui` | Loopback explorer. This is the default when the first argument is a path. |
| `tui` | Full-screen list. |
| `mcp` | JSON-RPC MCP server. Logs go to stderr; stdout is the protocol. |

Every command accepts `--json` where it prints data. `summary` and `patterns` also honor `-q`.

## MCP

Tools, all read-only:

| Tool | Budget |
| --- | --- |
| `logfog_summary` | Under 500 tokens: totals, span, severity, top 10 error templates, anomaly windows. |
| `logfog_patterns` | Templates filtered by `level`, `service`, or `q`. Sort `count` or `spike`. |
| `logfog_query` | Facet, regex, and time queries. Deduped, hard line cap and `token_budget` (default 2000). |
| `logfog_trace` | Chronological cross-service lifecycle for one id. |
| `logfog_context` | ±N lines and the folded stack for an event id, or `file_id` plus `line` or `offset`. |

The server speaks JSON-RPC 2.0 on stdio, with `Content-Length` framing and newline-delimited JSON.

### Cursor

`.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "logfog": {
      "command": "logfog",
      "args": ["mcp", "./logs/app.jsonl"]
    }
  }
}
```

### Claude Code

```json
{
  "mcpServers": {
    "logfog": {
      "command": "logfog",
      "args": ["mcp", "/var/log/app/app.log"]
    }
  }
}
```

Pass every file, glob, `--ssh`, `--docker`, or `--journald` source as arguments to `logfog mcp`. The tools query that dataset; they do not open arbitrary paths later.

## What gets parsed

Per line, after a Docker Compose prefix (`api_1 | …`) is peeled off:

- **JSON / NDJSON** — pino, bunyan, zap, slog, winston, structlog. `level` may be a name or a pino number (`10..60` → TRACE..FATAL). `time` / `timestamp` / `ts` / `@timestamp` may be RFC3339 or unix s/ms/µs/ns. `msg` / `message` is the message. `err` / `error` / `stack` are kept. Correlation ids (`req_id`, `request_id`, `trace_id`, `span_id`, `correlation_id`, and the camelCase forms) are indexed for `trace`.
- **journald JSON** — `MESSAGE`, `PRIORITY`, `__REALTIME_TIMESTAMP`, `SYSLOG_IDENTIFIER`, `_SYSTEMD_UNIT`, `_HOSTNAME`.
- **logfmt** — the whole line must be `key=value` pairs, quotes included. Numbers stay numbers, which is why `status:>=500` works. The scanner is hand-written: the `logfmt` crate returns strings only, and a second parse would throw away the type.
- **Plain text** — leading timestamps, a level word, and inline `key=value` tokens. ANSI is stripped for indexing and kept in the raw bytes for display.
- **Nginx/Apache access logs** — client, method, path, status. `5xx` is error, `4xx` is warn.
- **Stacks** — Python tracebacks, Java/Node `at` frames, Go panics, Rust backtraces. Continuation lines fold into the parent event instead of becoming their own rows.

Every event is tagged with `source`, `host`, and `service` when those are not already in the line.

## Architecture

The index does not copy payloads. Local files are `mmap`ed (read-only). Stdin, SSH, Docker, journald, and compressed files live in a buffer. Each event is a **32-byte** packed record inside a block of up to 128 events:

```text
ts delta (i64) | offset delta (u32) | length (u32) | line delta (u32)
template id (u32) | span (u16) | level (u8)
```

`summary` reports `index_bytes` and `payload_bytes`. Thirty-two bytes is under 2% when the average event is longer than 1600 bytes, and higher for short lines (a 200-byte line is about 16% for the record alone). Facet values and the per-file id list sit beside that. Raw bytes are never duplicated into the index.

Drain is an in-process fixed-depth prefix tree (depth 4, counting the length root). Before clustering, UUIDs, IPs, durations, paths, hex, and numbers are masked to `<*>`. Templates track count, severity, first/last seen, and per-minute buckets. A spike is an error bucket of at least 5 that is more than 3× the previous baseline, or a brand-new all-error template at the end of a long span.

## Tests

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Fixtures live in `tests/fixtures/`. Coverage includes mixed-format ingest (pino levels, logfmt, text, Python/Rust/Node/Go stacks, ANSI), byte-offset round trips, facet queries, Drain templates, the HTTP API, and MCP JSON-RPC tool calls. GitHub Actions runs the same three commands.

## Limitations

- The 32-byte record is the sparse index. It is not always under 2% of the file; short lines cost more. Facets are extra, on purpose, so `status:>=500` does not re-parse the file.
- `.gz` and `.zst` are decompressed fully into memory. Offsets are into that buffer, not the compressed file.
- SSH, Docker, and journald require the local `ssh`, `docker`, and `journalctl` binaries. Remote globs may not contain shell metacharacters other than `* ? [`. Docker and journald batch reads default to 5000 lines.
- Follow mode polls and also watches the parent directory with `notify`. A rotated file is ingested as a new buffer; the previous bytes stay searchable. An unfinished stack is held for about 400ms so the next frame can join it.
- The query evaluator scans events. It is fine for a few million in-process rows and is not an inverted index.
- Pretty-printed multi-line JSON is not reassembled. One JSON object per line.
- The TUI is a keyboard list and a pattern view, not the full facet sidebar.
- The browser has no authentication because it binds to loopback only. Do not put it on a public interface; the binary will not.
- Drain state is in-process and forgotten when the process exits. Spike notes need a real time range; a one-second fixture usually has none.
- `since:1h` is relative to the newest timestamp in the loaded data, not the wall clock.

## License

MIT. See [LICENSE](LICENSE).
