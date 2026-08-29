//! Synthetic text: code, build logs, prose, shell commands, and paths.
//!
//! Everything is composed from the word lists below plus seeded numbers and
//! hex digits. The lists were written for this benchmark; nothing here was
//! taken from a captured prompt, command, path, or tool output. The
//! vocabulary is deliberately a few hundred tokens with numeric and hex
//! noise mixed in so that zstd compresses it in the same ballpark as real
//! agent traffic (see `model::SAMPLED_SEGMENT_BYTES_PER_EVENT`).

use crate::rng::Rng;

const IDENTIFIERS: &[&str] = &[
    "config",
    "buffer",
    "handle",
    "parser",
    "stream",
    "segment",
    "manifest",
    "writer",
    "reader",
    "index",
    "cursor",
    "batch",
    "record",
    "schema",
    "field",
    "value",
    "result",
    "error",
    "context",
    "session",
    "event",
    "kind",
    "payload",
    "adapter",
    "runtime",
    "socket",
    "timeout",
    "retry",
    "queue",
    "worker",
    "commit",
    "token",
    "lexer",
    "planner",
    "engine",
    "table",
    "column",
    "row",
    "filter",
    "project",
    "scanner",
    "merge",
    "chunk",
    "frame",
    "header",
    "footer",
    "checksum",
    "digest",
    "entry",
    "key",
    "map",
    "set",
    "list",
    "vector",
    "option",
    "state",
    "machine",
    "clock",
    "timestamp",
    "duration",
    "interval",
    "window",
    "limit",
    "offset",
    "cache",
    "store",
    "loader",
    "opener",
    "closer",
    "append",
    "truncate",
    "rotate",
    "sync",
    "lock",
    "spawn",
    "join",
    "notify",
    "signal",
    "channel",
    "sender",
    "receiver",
    "request",
    "response",
    "client",
    "server",
    "route",
    "handler",
    "middleware",
    "policy",
    "budget",
    "quota",
    "counter",
    "gauge",
    "metric",
    "label",
    "span",
    "trace",
    "report",
    "summary",
    "outcome",
    "attempt",
    "turn",
    "prompt",
    "reply",
];

const TYPES: &[&str] = &[
    "u32",
    "u64",
    "i64",
    "usize",
    "bool",
    "String",
    "&str",
    "Vec<u8>",
    "Option<usize>",
    "Result<()>",
    "HashMap<String, Value>",
    "Timestamp",
    "EventId",
    "SessionId",
    "PathBuf",
    "Duration",
    "Arc<Mutex<State>>",
    "Box<dyn Error>",
    "Vec<Event>",
    "&mut Self",
    "f64",
    "u16",
];

const KEYWORDS: &[&str] = &[
    "let",
    "let mut",
    "pub fn",
    "fn",
    "pub struct",
    "impl",
    "match",
    "if",
    "else",
    "for",
    "while",
    "return",
    "use",
    "mod",
    "const",
    "static",
    "trait",
    "async fn",
    "await",
    "Some",
    "None",
    "Ok",
    "Err",
    "true",
    "false",
    "self",
    "def",
    "class",
    "import",
    "from",
    "return",
    "function",
    "export",
    "try",
    "catch",
    "finally",
    "yield",
    "assert",
    "lambda",
];

const LOG_LINES: &[&str] = &[
    "   Compiling {ident} v0.{n}.{n} ({path})",
    "warning: unused variable: `{ident}`",
    "  --> {path}:{n}:{n}",
    "   |",
    "{n} |     let {ident} = {ident}({ident}, {n});",
    "   |         ^^^^^^ help: if this is intentional, prefix it with an underscore",
    "test {ident}::{ident}::{ident} ... ok",
    "test {ident}::{ident} ... FAILED",
    "test result: ok. {n} passed; 0 failed; {n} ignored; 0 measured; 0 filtered out; finished in {n}.{n}s",
    "error[E0{n}]: mismatched types",
    "   = note: expected type `{type}`",
    "              found type `{type}`",
    "    Finished `release` profile [optimized] target(s) in {n}.{n}s",
    "     Running `target/debug/{ident}`",
    "thread 'main' panicked at {path}:{n}:{n}:",
    "assertion `left == right` failed",
    "  left: {n}",
    " right: {n}",
    "{hex}  {path}",
    "{ident}: {ident}={n} {ident}={n} elapsed={n}ms",
    "[{n}/{n}] {ident} {ident} {hex}",
    "-rw-r--r--  1 dev  staff  {n} {path}",
    "{path}:{n}:{ident}: {ident} {ident} {ident}",
    "+{ident}({ident}, {n})",
    "-{ident}({ident})",
    "@@ -{n},{n} +{n},{n} @@ fn {ident}",
    "PASS {ident}.{ident} ({n} ms)",
    "  {ident}: {n} {ident}, {n} {ident}",
    "INFO {ident} {ident}={hex} took {n}ms",
    "DEBUG {ident}::{ident} {ident}={n}",
    "Updating crates.io index",
    "  Downloaded {ident} v{n}.{n}.{n}",
    "{n} files changed, {n} insertions(+), {n} deletions(-)",
    "On branch {ident}-{ident}",
    "nothing to commit, working tree clean",
    "?? {path}",
    " M {path}",
];

