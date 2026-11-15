//! Local files, globs, stdin, and agentless readers for SSH, Docker, and journald.
//!
//! Remote sources shell out to the local `ssh`, `docker`, or `journalctl` binary.
//! Nothing is installed on the far side: SSH runs `tail` via the remote `sh`.

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

pub fn expand_input(pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    if pattern.is_empty() {
        bail!("empty path");
    }
    if !contains_meta(pattern) {
        let path = Path::new(pattern);
        if path.is_dir() {
            let files = walk_logs(path);
            if files.is_empty() {
                bail!("no log files in {}", path.display());
            }
            return Ok(files);
        }
        if path.is_file() {
            return Ok(vec![path.to_path_buf()]);
        }
        bail!("path not found: {pattern}");
    }
    let re = glob_regex(pattern)?;
    let root = literal_root(pattern);
    let root_path = if root.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(root)
    };
    if !root_path.exists() {
        bail!("glob root not found: {}", root_path.display());
    }
    let mut out = Vec::new();
    for ent in WalkDir::new(&root_path).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        name != "target" && name != ".git" && name != "node_modules"
    }) {
        let ent = match ent {
            Ok(ent) => ent,
            Err(_) => continue,
        };
        if !ent.file_type().is_file() {
            continue;
        }
        let text = ent
            .path()
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        if re.is_match(&text) {
            out.push(ent.path().to_path_buf());
        }
    }
    out.sort();
    if out.is_empty() {
        bail!("glob matched no files: {pattern}");
    }
    Ok(out)
}

fn walk_logs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for ent in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if ent.file_type().is_file() && is_log_name(ent.path()) {
            out.push(ent.path().to_path_buf());
        }
    }
    out.sort();
    out
}

fn is_log_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let trimmed = name
        .trim_end_matches(".gz")
        .trim_end_matches(".zst")
        .trim_end_matches(".zstd");
    let ext = trimmed.rsplit('.').next().unwrap_or("");
    matches!(ext, "log" | "jsonl" | "json" | "txt" | "out")
        || trimmed.contains("log")
        || name.ends_with(".gz")
        || name.ends_with(".zst")
}

fn contains_meta(pattern: &str) -> bool {
    pattern.chars().any(|c| matches!(c, '*' | '?' | '['))
}

fn literal_root(pattern: &str) -> &str {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !matches!(bytes[i], b'*' | b'?' | b'[') {
        i += 1;
    }
    let prefix = &pattern[..i];
    match prefix.rfind('/') {
        Some(pos) => &prefix[..=pos],
        None => "",
    }
}

fn glob_regex(pattern: &str) -> Result<Regex> {
    let mut out = String::from("^");
    let p = pattern.as_bytes();
    let mut i = 0;
    while i < p.len() {
        if p[i] == b'*' && i + 1 < p.len() && p[i + 1] == b'*' {
            i += 2;
            if i < p.len() && p[i] == b'/' {
                i += 1;
            }
            out.push_str("(?:.*/)?");
            continue;
        }
        match p[i] {
            b'*' => out.push_str("[^/]*"),
            b'?' => out.push_str("[^/]"),
            c @ (b'.' | b'+' | b'(' | b')' | b'|' | b'^' | b'$' | b'{' | b'}' | b'[' | b']'
            | b'\\') => {
                out.push('\\');
                out.push(c as char);
            }
            c => out.push(c as char),
        }
        i += 1;
    }
    out.push('$');
    Regex::new(&out).with_context(|| format!("bad glob {pattern}"))
}

pub fn parse_ssh_spec(spec: &str) -> Result<(String, String)> {
    let (target, path) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("ssh spec must look like user@host:/absolute/path"))?;
    if target.is_empty() || target.contains([' ', '\'']) {
        bail!("ssh target must look like user@host");
    }
    if !path.starts_with('/') {
        bail!("ssh path must be absolute: {path}");
    }
    validate_remote_path(path)?;
    Ok((target.to_string(), path.to_string()))
}

pub fn ssh_host(spec: &str) -> Result<String> {
    let (target, _) = parse_ssh_spec(spec)?;
    Ok(target
        .rsplit_once('@')
        .map(|(_, host)| host.to_string())
        .unwrap_or(target))
}

fn validate_remote_path(path: &str) -> Result<()> {
    if path.chars().any(|c| {
        matches!(
            c,
            ';' | '&' | '|' | '$' | '`' | '(' | ')' | '<' | '>' | '\n' | '\r' | '\'' | '"' | '\\'
        )
    }) {
        bail!("remote path contains unsupported shell characters");
    }
    Ok(())
}

