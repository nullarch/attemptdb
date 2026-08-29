//! Hand-written recursive-descent parser for AttemptQL (RFC 0004 §3).
//!
//! Keywords are case-insensitive; a single statement with an optional
//! trailing `;`. Every error is positional and names what was expected.

use super::ast::{
    DiffStatement, Direction, Filter, OrderBy, ShowStatement, ShowTarget, StateStatement,
    Statement, Subject, TimeExpr, TraceStatement, WhatIsStatement, WhyStatement,
};
use super::lexer::{TokKind, Token, lex};
use crate::error::{QueryError, Result};

/// Words that can never be an id or a value in subject position.
const RESERVED: &[&str] = &[
    "SHOW",
    "WHY",
    "TRACE",
    "STATE",
    "DIFF",
    "WHAT",
    "IS",
    "DOING",
    "NOW",
    "STATUS",
    "CAUSES",
    "AT",
    "FOR",
    "WHERE",
    "SINCE",
    "UNTIL",
    "ORDER",
    "BY",
    "LIMIT",
    "DEPTH",
    "DIRECTION",
    "AND",
    "BETWEEN",
    "EXPLAIN",
    "ATTEMPTS",
    "SESSIONS",
    "TURNS",
    "HANDOFFS",
    "DECISIONS",
    "EVIDENCE",
    "INCLUDING",
    "RETRACTED",
    "WORK",
    "UNITS",
    "CORRECTIONS",
    "RETRACTIONS",
];

const FILTER_KEYS: &str =
    "project, provider, agent, session, turn, path, outcome, tool, status, phase, since, until";

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Token>,
    pos: usize,
}

/// Parse one AttemptQL statement.
pub fn parse(text: &str) -> Result<Statement> {
    let toks = lex(text)?;
    let mut p = Parser { text, toks, pos: 0 };
    if p.at_eof() {
        return Err(QueryError::parse(
            "empty statement; expected SHOW, WHY, TRACE, STATE, DIFF, WHAT IS or EXPLAIN",
            0,
        ));
    }
    let explain = p.eat_word("EXPLAIN");
    let stmt = p.statement()?;
    p.eat_punct(";");
    if !p.at_eof() {
        return Err(p.unexpected("end of statement"));
    }
    Ok(if explain {
        Statement::Explain(Box::new(stmt))
    } else {
        stmt
    })
}

