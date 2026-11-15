//! JSON-RPC 2.0 MCP server over stdio (`logfog mcp`).
//!
//! Tools return counts, templates, and capped lines. `logfog_summary` stays
//! under 500 tokens so an agent can orient itself before pulling raw lines.

use crate::engine::{render_summary, ContextView, Engine, EventPreview, PatternRow, Summary};
use crate::types::estimate_tokens;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2024-11-05";

pub fn serve(engine: &Engine) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout().lock();
    loop {
        let Some(body) = read_frame(&mut reader)? else {
            break;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            let err = rpc_error(Value::Null, -32700, "parse error");
            write_frame(&mut stdout, &err)?;
            continue;
        };
        if let Some(resp) = handle_message(engine, &msg) {
            write_frame(&mut stdout, &resp)?;
        }
    }
    Ok(())
}

pub fn handle_message(engine: &Engine, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    // Notifications have no id.
    let is_notification = msg.get("id").is_none() || method.starts_with("notifications/");
    if method.starts_with("notifications/") {
        return None;
    }
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "logfog", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_defs() })),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            call_tool(engine, &params)
        }
        _ => {
            if is_notification {
                return None;
            }
            Err(anyhow!("method not found: {method}"))
        }
    };
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err(err) => rpc_error(id, -32601, &err.to_string()),
    })
}

fn call_tool(engine: &Engine, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let text = match name {
        "logfog_summary" => {
            let q = args.get("q").and_then(|v| v.as_str()).unwrap_or("");
            engine.summary_text(q, 500)?
        }
        "logfog_patterns" => patterns_text(engine, &args)?,
        "logfog_query" => query_text(engine, &args)?,
        "logfog_trace" => trace_text(engine, &args)?,
        "logfog_context" => context_text(engine, &args)?,
        other => return Err(anyhow!("unknown tool {other}")),
    };
    let tokens = estimate_tokens(&text);
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
        "_meta": { "estimated_tokens": tokens }
    }))
}

fn patterns_text(engine: &Engine, args: &Value) -> Result<String> {
    let level = args.get("level").and_then(|v| v.as_str());
    let service = args.get("service").and_then(|v| v.as_str());
    let mut parts = Vec::new();
    if let Some(level) = level {
        parts.push(format!("level:{level}"));
    }
    if let Some(service) = service {
        parts.push(format!("service:{service}"));
    }
    if let Some(q) = args.get("q").and_then(|v| v.as_str()) {
        parts.push(q.to_string());
    }
    let sort = args.get("sort").and_then(|v| v.as_str()).unwrap_or("count");
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let rows = engine.patterns(&parts.join(" "), sort)?;
    Ok(render_patterns(&rows, limit))
}

fn query_text(engine: &Engine, args: &Value) -> Result<String> {
    let q = args.get("q").and_then(|v| v.as_str()).unwrap_or("");
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(40) as usize;
    let budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(2_000) as usize;
    let page = engine.query_page(crate::engine::PageQuery {
        query: q,
        from_ns: None,
        to_ns: None,
        offset: 0,
        limit: limit.min(200),
        token_budget: Some(budget),
        dedup: true,
    })?;
    let mut lines = vec![format!(
        "total={} returned={} truncated={} tokens={}",
        page.total,
        page.events.len(),
        page.truncated,
        page.estimated_tokens
    )];
    for ev in &page.events {
        lines.push(format_event_line(ev));
    }
    if page.truncated {
        lines
            .push("…truncated to the token budget. Narrow the query or raise token_budget.".into());
    }
    Ok(cap_tokens(&lines.join("\n"), budget))
}

fn trace_text(engine: &Engine, args: &Value) -> Result<String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("logfog_trace requires id"))?;
    let events = engine.trace(id);
    let budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(2_000) as usize;
    let mut lines = vec![format!("trace {id} events={}", events.len())];
    for ev in &events {
        lines.push(format_event_line(ev));
    }
    if events.is_empty() {
        lines.push("no events carried that trace, request, or correlation id".into());
    }
    Ok(cap_tokens(&lines.join("\n"), budget))
}

fn context_text(engine: &Engine, args: &Value) -> Result<String> {
    let id = args.get("id").and_then(|v| v.as_u64()).map(|n| n as u32);
    let file_id = args
        .get("file_id")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let offset = args.get("offset").and_then(|v| v.as_u64());
    let line = args.get("line").and_then(|v| v.as_u64()).map(|n| n as u32);
    let n = args
        .get("lines")
        .or_else(|| args.get("n"))
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as u32;
    let budget = args
        .get("token_budget")
        .and_then(|v| v.as_u64())
        .unwrap_or(2_000) as usize;
    let ctx = engine.context(id, file_id, offset, line, n)?;
    Ok(cap_tokens(&render_context(&ctx), budget))
}