pub fn read_ssh(spec: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let (target, remote) = parse_ssh_spec(spec)?;
    let script = format!(
        "for f in {remote}; do if [ -f \"$f\" ]; then printf '@@@LOGFOG-FILE %s\\n' \"$f\"; tail -n +1 -- \"$f\"; printf '\\n'; fi; done"
    );
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &target,
            "sh",
            "-c",
            &script,
        ])
        .output()
        .context("failed to spawn ssh")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("ssh failed: {}", err.trim());
    }
    let chunks = split_marked(&out.stdout, spec);
    if chunks.is_empty() || chunks.iter().all(|(_, b)| b.is_empty()) {
        bail!("ssh source produced no files: {spec}");
    }
    Ok(chunks)
}

pub fn split_marked(buf: &[u8], fallback: &str) -> Vec<(String, Vec<u8>)> {
    let text = String::from_utf8_lossy(buf);
    if !text.contains("@@@LOGFOG-FILE ") {
        if buf.is_empty() {
            return Vec::new();
        }
        return vec![(fallback.to_string(), buf.to_vec())];
    }
    let mut out = Vec::new();
    let mut name = fallback.to_string();
    let mut body = String::new();
    let mut seen = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("@@@LOGFOG-FILE ") {
            if seen {
                out.push((name.clone(), std::mem::take(&mut body).into_bytes()));
            }
            name = rest.trim().to_string();
            seen = true;
        } else if seen {
            body.push_str(line);
            body.push('\n');
        }
    }
    if seen {
        out.push((name, body.into_bytes()));
    }
    out
}

pub fn validate_container(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
    {
        bail!("unsupported container name `{name}`");
    }
    Ok(())
}

pub fn read_docker(container: &str, tail: usize) -> Result<Vec<u8>> {
    validate_container(container)?;
    let tail = tail.to_string();
    let out = Command::new("docker")
        .args(["logs", "--timestamps", "--tail", &tail, container])
        .output()
        .context("failed to spawn docker")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("docker logs failed: {}", err.trim());
    }
    Ok(merge_stdio(out.stdout, out.stderr))
}

pub fn validate_unit(unit: &str) -> Result<()> {
    if unit.is_empty()
        || !unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | ':' | '\\'))
    {
        bail!("unsupported journald unit `{unit}`");
    }
    Ok(())
}

pub fn read_journal(unit: &str, lines: usize) -> Result<Vec<u8>> {
    validate_unit(unit)?;
    let n = lines.to_string();
    let out = Command::new("journalctl")
        .args(["-u", unit, "-o", "json", "--no-pager", "-n", &n])
        .output()
        .context("failed to spawn journalctl")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("journalctl failed: {}", err.trim());
    }
    Ok(out.stdout)
}

fn merge_stdio(mut stdout: Vec<u8>, stderr: Vec<u8>) -> Vec<u8> {
    if stderr.is_empty() {
        return stdout;
    }
    if !stdout.is_empty() && !stdout.ends_with(b"\n") {
        stdout.push(b'\n');
    }
    stdout.extend(stderr);
    stdout
}

pub fn docker_follow_args(container: &str, tail: usize) -> Result<Vec<String>> {
    validate_container(container)?;
    Ok(vec![
        "logs".into(),
        "--timestamps".into(),
        "--tail".into(),
        tail.to_string(),
        "-f".into(),
        container.to_string(),
    ])
}

pub fn journal_follow_args(unit: &str, lines: usize) -> Result<Vec<String>> {
    validate_unit(unit)?;
    Ok(vec![
        "-u".into(),
        unit.to_string(),
        "-o".into(),
        "json".into(),
        "-n".into(),
        lines.to_string(),
        "-f".into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_spec() {
        let (t, p) = parse_ssh_spec("alice@host:/var/log/app/*.log").unwrap();
        assert_eq!(t, "alice@host");
        assert_eq!(p, "/var/log/app/*.log");
        assert!(parse_ssh_spec("host:relative").is_err());
    }

    #[test]
    fn splits_ssh_markers() {
        let raw = b"@@@LOGFOG-FILE /var/log/a.log\nhello\n@@@LOGFOG-FILE /var/log/b.log\nworld\n";
        let chunks = split_marked(raw, "fallback");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, "/var/log/a.log");
        assert_eq!(chunks[0].1, b"hello\n");
        assert_eq!(chunks[1].1, b"world\n");
    }

    #[test]
    fn glob_star_star() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("logs").join("api");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("app.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.path().join("logs").join("skip.txt"), b"no\n").unwrap();
        let pattern = format!("{}/**/*.jsonl", dir.path().display());
        let hits = expand_input(&pattern).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with("app.jsonl"));
    }
}
