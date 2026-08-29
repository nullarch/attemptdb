//! Tokeniser for AttemptQL.
//!
//! Tokens carry byte offsets so parse errors can point at the input. The
//! lexer never panics: every unexpected character becomes a positional
//! parse error.

use crate::error::{QueryError, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokKind {
    /// Keyword, identifier, or id (`SHOW`, `attempts`, `ses_0191…`).
    Word(String),
    /// Single-quoted string with `''` escapes already unescaped.
    Str(String),
    /// Integer or decimal literal text.
    Num(String),
    /// Relative timestamp such as `-15m`.
    Relative(String),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    /// The token as the user typed it (for error messages).
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        source.get(self.start..self.end).unwrap_or("")
    }
}

const TWO_CHAR: &[&str] = &["<=", ">=", "<>", "!=", "||", "::"];
const ONE_CHAR: &[&str] = &[
    "=", "<", ">", "(", ")", "+", "-", "*", "/", "%", ",", ".", ";", "[", "]", "!", "|", ":",
];

fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Whether the atom so far may continue through a hyphen as a UUID-like id:
/// everything after an optional `xxx_` prefix must be hex or hyphens.
fn hyphen_continues(atom: &str) -> bool {
    let rest = match atom.find('_') {
        Some(i) => &atom[i + 1..],
        None => atom,
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn lex(text: &str) -> Result<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // `--` line comment.
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'\'' {
            let start = i;
            let mut value = String::new();
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        value.push('\'');
                        i += 2;
                        continue;
                    }
                    closed = true;
                    i += 1;
                    break;
                }
                // Copy one UTF-8 character.
                let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
                value.push(ch);
                i += ch.len_utf8();
            }
            if !closed {
                return Err(QueryError::parse("unterminated string literal", start));
            }
            out.push(Token {
                kind: TokKind::Str(value),
                start,
                end: i,
            });
            continue;
        }
        if c == b'"' {
            // Quoted identifier: kept verbatim (quotes included) as a word.
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            if i >= bytes.len() {
                return Err(QueryError::parse("unterminated quoted identifier", start));
            }
            i += 1;
            out.push(Token {
                kind: TokKind::Word(text[start..i].to_string()),
                start,
                end: i,
            });
            continue;
        }
        if c == b'-' {
            // Relative timestamp: `-` digits unit, not followed by a word char.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1
                && j < bytes.len()
                && matches!(bytes[j], b's' | b'm' | b'h' | b'd' | b'w')
                && bytes.get(j + 1).is_none_or(|b| !is_word_char(*b))
            {
                out.push(Token {
                    kind: TokKind::Relative(text[i..=j].to_string()),
                    start: i,
                    end: j + 1,
                });
                i = j + 1;
                continue;
            }
        }
        if is_word_char(c) {
            let start = i;
            while i < bytes.len() && is_word_char(bytes[i]) {
                i += 1;
            }
            // Decimal literal.
            if bytes[start].is_ascii_digit()
                && bytes.get(i) == Some(&b'.')
                && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            // UUID-like continuation through hyphens.
            while bytes.get(i) == Some(&b'-')
                && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                && hyphen_continues(&text[start..i])
            {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            }
            let atom = &text[start..i];
            let kind = if atom.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
                TokKind::Num(atom.to_string())
            } else {
                TokKind::Word(atom.to_string())
            };
            out.push(Token {
                kind,
                start,
                end: i,
            });
            continue;
        }
        if let Some(p) = TWO_CHAR.iter().find(|p| text[i..].starts_with(**p)) {
            out.push(Token {
                kind: TokKind::Punct(p),
                start: i,
                end: i + 2,
            });
            i += 2;
            continue;
        }
        if let Some(p) = ONE_CHAR.iter().find(|p| text[i..].starts_with(**p)) {
            out.push(Token {
                kind: TokKind::Punct(p),
                start: i,
                end: i + 1,
            });
            i += 1;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        return Err(QueryError::parse(format!("unexpected character '{ch}'"), i));
    }
    out.push(Token {
        kind: TokKind::Eof,
        start: text.len(),
        end: text.len(),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<TokKind> {
        lex(s).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn words_strings_numbers_relatives() {
        assert_eq!(
            kinds("SHOW ATTEMPTS LIMIT 5 SINCE -15m -- comment"),
            vec![
                TokKind::Word("SHOW".into()),
                TokKind::Word("ATTEMPTS".into()),
                TokKind::Word("LIMIT".into()),
                TokKind::Num("5".into()),
                TokKind::Word("SINCE".into()),
                TokKind::Relative("-15m".into()),
                TokKind::Eof,
            ]
        );
        assert_eq!(
            kinds("session = 'it''s'"),
            vec![
                TokKind::Word("session".into()),
                TokKind::Punct("="),
                TokKind::Str("it's".into()),
                TokKind::Eof
            ]
        );
    }

    #[test]
    fn ids_with_hyphens_stay_whole() {
        let id = "ses_0191c2a3-1b2c-7d3e-8f4a-5b6c7d8e9f00";
        assert_eq!(kinds(id), vec![TokKind::Word(id.into()), TokKind::Eof]);
        let bare = "0191c2a3-1b2c-7d3e-8f4a-5b6c7d8e9f00";
        assert_eq!(kinds(bare), vec![TokKind::Word(bare.into()), TokKind::Eof]);
        // Arithmetic is not swallowed.
        assert_eq!(
            kinds("a - b"),
            vec![
                TokKind::Word("a".into()),
                TokKind::Punct("-"),
                TokKind::Word("b".into()),
                TokKind::Eof
            ]
        );
    }

    #[test]
    fn errors_are_positional() {
        match lex("SHOW 'oops").unwrap_err() {
            QueryError::Parse { position, .. } => assert_eq!(position, 5),
            other => panic!("unexpected {other:?}"),
        }
        match lex("SHOW ATTEMPTS €").unwrap_err() {
            QueryError::Parse { position, message } => {
                assert_eq!(position, 14);
                assert!(message.contains('€'));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