fn format_event_line(ev: &EventPreview) -> String {
    let ts = ev.ts.clone().unwrap_or_else(|| "-".into());
    let svc = if ev.service.is_empty() {
        "-".into()
    } else {
        ev.service.clone()
    };
    format!("{ts} {} {svc} {}", ev.level, ev.message)
}

fn render_patterns(rows: &[PatternRow], limit: usize) -> String {
    let mut lines = vec!["count\terrors\ttemplate".into()];
    for row in rows.iter().take(limit) {
        let errors = row.levels.get("error").copied().unwrap_or(0)
            + row.levels.get("fatal").copied().unwrap_or(0);
        let spike = row.spike.as_deref().unwrap_or("");
        lines.push(format!(
            "{}\t{}\t{}{}",
            row.count,
            errors,
            row.template,
            if spike.is_empty() {
                String::new()
            } else {
                format!("  [{spike}]")
            }
        ));
    }
    if rows.is_empty() {
        lines.push("(no templates)".into());
    }
    lines.join("\n")
}

fn render_context(ctx: &ContextView) -> String {
    let ev = &ctx.event;
    let mut lines = vec![
        format!(
            "file={} line={} offset={} level={} service={}",
            ev.file_id, ev.line, ev.offset, ev.level, ev.service
        ),
        format!("message {}", ev.message),
        "--- before".into(),
    ];
    lines.extend(ctx.before.iter().cloned());
    lines.push("--- event".into());
    lines.push(ctx.event_text.clone());
    lines.push("--- after".into());
    lines.extend(ctx.after.iter().cloned());
    if !ctx.stack.is_empty() {
        lines.push("--- stack".into());
        lines.push(ctx.stack.clone());
    }
    lines.push("--- facets".into());
    for (k, v) in &ev.facets {
        lines.push(format!("{k}={v}"));
    }
    lines.join("\n")
}

fn cap_tokens(text: &str, max_tokens: usize) -> String {
    if estimate_tokens(text) <= max_tokens {
        return text.to_string();
    }
    let mut end = max_tokens.saturating_mul(4).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = text[..end].to_string();
    clipped.push_str("\n…truncated");
    clipped
}

pub fn summary_token_guard(engine: &Engine) -> Result<(Summary, String)> {
    let summary = engine.summary("")?;
    let text = render_summary(&summary, 500);
    Ok((summary, text))
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "logfog_summary",
            "description": "Totals, time span, severity breakdown, top 10 error templates, and anomaly windows. Under 500 tokens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": { "type": "string", "description": "Optional filter using LogFog query syntax" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "logfog_patterns",
            "description": "Drain templates with counts, severity, and spike notes. Filter with level, service, or q.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "level": { "type": "string" },
                    "service": { "type": "string" },
                    "q": { "type": "string" },
                    "sort": { "type": "string", "enum": ["count", "spike"] },
                    "limit": { "type": "integer" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "logfog_query",
            "description": "Facet, regex, and time query. Results are deduplicated and capped by line count and token budget.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": { "type": "string" },
                    "limit": { "type": "integer" },
                    "token_budget": { "type": "integer" }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "logfog_trace",
            "description": "Chronological cross-service lifecycle for a trace_id, request_id, span_id, or correlation_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "token_budget": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        },
        {
            "name": "logfog_context",
            "description": "±N lines around an event plus the full folded stack. Address it by id, or by file_id plus line or offset.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "file_id": { "type": "integer" },
                    "line": { "type": "integer" },
                    "offset": { "type": "integer" },
                    "lines": { "type": "integer" },
                    "token_budget": { "type": "integer" }
                },
                "additionalProperties": false
            }
        }
    ])
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

pub fn encode_frame(value: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = header.into_bytes();
    out.extend(body);
    out
}

pub fn read_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut first = String::new();
    let n = reader.read_line(&mut first)?;
    if n == 0 {
        return Ok(None);
    }
    if first.to_ascii_lowercase().starts_with("content-length:") {
        let len: usize = first
            .split(':')
            .nth(1)
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|_| anyhow!("bad content-length"))?;
        // Consume the blank line and any extra headers.
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            if header == "\r\n" || header == "\n" || header.is_empty() {
                break;
            }
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        return Ok(Some(body));
    }
    let trimmed = first.trim();
    if trimmed.is_empty() {
        return read_frame(reader);
    }
    Ok(Some(trimmed.as_bytes().to_vec()))
}

fn write_frame<W: Write>(out: &mut W, value: &Value) -> Result<()> {
    out.write_all(&encode_frame(value))?;
    out.flush()?;
    Ok(())
}
