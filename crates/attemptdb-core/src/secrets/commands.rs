//! Credentials on a command line, and in a `.netrc`.
//!
//! These rules need the command around the flag: `-p` is the password for
//! `mysql`, a port for `ssh` and "make parents" for `mkdir`, so a flag is read
//! only as part of the command that gives it that meaning.
//!
//! The scanner calls [`parse`] at the start of a word. When the word names a
//! command the rules know, [`parse`] reads that one command line (up to a line
//! break, `;`, `|`, `&`, `<` or `>` outside quotes, at most [`MAX_LINE`]
//! bytes) and reports every credential in it. The scan hands those spans out
//! when it reaches them, so nothing else in the line is skipped, and it does
//! not read another command inside the line it just read: every byte is read
//! by at most one command line, which keeps the cost linear.
//!
//! A word that follows a flag is the credential only when the flag is one the
//! command documents. Placeholders (`password`, `<pass>`, `$PASS`, `****`)
//! are not credentials.

use super::{Hit, is_placeholder, is_reference};

const RULE: &str = "command_line_credential";

/// How far into the text one command line is read.
const MAX_LINE: usize = 2048;

/// How many words of one command line are read.
const MAX_WORDS: usize = 128;

/// How many words of a registry command are read looking for `login`.
const MAX_LOGIN_LOOKAHEAD: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Curl,
    Mysql,
    Sshpass,
    /// `docker login`, `podman login`, `az acr login`, `skopeo --creds`, …
    Registry,
    Redis,
    Mongo,
    Openssl,
    Htpasswd,
}

/// Whether a command word can start with this byte: the first bytes of every
/// name [`kind_of`] knows, plus the netrc keywords.
pub(super) fn starts_command(c: u8) -> bool {
    matches!(
        c,
        b'c' | b'm' | b's' | b'd' | b'p' | b'n' | b'b' | b'o' | b'h' | b'a' | b'r'
    )
}

fn kind_of(word: &[u8]) -> Option<Kind> {
    let word = word.strip_suffix(b".exe").unwrap_or(word);
    Some(match word {
        b"curl" => Kind::Curl,
        b"mysql" | b"mysqldump" | b"mysqladmin" | b"mysqlimport" | b"mysqlshow" | b"mysqlcheck"
        | b"mysqlslap" | b"mysqlbinlog" | b"mariadb" | b"mariadb-dump" | b"mariadb-admin"
        | b"mariadb-import" | b"mariadb-show" | b"mariadb-check" | b"mariadb-slap" => Kind::Mysql,
        b"sshpass" => Kind::Sshpass,
        b"docker" | b"podman" | b"nerdctl" | b"buildah" | b"skopeo" | b"oras" | b"crane"
        | b"helm" | b"az" => Kind::Registry,
        b"redis-cli" => Kind::Redis,
        b"mongo" | b"mongosh" | b"mongodump" | b"mongorestore" | b"mongoexport"
        | b"mongoimport" | b"mongostat" | b"mongotop" | b"mongofiles" => Kind::Mongo,
        b"openssl" => Kind::Openssl,
        b"htpasswd" => Kind::Htpasswd,
        _ => return None,
    })
}

/// Words that stand where a password goes in documentation and examples.
fn is_stand_in(v: &str) -> bool {
    is_placeholder(v)
        || [
            "password",
            "passwd",
            "pass",
            "pwd",
            "secret",
            "token",
            "apikey",
            "api_key",
            "changeme",
            "yourpassword",
            "mypassword",
            "mysecret",
        ]
        .iter()
        .any(|w| v.eq_ignore_ascii_case(w))
}

fn is_credential(v: &str) -> bool {
    !v.is_empty()
        && !is_stand_in(v)
        && !is_reference(v)
        && !v.starts_with(['-', '`'])
        && !v.contains("://")
}

/// One word of a command line: its extent in the text, quotes included.
#[derive(Clone, Copy)]
struct Word {
    start: usize,
    end: usize,
}