const PROSE: &[&str] = &[
    "please",
    "add",
    "fix",
    "the",
    "a",
    "an",
    "and",
    "then",
    "make",
    "sure",
    "that",
    "this",
    "function",
    "should",
    "return",
    "when",
    "instead",
    "of",
    "also",
    "update",
    "tests",
    "so",
    "they",
    "pass",
    "explain",
    "why",
    "what",
    "how",
    "does",
    "check",
    "whether",
    "we",
    "need",
    "to",
    "handle",
    "case",
    "where",
    "is",
    "not",
    "can",
    "you",
    "look",
    "at",
    "file",
    "refactor",
    "into",
    "smaller",
    "pieces",
    "without",
    "changing",
    "behavior",
    "keep",
    "it",
    "simple",
    "run",
    "benchmark",
    "again",
    "compare",
    "results",
    "before",
    "after",
    "write",
    "summary",
    "docs",
    "section",
    "for",
    "new",
    "flag",
    "yes",
    "no",
    "continue",
    "go",
    "ahead",
    "looks",
    "good",
    "try",
    "another",
    "approach",
    "revert",
    "last",
    "change",
    "only",
    "touch",
    "module",
    "never",
    "delete",
    "data",
    "measure",
    "first",
    "report",
    "numbers",
    "with",
    "percentiles",
    "table",
    "in",
    "markdown",
    "under",
    "heading",
    "commit",
    "message",
    "branch",
    "merge",
    "conflict",
    "resolve",
    "carefully",
    "read",
    "spec",
    "implement",
    "parser",
    "grammar",
    "edge",
    "cases",
    "empty",
    "input",
    "unicode",
    "windows",
    "paths",
    "linux",
    "macos",
    "daemon",
    "socket",
    "hook",
    "latency",
    "budget",
    "milliseconds",
    "fsync",
    "durability",
    "relaxed",
    "strict",
    "mode",
];

const DIRS: &[&str] = &[
    "src",
    "crates",
    "lib",
    "core",
    "storage",
    "query",
    "capture",
    "adapters",
    "tests",
    "docs",
    "scripts",
    "internal",
    "pkg",
    "cmd",
    "app",
    "components",
    "hooks",
    "utils",
    "models",
    "services",
    "api",
    "web",
    "server",
    "client",
    "config",
    "bench",
    "examples",
    "fixtures",
];

const STEMS: &[&str] = &[
    "main",
    "lib",
    "mod",
    "db",
    "wal",
    "segment",
    "manifest",
    "spool",
    "frame",
    "codec",
    "event",
    "ids",
    "clock",
    "paths",
    "schema",
    "parser",
    "lexer",
    "exec",
    "tables",
    "graph",
    "result",
    "hook",
    "daemon",
    "ipc",
    "install",
    "doctor",
    "import",
    "state",
    "attempts",
    "projector",
    "handoff",
    "approach",
    "render",
    "cli",
    "ctx",
    "util",
    "types",
    "index",
    "handler",
    "router",
    "service",
    "model",
    "view",
    "controller",
    "helpers",
    "fixtures",
    "README",
    "CHANGELOG",
    "notes",
    "setup",
    "build",
    "deploy",
    "migrate",
    "seed",
    "report",
];

const PROJECT_WORDS: &[&str] = &[
    "orbit", "ledger", "harbor", "signal", "quartz", "meadow", "lantern", "compass", "beacon",
    "atlas", "ember", "glacier", "willow", "summit", "prairie", "cobalt", "saffron", "tundra",
];