impl Parser<'_> {
    // --- token helpers -----------------------------------------------------

    fn peek(&self) -> &Token {
        // The token vector always ends with Eof, so this cannot go out of
        // bounds unless `pos` was advanced past it, which `next` prevents.
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokKind::Eof)
    }

    fn is_word(&self, kw: &str) -> bool {
        matches!(&self.peek().kind, TokKind::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn eat_word(&mut self, kw: &str) -> bool {
        if self.is_word(kw) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, kw: &str) -> Result<()> {
        if self.eat_word(kw) {
            Ok(())
        } else {
            Err(self.unexpected(kw))
        }
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if matches!(self.peek().kind, TokKind::Punct(q) if q == p) {
            self.next();
            true
        } else {
            false
        }
    }

    fn describe(&self, t: &Token) -> String {
        match &t.kind {
            TokKind::Eof => "end of statement".to_string(),
            TokKind::Str(_) => format!("string {}", quote(t.text(self.text))),
            _ => format!("'{}'", t.text(self.text)),
        }
    }

    fn unexpected(&self, expected: &str) -> QueryError {
        let t = self.peek();
        let what = if matches!(t.kind, TokKind::Eof) {
            "unexpected end of statement".to_string()
        } else {
            format!("unexpected token {}", self.describe(t))
        };
        QueryError::parse(format!("{what}; expected {expected}"), t.start)
    }

    fn integer(&mut self, what: &str) -> Result<usize> {
        let t = self.peek().clone();
        if let TokKind::Num(n) = &t.kind
            && let Ok(v) = n.parse::<usize>()
        {
            self.next();
            return Ok(v);
        }
        Err(self.unexpected(&format!("an integer after {what}")))
    }

    /// A string, word, or number used as a value.
    fn value(&mut self, what: &str) -> Result<String> {
        let t = self.peek().clone();
        match &t.kind {
            TokKind::Str(s) => {
                self.next();
                Ok(s.clone())
            }
            TokKind::Word(w) if !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w)) => {
                self.next();
                Ok(w.clone())
            }
            TokKind::Num(n) => {
                self.next();
                Ok(n.clone())
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn time(&mut self) -> Result<TimeExpr> {
        let t = self.peek().clone();
        let parsed = match &t.kind {
            TokKind::Str(s) | TokKind::Num(s) | TokKind::Relative(s) => TimeExpr::parse_literal(s),
            TokKind::Word(w) => TimeExpr::parse_literal(w).filter(|_| {
                matches!(
                    w.to_ascii_lowercase().as_str(),
                    "now" | "today" | "yesterday"
                )
            }),
            _ => None,
        };
        match parsed {
            Some(expr) => {
                self.next();
                Ok(expr)
            }
            None => {
                if matches!(t.kind, TokKind::Str(_) | TokKind::Num(_)) {
                    Err(QueryError::parse(
                        format!(
                            "invalid timestamp {}; expected RFC 3339 ('2026-08-28T14:00:00Z'), a date ('2026-08-28'), epoch seconds, now, today, yesterday, or a relative form like -15m / -2h / -1d",
                            self.describe(&t)
                        ),
                        t.start,
                    ))
                } else {
                    Err(self
                        .unexpected("a timestamp ('2026-08-28T14:00:00Z', now, -15m, yesterday)"))
                }
            }
        }
    }

    // --- statements ----------------------------------------------------------

    fn statement(&mut self) -> Result<Statement> {
        let t = self.peek().clone();
        let TokKind::Word(w) = &t.kind else {
            return Err(self.unexpected("SHOW, WHY, TRACE, STATE, DIFF or WHAT IS"));
        };
        match w.to_ascii_uppercase().as_str() {
            "SHOW" => self.show().map(Statement::Show),
            "WHY" => self.why().map(Statement::Why),
            "TRACE" => self.trace().map(Statement::Trace),
            "STATE" => self.state().map(Statement::State),
            "DIFF" => self.diff().map(Statement::Diff),
            "WHAT" => self.what_is().map(Statement::WhatIs),
            _ => Err(self.unexpected("SHOW, WHY, TRACE, STATE, DIFF or WHAT IS")),
        }
    }

    fn show(&mut self) -> Result<ShowStatement> {
        self.expect_word("SHOW")?;
        let target = self.show_target()?;
        let mut stmt = ShowStatement {
            target,
            filters: Vec::new(),
            predicate: None,
            since: None,
            until: None,
            order_by: None,
            limit: None,
            including_retracted: false,
        };
        loop {
            if self.eat_word("FOR") {
                stmt.filters.extend(self.filter_list()?);
            } else if self.eat_word("WHERE") {
                stmt.predicate = Some(self.predicate()?);
            } else if self.eat_word("SINCE") {
                stmt.since = Some(self.time()?);
            } else if self.eat_word("UNTIL") {
                stmt.until = Some(self.time()?);
            } else if self.eat_word("ORDER") {
                self.expect_word("BY")?;
                stmt.order_by = Some(self.order_by()?);
            } else if self.eat_word("LIMIT") {
                stmt.limit = Some(self.integer("LIMIT")?);
            } else if self.eat_word("INCLUDING") {
                self.expect_word("RETRACTED")?;
                stmt.including_retracted = true;
            } else if self.at_eof() || matches!(self.peek().kind, TokKind::Punct(";")) {
                break;
            } else {
                return Err(self.unexpected(
                    "FOR, WHERE, SINCE, UNTIL, ORDER BY, LIMIT, INCLUDING RETRACTED or end of statement",
                ));
            }
        }
        Ok(stmt)
    }

    fn show_target(&mut self) -> Result<ShowTarget> {
        const EXPECTED: &str = "ATTEMPTS, FAILED ATTEMPTS, SUPERSEDED ATTEMPTS, SESSIONS, TURNS, TOOL CALLS, HANDOFFS, WORK UNITS, DECISIONS, EVIDENCE FOR <id>, EDGES, SIGNALS, CORRECTIONS or RETRACTIONS";
        let t = self.peek().clone();
        let TokKind::Word(w) = &t.kind else {
            return Err(self.unexpected(EXPECTED));
        };
        match w.to_ascii_uppercase().as_str() {
            "ATTEMPTS" => {
                self.next();
                Ok(ShowTarget::Attempts)
            }
            "FAILED" => {
                self.next();
                self.expect_word("ATTEMPTS")?;
                Ok(ShowTarget::FailedAttempts)
            }
            "SUPERSEDED" => {
                self.next();
                self.expect_word("ATTEMPTS")?;
                Ok(ShowTarget::SupersededAttempts)
            }
            "SESSIONS" => {
                self.next();
                Ok(ShowTarget::Sessions)
            }
            "TURNS" => {
                self.next();
                Ok(ShowTarget::Turns)
            }
            "TOOL" => {
                self.next();
                self.expect_word("CALLS")?;
                Ok(ShowTarget::ToolCalls)
            }
            "TOOL_CALLS" => {
                self.next();
                Ok(ShowTarget::ToolCalls)
            }
            "HANDOFFS" => {
                self.next();
                let between = if self.eat_word("BETWEEN") {
                    let a = self.agent_filter()?;
                    self.expect_word("AND")?;
                    let b = self.agent_filter()?;
                    Some((a, b))
                } else {
                    None
                };
                Ok(ShowTarget::Handoffs { between })
            }
            "WORK" => {
                self.next();
                self.expect_word("UNITS")?;
                Ok(ShowTarget::WorkUnits)
            }
            "WORK_UNITS" => {
                self.next();
                Ok(ShowTarget::WorkUnits)
            }
            "DECISIONS" => {
                self.next();
                Ok(ShowTarget::Decisions)
            }
            "CORRECTIONS" => {
                self.next();
                Ok(ShowTarget::Corrections)
            }
            "RETRACTIONS" => {
                self.next();
                Ok(ShowTarget::Retractions)
            }
            "EVIDENCE" => {
                self.next();
                self.expect_word("FOR")?;
                let subject = self.subject()?;
                Ok(ShowTarget::Evidence(subject))
            }
            "EDGES" => {
                self.next();
                Ok(ShowTarget::Edges)
            }
            "SIGNALS" => {
                self.next();
                Ok(ShowTarget::Signals)
            }
            _ => Err(self.unexpected(EXPECTED)),
        }
    }

    /// `agent = '<value>'` (also `provider = '<value>'`).
    fn agent_filter(&mut self) -> Result<String> {
        if !(self.eat_word("agent") || self.eat_word("provider")) {
            return Err(self.unexpected("agent = '<provider>'"));
        }
        if !self.eat_punct("=") {
            return Err(self.unexpected("'=' after agent"));
        }
        self.value("a provider id such as 'claude_code'")
    }

    fn filter_list(&mut self) -> Result<Vec<Filter>> {
        let mut out = Vec::new();
        loop {
            out.push(self.filter()?);
            if self.eat_word("AND") || self.eat_punct(",") {
                continue;
            }
            break;
        }
        Ok(out)
    }

    fn filter(&mut self) -> Result<Filter> {
        let t = self.peek().clone();
        let TokKind::Word(key) = &t.kind else {
            return Err(self.unexpected(&format!("a filter ({FILTER_KEYS})")));
        };
        let key = key.to_ascii_lowercase();
        self.next();
        match key.as_str() {
            "since" | "until" => {
                self.eat_punct("=");
                let expr = self.time()?;
                Ok(if key == "since" {
                    Filter::Since(expr)
                } else {
                    Filter::Until(expr)
                })
            }
            "project" | "provider" | "agent" | "session" | "turn" | "path" | "outcome" | "tool"
            | "status" | "phase" => {
                if !self.eat_punct("=") {
                    return Err(self.unexpected(&format!("'=' after {key}")));
                }
                let v = self.value(&format!("a value for {key}"))?;
                Ok(match key.as_str() {
                    "project" => Filter::Project(v),
                    "provider" => Filter::Provider(v),
                    "agent" => Filter::Agent(v),
                    "session" => Filter::Session(v),
                    "turn" => Filter::Turn(v),
                    "path" => Filter::Path(v),
                    "outcome" => Filter::Outcome(v),
                    "tool" => Filter::Tool(v),
                    "phase" => Filter::Phase(v),
                    _ => Filter::Status(v),
                })
            }
            _ => Err(QueryError::parse(
                format!("unknown filter '{key}'; expected one of {FILTER_KEYS}"),
                t.start,
            )),
        }
    }

    /// Raw predicate text up to the next top-level clause keyword.
    fn predicate(&mut self) -> Result<String> {
        let start = self.pos;
        let mut depth: i32 = 0;
        loop {
            let t = self.peek();
            match &t.kind {
                TokKind::Eof => break,
                TokKind::Punct("(") | TokKind::Punct("[") => depth += 1,
                TokKind::Punct(")") | TokKind::Punct("]") => depth -= 1,
                TokKind::Punct(";") if depth <= 0 => break,
                TokKind::Word(w)
                    if depth <= 0
                        && ["SINCE", "UNTIL", "ORDER", "LIMIT", "INCLUDING"]
                            .iter()
                            .any(|k| k.eq_ignore_ascii_case(w)) =>
                {
                    break;
                }
                _ => {}
            }
            self.next();
        }
        if self.pos == start {
            return Err(self.unexpected("a predicate after WHERE"));
        }
        let from = self.toks[start].start;
        let to = self.toks[self.pos - 1].end;
        Ok(self.text[from..to].trim().to_string())
    }

    fn order_by(&mut self) -> Result<OrderBy> {
        let t = self.peek().clone();
        let TokKind::Word(w) = &t.kind else {
            return Err(self.unexpected("a column name after ORDER BY"));
        };
        let column = w.trim_matches('"').to_string();
        if column.is_empty()
            || !column
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(QueryError::parse(
                format!("invalid column name '{w}'"),
                t.start,
            ));
        }
        self.next();
        let descending = if self.eat_word("DESC") {
            true
        } else {
            self.eat_word("ASC");
            false
        };
        Ok(OrderBy { column, descending })
    }

    fn subject(&mut self) -> Result<Subject> {
        const EXPECTED: &str = "a subject: project, project '<name>', session '<ses_id>', attempt '<att_id>', turn '<trn_id>', span '<spn_id>', event '<ev_id>', work_unit '<wu_id>', agent '<provider>' or a prefixed id";
        let t = self.peek().clone();
        let TokKind::Word(w) = &t.kind else {
            if let TokKind::Str(s) = &t.kind {
                self.next();
                return Ok(Subject::Id(s.clone()));
            }
            return Err(self.unexpected(EXPECTED));
        };
        match w.to_ascii_lowercase().as_str() {
            "project" => {
                self.next();
                let name = match &self.peek().kind {
                    TokKind::Str(s) => {
                        let s = s.clone();
                        self.next();
                        Some(s)
                    }
                    _ => None,
                };
                Ok(Subject::Project(name))
            }
            "session" => {
                self.next();
                self.value("a session id").map(Subject::Session)
            }
            "attempt" => {
                self.next();
                self.value("an attempt id").map(Subject::Attempt)
            }
            "turn" => {
                self.next();
                self.value("a turn id").map(Subject::Turn)
            }
            "span" | "tool_call" => {
                self.next();
                self.value("a tool call (span) id").map(Subject::Span)
            }
            "event" => {
                self.next();
                self.value("an event id").map(Subject::Event)
            }
            "agent" => {
                self.next();
                self.value("an agent (provider id)").map(Subject::Agent)
            }
            "work_unit" => {
                self.next();
                self.value("a work unit id").map(Subject::WorkUnit)
            }
            "work" if matches!(self.toks.get(self.pos + 1), Some(Token { kind: TokKind::Word(u), .. }) if u.eq_ignore_ascii_case("unit")) =>
            {
                self.next();
                self.next();
                self.value("a work unit id").map(Subject::WorkUnit)
            }
            _ if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w)) => {
                Err(self.unexpected(EXPECTED))
            }
            _ => {
                self.next();
                Ok(Subject::Id(w.clone()))
            }
        }
    }

    fn why(&mut self) -> Result<WhyStatement> {
        self.expect_word("WHY")?;
        let subject = self.subject()?;
        self.eat_word("STATUS");
        let t = self.peek().clone();
        let TokKind::Word(state) = &t.kind else {
            return Err(self.unexpected(
                "a state: BLOCKED (session, project or work unit) or FAILED (attempt)",
            ));
        };
        let state = state.to_ascii_uppercase();
        self.next();
        Ok(WhyStatement { subject, state })
    }

    fn trace(&mut self) -> Result<TraceStatement> {
        self.expect_word("TRACE")?;
        let subject = self.subject()?;
        self.expect_word("CAUSES")?;
        let mut depth = None;
        let mut direction = Direction::Up;
        loop {
            if self.eat_word("DEPTH") {
                depth = Some(self.integer("DEPTH")?);
            } else if self.eat_word("DIRECTION") {
                direction = if self.eat_word("UP") {
                    Direction::Up
                } else if self.eat_word("DOWN") {
                    Direction::Down
                } else if self.eat_word("BOTH") {
                    Direction::Both
                } else {
                    return Err(self.unexpected("UP, DOWN or BOTH"));
                };
            } else {
                break;
            }
        }
        Ok(TraceStatement {
            subject,
            depth,
            direction,
        })
    }

    fn state(&mut self) -> Result<StateStatement> {
        self.expect_word("STATE")?;
        let subject = self.subject()?;
        self.expect_word("AT")?;
        let at = self.time()?;
        if self.is_word("AS") {
            let t = self.peek().clone();
            return Err(QueryError::parse(
                "AS KNOWN AT is not supported yet (planned)",
                t.start,
            ));
        }
        Ok(StateStatement { subject, at })
    }

    fn diff(&mut self) -> Result<DiffStatement> {
        self.expect_word("DIFF")?;
        self.expect_word("STATE")?;
        let subject = if self.is_word("project") {
            self.next();
            // `project 'name' t1 t2` versus `project t1 t2`: a name is only
            // present when three value tokens remain.
            let remaining = self.remaining_values();
            let name = match &self.peek().kind {
                TokKind::Str(s) if remaining >= 3 => {
                    let s = s.clone();
                    self.next();
                    Some(s)
                }
                _ => None,
            };
            Some(Subject::Project(name))
        } else if self.starts_subject() {
            Some(self.subject()?)
        } else {
            None
        };
        let from = self.time()?;
        let to = self.time()?;
        Ok(DiffStatement { subject, from, to })
    }

    /// Whether the next token can begin a subject (`session`, `attempt`,
    /// ... or a bare id) rather than a timestamp keyword.
    fn starts_subject(&self) -> bool {
        match &self.peek().kind {
            TokKind::Word(w) => {
                !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w))
                    && !matches!(
                        w.to_ascii_lowercase().as_str(),
                        "now" | "today" | "yesterday"
                    )
            }
            _ => false,
        }
    }

    /// Number of value-like tokens before the end of the statement.
    fn remaining_values(&self) -> usize {
        self.toks[self.pos..]
            .iter()
            .take_while(|t| !matches!(t.kind, TokKind::Eof | TokKind::Punct(";")))
            .filter(|t| {
                matches!(
                    t.kind,
                    TokKind::Str(_) | TokKind::Word(_) | TokKind::Num(_) | TokKind::Relative(_)
                )
            })
            .count()
    }

    fn what_is(&mut self) -> Result<WhatIsStatement> {
        self.expect_word("WHAT")?;
        self.expect_word("IS")?;
        let subject = self.subject()?;
        self.expect_word("DOING")?;
        self.expect_word("NOW")?;
        Ok(WhatIsStatement { subject })
    }
}