struct Args<'a> {
    text: &'a str,
    b: &'a [u8],
    p: usize,
    limit: usize,
    words: usize,
}

impl<'a> Args<'a> {
    fn new(text: &'a str, from: usize) -> Self {
        let b = text.as_bytes();
        Self {
            text,
            b,
            p: from,
            limit: b.len().min(from + MAX_LINE),
            words: 0,
        }
    }

    /// The next word, or `None` at the end of the command line.
    fn next(&mut self) -> Option<Word> {
        let b = self.b;
        loop {
            if self.p >= self.limit {
                return None;
            }
            match b[self.p] {
                b' ' | b'\t' => self.p += 1,
                // A backslash before the line break continues the command.
                b'\\' if b.get(self.p + 1) == Some(&b'\n') => self.p += 2,
                b'\\' if b.get(self.p + 1) == Some(&b'\r') && b.get(self.p + 2) == Some(&b'\n') => {
                    self.p += 3
                }
                b'\n' | b'\r' | b';' | b'|' | b'&' | b'<' | b'>' => return None,
                _ => break,
            }
        }
        if self.words >= MAX_WORDS {
            return None;
        }
        self.words += 1;
        let start = self.p;
        let mut quote = 0u8;
        while self.p < self.limit {
            let c = b[self.p];
            if quote != 0 {
                if c == b'\\' && quote == b'"' {
                    self.p += 2;
                    continue;
                }
                if c == quote {
                    quote = 0;
                }
            } else {
                match c {
                    b'\'' | b'"' => quote = c,
                    b'\\' => {
                        self.p += 2;
                        continue;
                    }
                    b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'<' | b'>' => break,
                    _ => {}
                }
            }
            self.p += 1;
        }
        // An escape at the very end can step past the cap, or into the middle
        // of a character.
        let mut end = self.p.min(self.limit);
        while end > start && !self.text.is_char_boundary(end) {
            end -= 1;
        }
        self.p = end;
        Some(Word { start, end })
    }

    fn bytes(&self, w: Word) -> &'a [u8] {
        &self.b[w.start..w.end]
    }

    /// The word after a flag, as long as it is not another flag.
    fn value(&mut self) -> Option<Word> {
        let w = self.next()?;
        (self.b[w.start] != b'-').then_some(w)
    }

    /// The span of `w` without its quotes (`"x"`, `'x'`, `\"x\"`).
    fn unquoted(&self, w: Word) -> (usize, usize) {
        self.trim(w.start, w.end)
    }

    fn trim(&self, mut s: usize, mut e: usize) -> (usize, usize) {
        let b = self.b;
        let quote = |c: u8| matches!(c, b'"' | b'\'');
        if e - s >= 4
            && b[s] == b'\\'
            && quote(b[s + 1])
            && b[e - 2] == b'\\'
            && b[e - 1] == b[s + 1]
        {
            s += 2;
            e -= 2;
        } else if e - s >= 2 && quote(b[s]) && b[e - 1] == b[s] {
            s += 1;
            e -= 1;
        } else if e - s >= 1 && quote(b[s]) {
            s += 1;
        }
        (s, e)
    }
}

/// The credential in the span `[s, e)`, as a hit.
fn secret(args: &Args, s: usize, e: usize) -> Option<Hit> {
    let (s, e) = args.trim(s, e);
    (e > s && is_credential(&args.text[s..e])).then_some(Hit {
        rule: RULE,
        start: s,
        end: e,
    })
}

/// The password half of a `user:password` word (curl `-u`).
fn user_password(args: &Args, s: usize, e: usize) -> Option<Hit> {
    let (s, e) = args.trim(s, e);
    let colon = args.b[s..e].iter().position(|c| *c == b':')?;
    secret(args, s + colon + 1, e)
}