const SHELL_TEMPLATES: &[&str] = &[
    "cargo test -p {ident} -- --nocapture",
    "cargo build --release",
    "cargo clippy -p {ident} --all-targets",
    "grep -rn \"{ident}\" {dir}/ | head -{n}",
    "git status --short",
    "git diff --stat",
    "git log --oneline -{n}",
    "sed -n '{n},{n}p' {path}",
    "ls -la {dir}",
    "python3 scripts/{ident}.py --{ident} {n}",
    "npm run {ident}",
    "wc -l {path}",
    "cat {path} | head -{n}",
    "find {dir} -name '*.{ident}' | wc -l",
    "rg -n \"{ident}\\(\" {dir} --type rust",
    "curl -s http://localhost:{n}/{ident} | head -c {n}",
    "mkdir -p {dir}/{ident} && touch {dir}/{ident}/{ident}.rs",
    "cargo run -p {ident} -- --{ident} {n} --{ident} {dir}",
    "du -sh {dir}",
    "tail -n {n} {path}",
];

fn number(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 => rng.range(0, 9).to_string(),
        1 => rng.range(10, 999).to_string(),
        2 => rng.range(1_000, 99_999).to_string(),
        _ => rng.range(100_000, 9_999_999).to_string(),
    }
}

fn ident(rng: &mut Rng) -> String {
    match rng.below(5) {
        0 => format!("{}_{}", rng.word(IDENTIFIERS), rng.word(IDENTIFIERS)),
        1 => format!("{}{}", rng.word(IDENTIFIERS), rng.range(1, 64)),
        _ => (*rng.word(IDENTIFIERS)).to_string(),
    }
}