fn quote(s: &str) -> String {
    if s.len() > 40 {
        format!(
            "{}…",
            &s[..s.char_indices().nth(39).map(|(i, _)| i).unwrap_or(s.len())]
        )
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show_variants() {
        let s = parse("show failed attempts for project = 'acme/repo' and since '2026-08-01' where outcome = 'failed' order by started_at desc limit 5;").unwrap();
        let Statement::Show(s) = s else {
            panic!("not a show")
        };
        assert_eq!(s.target, ShowTarget::FailedAttempts);
        assert_eq!(s.filters.len(), 2);
        assert_eq!(s.predicate.as_deref(), Some("outcome = 'failed'"));
        assert_eq!(
            s.order_by,
            Some(OrderBy {
                column: "started_at".into(),
                descending: true
            })
        );
        assert_eq!(s.limit, Some(5));

        let s = parse("SHOW HANDOFFS BETWEEN agent = 'claude_code' AND agent = 'codex'").unwrap();
        assert!(matches!(
            s,
            Statement::Show(ShowStatement {
                target: ShowTarget::Handoffs { between: Some(_) },
                ..
            })
        ));
        let s = parse("SHOW TOOL CALLS FOR session = ses_0191c2a3").unwrap();
        assert!(matches!(
            s,
            Statement::Show(ShowStatement {
                target: ShowTarget::ToolCalls,
                ..
            })
        ));
        let s = parse("SHOW EVIDENCE FOR att_0191c2a3-1b2c-7d3e-8f4a-5b6c7d8e9f00").unwrap();
        assert!(matches!(
            s,
            Statement::Show(ShowStatement {
                target: ShowTarget::Evidence(Subject::Id(_)),
                ..
            })
        ));
    }

    #[test]
    fn parses_why_trace_state_diff_what() {
        assert_eq!(
            parse("WHY session 'ses_abc123' STATUS BLOCKED").unwrap(),
            Statement::Why(WhyStatement {
                subject: Subject::Session("ses_abc123".into()),
                state: "BLOCKED".into()
            })
        );
        assert_eq!(
            parse("why att_abc123 failed").unwrap(),
            Statement::Why(WhyStatement {
                subject: Subject::Id("att_abc123".into()),
                state: "FAILED".into()
            })
        );
        assert_eq!(
            parse("TRACE att_abc123 CAUSES DEPTH 3 DIRECTION BOTH").unwrap(),
            Statement::Trace(TraceStatement {
                subject: Subject::Id("att_abc123".into()),
                depth: Some(3),
                direction: Direction::Both
            })
        );
        assert_eq!(
            parse("STATE project AT '2026-08-28T08:00:00Z'").unwrap(),
            Statement::State(StateStatement {
                subject: Subject::Project(None),
                at: TimeExpr::Absolute(
                    attemptdb_core::Timestamp::parse("2026-08-28T08:00:00Z").unwrap()
                )
            })
        );
        assert_eq!(
            parse("STATE session 'ses_abc123' AT -15m").unwrap(),
            Statement::State(StateStatement {
                subject: Subject::Session("ses_abc123".into()),
                at: TimeExpr::Relative {
                    amount: 15,
                    unit: 'm'
                }
            })
        );
        assert_eq!(
            parse("DIFF STATE '2026-08-28' '2026-08-29'").unwrap(),
            Statement::Diff(DiffStatement {
                subject: None,
                from: TimeExpr::Absolute(attemptdb_core::Timestamp::parse("2026-08-28").unwrap()),
                to: TimeExpr::Absolute(attemptdb_core::Timestamp::parse("2026-08-29").unwrap()),
            })
        );
        assert!(matches!(
            parse("DIFF STATE project 'acme' yesterday now").unwrap(),
            Statement::Diff(DiffStatement {
                subject: Some(Subject::Project(Some(_))),
                ..
            })
        ));
        assert!(matches!(
            parse("DIFF STATE project yesterday now").unwrap(),
            Statement::Diff(DiffStatement {
                subject: Some(Subject::Project(None)),
                ..
            })
        ));
        assert_eq!(
            parse("WHAT IS project DOING NOW").unwrap(),
            Statement::WhatIs(WhatIsStatement {
                subject: Subject::Project(None)
            })
        );
        assert!(matches!(
            parse("EXPLAIN SHOW ATTEMPTS").unwrap(),
            Statement::Explain(_)
        ));
    }

    #[test]
    fn parses_work_units_retractions_and_including_retracted() {
        let s = parse("SHOW WORK UNITS FOR phase = 'blocked' AND status = open LIMIT 3").unwrap();
        let Statement::Show(s) = s else {
            panic!("not a show")
        };
        assert_eq!(s.target, ShowTarget::WorkUnits);
        assert_eq!(
            s.filters,
            vec![
                Filter::Phase("blocked".into()),
                Filter::Status("open".into())
            ]
        );
        assert!(!s.including_retracted);
        let s = parse("SHOW SESSIONS INCLUDING RETRACTED ORDER BY started_at").unwrap();
        let Statement::Show(s) = s else {
            panic!("not a show")
        };
        assert_eq!(s.target, ShowTarget::Sessions);
        assert!(s.including_retracted);
        assert!(s.order_by.is_some());
        let s = parse("SHOW ATTEMPTS WHERE outcome = 'failed' INCLUDING RETRACTED").unwrap();
        let Statement::Show(s) = s else {
            panic!("not a show")
        };
        assert_eq!(s.predicate.as_deref(), Some("outcome = 'failed'"));
        assert!(s.including_retracted);
        for (text, target) in [
            ("SHOW WORK_UNITS", ShowTarget::WorkUnits),
            ("SHOW DECISIONS", ShowTarget::Decisions),
            ("SHOW CORRECTIONS", ShowTarget::Corrections),
            ("SHOW RETRACTIONS", ShowTarget::Retractions),
        ] {
            let Statement::Show(s) = parse(text).unwrap() else {
                panic!("{text}")
            };
            assert_eq!(s.target, target, "{text}");
        }
        assert_eq!(
            parse("WHY work_unit 'wu_abc123' STATUS BLOCKED").unwrap(),
            Statement::Why(WhyStatement {
                subject: Subject::WorkUnit("wu_abc123".into()),
                state: "BLOCKED".into()
            })
        );
        assert_eq!(
            parse("WHY work unit wu_abc123 BLOCKED").unwrap(),
            Statement::Why(WhyStatement {
                subject: Subject::WorkUnit("wu_abc123".into()),
                state: "BLOCKED".into()
            })
        );
        assert!(matches!(
            parse("SHOW EVIDENCE FOR wu_abc123").unwrap(),
            Statement::Show(ShowStatement {
                target: ShowTarget::Evidence(Subject::Id(_)),
                ..
            })
        ));
        assert!(matches!(
            parse("SHOW SESSIONS INCLUDING"),
            Err(QueryError::Parse { .. })
        ));
        assert!(matches!(parse("SHOW WORK"), Err(QueryError::Parse { .. })));
    }

    #[test]
    fn errors_have_positions_and_expectations() {
        let err = parse("SHOW FOO").unwrap_err();
        match err {
            QueryError::Parse { message, position } => {
                assert_eq!(position, 5);
                assert!(message.contains("'FOO'"), "{message}");
                assert!(message.contains("ATTEMPTS"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        let err = parse("WHAT IS project 'attemptdb' NOW").unwrap_err();
        match err {
            QueryError::Parse { message, position } => {
                assert_eq!(position, 28);
                assert!(message.contains("DOING"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse(""),
            Err(QueryError::Parse { position: 0, .. })
        ));
        assert!(matches!(
            parse("SHOW ATTEMPTS LIMIT x"),
            Err(QueryError::Parse { .. })
        ));
        assert!(matches!(
            parse("STATE project AT 'soon'"),
            Err(QueryError::Parse { .. })
        ));
        assert!(matches!(
            parse("SHOW ATTEMPTS; SHOW SESSIONS"),
            Err(QueryError::Parse { .. })
        ));
    }
}