/// Read the command line that starts at `i` (a word start). Returns the
/// credentials in it, in order, and where the reading stopped.
pub(super) fn parse(text: &str, b: &[u8], i: usize) -> (Vec<Hit>, usize) {
    let word_len = b[i..]
        .iter()
        .take(25)
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        .count();
    let after = i + word_len;
    if word_len == 0 || word_len > 24 || !matches!(b.get(after), Some(b' ') | Some(b'\t')) {
        return (Vec::new(), i);
    }
    let word = &b[i..after];
    if word == b"machine" || word == b"default" {
        return netrc(text, b, i, after);
    }
    let Some(kind) = kind_of(word) else {
        return (Vec::new(), i);
    };
    let mut args = Args::new(text, after);
    let mut hits = Vec::new();
    let mut saw_login = false;
    let mut positional: Vec<Word> = Vec::new();
    let mut batch = false;
    while let Some(w) = args.next() {
        let tok = args.bytes(w);
        match kind {
            Kind::Curl => {
                if tok.starts_with(b"--") {
                    let (name, attached) = match tok.iter().position(|c| *c == b'=') {
                        Some(eq) => (&tok[..eq], Some(w.start + eq + 1)),
                        None => (tok, None),
                    };
                    let value = |args: &mut Args| match attached {
                        Some(s) => Some((s, w.end)),
                        None => args.value().map(|v| (v.start, v.end)),
                    };
                    match name {
                        b"--user" | b"--proxy-user" => {
                            if let Some((s, e)) = value(&mut args) {
                                hits.extend(user_password(&args, s, e));
                            }
                        }
                        b"--oauth2-bearer" => {
                            if let Some((s, e)) = value(&mut args) {
                                hits.extend(secret(&args, s, e));
                            }
                        }
                        _ => {}
                    }
                } else if tok.len() >= 2 && tok[0] == b'-' {
                    let letters = tok[1..].iter().all(u8::is_ascii_alphabetic);
                    if letters && matches!(tok[tok.len() - 1], b'u' | b'U') {
                        // `-u user:pass`, and `-su user:pass` (bundled flags).
                        if let Some(v) = args.value() {
                            hits.extend(user_password(&args, v.start, v.end));
                        }
                    } else if matches!(tok[1], b'u' | b'U') && tok.contains(&b':') {
                        // `-uuser:pass`.
                        hits.extend(user_password(&args, w.start + 2, w.end));
                    }
                }
            }
            Kind::Mysql => {
                // `-pSECRET`: the password is attached. `-p SECRET` names a
                // database, and a bare `-p` asks for the password.
                if tok.len() > 2 && tok.starts_with(b"-p") && tok[2] != b'-' {
                    hits.extend(secret(&args, w.start + 2, w.end));
                }
            }
            Kind::Sshpass => {
                if !tok.starts_with(b"-") {
                    // The command sshpass runs: its own flags (`ssh -p 22`)
                    // are not sshpass's.
                    break;
                }
                match tok {
                    b"-p" => {
                        if let Some(v) = args.value() {
                            hits.extend(secret(&args, v.start, v.end));
                        }
                    }
                    b"-f" | b"-d" | b"-P" => {
                        args.next();
                    }
                    _ if tok.len() > 2 && tok.starts_with(b"-p") => {
                        hits.extend(secret(&args, w.start + 2, w.end));
                    }
                    _ => {}
                }
            }
            Kind::Registry => {
                if tok == b"login" {
                    saw_login = true;
                } else if saw_login && matches!(tok, b"-p" | b"--password") {
                    if let Some(v) = args.value() {
                        hits.extend(secret(&args, v.start, v.end));
                    }
                } else if saw_login && tok.len() > 2 && tok.starts_with(b"-p") && tok[2] != b'-' {
                    hits.extend(secret(&args, w.start + 2, w.end));
                } else if matches!(tok, b"--creds" | b"--src-creds" | b"--dest-creds") {
                    if let Some(v) = args.value() {
                        hits.extend(user_password(&args, v.start, v.end));
                    }
                } else if let Some(eq) = tok.iter().position(|c| *c == b'=')
                    && matches!(&tok[..eq], b"--creds" | b"--src-creds" | b"--dest-creds")
                {
                    hits.extend(user_password(&args, w.start + eq + 1, w.end));
                }
                // Without `login` among the first words this is not a login
                // (`docker run -p 80:80`): stop reading.
                if !saw_login && args.words >= MAX_LOGIN_LOOKAHEAD {
                    break;
                }
            }
            Kind::Redis => {
                if matches!(tok, b"-a" | b"--pass")
                    && let Some(v) = args.value()
                {
                    hits.extend(secret(&args, v.start, v.end));
                }
            }
            Kind::Mongo => {
                if matches!(tok, b"-p" | b"--password") {
                    if let Some(v) = args.value() {
                        hits.extend(secret(&args, v.start, v.end));
                    }
                } else if tok.len() > 2 && tok.starts_with(b"-p") && tok[2] != b'-' {
                    hits.extend(secret(&args, w.start + 2, w.end));
                }
            }
            Kind::Openssl => {
                if matches!(tok, b"-pass" | b"-passin" | b"-passout")
                    && let Some(v) = args.value()
                {
                    let (s, e) = args.unquoted(v);
                    if b[s..e].starts_with(b"pass:") {
                        hits.extend(secret(&args, s + 5, e));
                    }
                }
            }
            Kind::Htpasswd => {
                if tok.len() > 1 && tok[0] == b'-' {
                    if tok[1..].iter().all(u8::is_ascii_alphabetic) && tok[1..].contains(&b'b') {
                        batch = true;
                    }
                    if tok == b"-C" {
                        args.next();
                    }
                } else {
                    positional.push(w);
                }
            }
        }
    }
    if kind == Kind::Htpasswd && batch && positional.len() >= 2 {
        let w = positional[positional.len() - 1];
        hits.extend(secret(&args, w.start, w.end));
    }
    if kind == Kind::Registry && !saw_login && hits.is_empty() {
        // Nothing was a login: do not claim the line, so a command inside it
        // (`docker exec web curl -u …`) is still read.
        return (hits, i);
    }
    hits.sort_by_key(|h| h.start);
    (hits, args.p)
}