/// Fill a template's `{ident}`, `{n}`, `{hex}`, `{path}`, `{dir}`, `{type}`
/// placeholders.
fn fill(rng: &mut Rng, template: &str) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        match key {
            "ident" => out.push_str(&ident(rng)),
            "n" => out.push_str(&number(rng)),
            "hex" => {
                let n = rng.range(7, 40) as usize;
                out.push_str(&rng.hex(n));
            }
            "path" => out.push_str(&relative_path(rng, 4, "rs")),
            "dir" => out.push_str(rng.word(DIRS)),
            "type" => out.push_str(rng.word(TYPES)),
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn truncate_to(mut s: String, len: usize) -> String {
    if s.len() > len {
        let mut cut = len;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// One line of plausible source code at a nesting depth.
fn code_line(rng: &mut Rng, depth: usize) -> String {
    let mut line = " ".repeat(depth * 4);
    let shape = rng.below(12);
    let body = match shape {
        0 => format!(
            "let {} = {}({}, {});",
            ident(rng),
            ident(rng),
            ident(rng),
            number(rng)
        ),
        1 => format!(
            "pub fn {}({}: {}, {}: {}) -> {} {{",
            ident(rng),
            ident(rng),
            rng.word(TYPES),
            ident(rng),
            rng.word(TYPES),
            rng.word(TYPES)
        ),
        2 => "}".to_string(),
        3 => format!(
            "// {} {} {} {}",
            rng.word(PROSE),
            rng.word(PROSE),
            rng.word(PROSE),
            rng.word(PROSE)
        ),
        4 => format!(
            "if {}.{}() {{ return Err({}::{}({})); }}",
            ident(rng),
            ident(rng),
            ident(rng),
            ident(rng),
            number(rng)
        ),
        5 => format!("assert_eq!({}, {});", ident(rng), number(rng)),
        6 => format!(
            "{} {}: {} = {};",
            rng.word(KEYWORDS),
            ident(rng),
            rng.word(TYPES),
            number(rng)
        ),
        7 => format!(
            "for {} in {}.iter().filter(|{}| {}.{} > {}) {{",
            ident(rng),
            ident(rng),
            ident(rng),
            ident(rng),
            ident(rng),
            number(rng)
        ),
        8 => format!("{}.{}(&{})?;", ident(rng), ident(rng), ident(rng)),
        9 => format!(
            "\"{}\": {{\"{}\": {}, \"{}\": \"{}\"}},",
            ident(rng),
            ident(rng),
            number(rng),
            ident(rng),
            rng.hex(12)
        ),
        10 => format!(
            "{} = {} + {} * {}",
            ident(rng),
            ident(rng),
            ident(rng),
            number(rng)
        ),
        _ => format!(
            "match {} {{ {}::{} => {}, _ => {} }}",
            ident(rng),
            ident(rng),
            ident(rng),
            number(rng),
            number(rng)
        ),
    };
    line.push_str(&body);
    line
}

/// Source-code-like text of about `len` bytes.
pub fn code(rng: &mut Rng, len: usize) -> String {
    let mut s = String::with_capacity(len + 64);
    let mut depth = 0usize;
    while s.len() < len {
        let line = code_line(rng, depth);
        if line.trim_end().ends_with('{') {
            depth = (depth + 1).min(4);
        } else if line.trim() == "}" {
            depth = depth.saturating_sub(1);
        }
        s.push_str(&line);
        s.push('\n');
    }
    truncate_to(s, len)
}

/// Build/test/shell log text of about `len` bytes.
pub fn log(rng: &mut Rng, len: usize) -> String {
    let mut s = String::with_capacity(len + 64);
    while s.len() < len {
        let t = rng.word(LOG_LINES);
        s.push_str(&fill(rng, t));
        s.push('\n');
    }
    truncate_to(s, len)
}

/// Human-style prose of about `len` bytes, with an occasional code fence.
pub fn prose(rng: &mut Rng, len: usize) -> String {
    let mut s = String::with_capacity(len + 64);
    let mut words = 0;
    while s.len() < len {
        if words > 0 && rng.chance(0.02) {
            s.push_str("\n\n```\n");
            let fence_len = rng.range(60, 400) as usize;
            s.push_str(&code(rng, fence_len));
            s.push_str("\n```\n\n");
        }
        let w = rng.word(PROSE);
        if words == 0 {
            let mut c = w.chars();
            if let Some(f) = c.next() {
                s.push(f.to_ascii_uppercase());
                s.push_str(c.as_str());
            }
        } else {
            s.push_str(w);
        }
        words += 1;
        if rng.chance(0.08) {
            s.push_str(if rng.chance(0.3) { "?\n" } else { ".\n" });
            words = 0;
        } else {
            s.push(' ');
        }
    }
    truncate_to(s, len)
}

/// A shell command of about `len` bytes. Short commands come from the
/// templates; long ones are heredocs or inline scripts.
pub fn shell_command(rng: &mut Rng, len: usize) -> String {
    let template = rng.word(SHELL_TEMPLATES);
    let base = fill(rng, template);
    if len <= base.len() + 16 {
        return truncate_to(base, len.max(4));
    }
    let mut s = String::with_capacity(len + 32);
    if rng.chance(0.5) {
        s.push_str("cat > ");
        s.push_str(&relative_path(rng, 4, "rs"));
        s.push_str(" <<'EOF'\n");
        let body = len.saturating_sub(s.len() + 5);
        s.push_str(&code(rng, body));
        s.push_str("\nEOF\n");
    } else {
        s.push_str(&base);
        s.push_str(" && ");
        while s.len() < len {
            let template = rng.word(SHELL_TEMPLATES);
            s.push_str(&fill(rng, template));
            s.push_str(" && ");
        }
    }
    truncate_to(s, len)
}

/// A one-line description of a command or task.
pub fn description(rng: &mut Rng) -> String {
    let n = rng.range(3, 8) as usize;
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(rng.word(PROSE));
    }
    s
}

/// A repository-relative path with `depth` components and extension `ext`.
pub fn relative_path(rng: &mut Rng, depth: usize, ext: &str) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(depth);
    for i in 0..depth.max(1) {
        if i + 1 == depth.max(1) {
            parts.push(format!("{}.{}", rng.word(STEMS), ext));
        } else {
            parts.push((*rng.word(DIRS)).to_string());
        }
    }
    parts.join("/")
}

/// A project name like `orbit-ledger`.
pub fn project_name(rng: &mut Rng) -> String {
    format!("{}-{}", rng.word(PROJECT_WORDS), rng.word(PROJECT_WORDS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_respected() {
        let mut r = Rng::new(3);
        for len in [0usize, 5, 100, 4_000, 60_000] {
            assert_eq!(code(&mut r, len).len(), len);
            assert_eq!(log(&mut r, len).len(), len);
            assert_eq!(prose(&mut r, len).len(), len);
            assert!(shell_command(&mut r, len).len() <= len.max(4));
        }
        let p = relative_path(&mut r, 4, "rs");
        assert_eq!(p.matches('/').count(), 3);
        assert!(p.ends_with(".rs"));
    }
}