/// `machine example.com login me password secret`, and the multi-line form:
/// the word after `password` in a `.netrc` entry. The entry must be shaped
/// like one (`machine <host>` or `default`, then keyword/value pairs), so the
/// words "machine" and "password" in prose do not match.
fn netrc(text: &str, b: &[u8], i: usize, after: usize) -> (Vec<Hit>, usize) {
    let limit = b.len().min(i + MAX_LINE);
    let mut p = after;
    let word = |p: &mut usize| -> Option<(usize, usize)> {
        while *p < limit && b[*p].is_ascii_whitespace() {
            *p += 1;
        }
        let s = *p;
        while *p < limit && !b[*p].is_ascii_whitespace() {
            *p += 1;
        }
        let e = (*p).min(limit);
        (e > s && text.is_char_boundary(e)).then_some((s, e))
    };
    let mut hits = Vec::new();
    if &b[i..after] == b"machine" {
        if word(&mut p).is_none() {
            return (hits, after);
        }
    } else {
        // `default` must be followed by an entry's keywords.
        let mut peek = p;
        match word(&mut peek) {
            Some((s, e)) if matches!(&b[s..e], b"login" | b"password" | b"account") => {}
            _ => return (hits, after),
        }
    }
    let mut login = false;
    for _ in 0..16 {
        let before = p;
        let Some((s, e)) = word(&mut p) else {
            break;
        };
        match &b[s..e] {
            b"login" | b"account" | b"port" => {
                login |= &b[s..e] == b"login";
                if word(&mut p).is_none() {
                    break;
                }
            }
            b"password" => {
                let Some((vs, ve)) = word(&mut p) else {
                    break;
                };
                // An entry names who logs in: `machine learning password
                // reset` is a sentence.
                if login && is_credential(&text[vs..ve]) {
                    hits.push(Hit {
                        rule: "netrc_password",
                        start: vs,
                        end: ve,
                    });
                }
            }
            // The next entry, or something that is not a netrc keyword.
            _ => {
                p = before;
                break;
            }
        }
    }
    (hits, p)
}
