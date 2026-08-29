//! AttemptQL abstract syntax.

pub use crate::graph::Direction;
pub use crate::timeexpr::TimeExpr;

/// One parsed AttemptQL statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Show(ShowStatement),
    Why(WhyStatement),
    Trace(TraceStatement),
    State(StateStatement),
    Diff(DiffStatement),
    WhatIs(WhatIsStatement),
    /// `EXPLAIN <statement>`.
    Explain(Box<Statement>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShowTarget {
    Attempts,
    FailedAttempts,
    SupersededAttempts,
    Sessions,
    Turns,
    ToolCalls,
    Handoffs {
        /// `BETWEEN agent = 'a' AND agent = 'b'`, matched in either order.
        between: Option<(String, String)>,
    },
    Decisions,
    /// `EVIDENCE FOR <subject>`.
    Evidence(Subject),
    Edges,
    Signals,
}

impl ShowTarget {
    pub fn name(&self) -> &'static str {
        match self {
            ShowTarget::Attempts => "attempts",
            ShowTarget::FailedAttempts => "failed attempts",
            ShowTarget::SupersededAttempts => "superseded attempts",
            ShowTarget::Sessions => "sessions",
            ShowTarget::Turns => "turns",
            ShowTarget::ToolCalls => "tool calls",
            ShowTarget::Handoffs { .. } => "handoffs",
            ShowTarget::Decisions => "decisions",
            ShowTarget::Evidence(_) => "evidence",
            ShowTarget::Edges => "edges",
            ShowTarget::Signals => "signals",
        }
    }
}

/// A `FOR key = value` filter.
#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    /// Project name or `prj_` id.
    Project(String),
    /// `ses_` id (short form allowed).
    Session(String),
    /// Provider id (`claude_code`, `codex`, ...), display names accepted.
    Provider(String),
    /// Provider id or `agt_` id.
    Agent(String),
    /// `trn_` id.
    Turn(String),
    /// Repository-relative path; `*` / `%` wildcards allowed.
    Path(String),
    Outcome(String),
    Tool(String),
    Status(String),
    Since(TimeExpr),
    Until(TimeExpr),
}

impl Filter {
    pub fn key(&self) -> &'static str {
        match self {
            Filter::Project(_) => "project",
            Filter::Session(_) => "session",
            Filter::Provider(_) => "provider",
            Filter::Agent(_) => "agent",
            Filter::Turn(_) => "turn",
            Filter::Path(_) => "path",
            Filter::Outcome(_) => "outcome",
            Filter::Tool(_) => "tool",
            Filter::Status(_) => "status",
            Filter::Since(_) => "since",
            Filter::Until(_) => "until",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShowStatement {
    pub target: ShowTarget,
    pub filters: Vec<Filter>,
    /// Raw SQL boolean expression from `WHERE`.
    pub predicate: Option<String>,
    pub since: Option<TimeExpr>,
    pub until: Option<TimeExpr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}

/// What a `WHY` / `TRACE` / `STATE` / `WHAT IS` statement is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// `project` (all loaded sessions) or `project '<name or prj_ id>'`.
    Project(Option<String>),
    Session(String),
    Attempt(String),
    Turn(String),
    Span(String),
    Event(String),
    Agent(String),
    /// A bare id whose type is decided by its prefix.
    Id(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhyStatement {
    pub subject: Subject,
    /// Upper-cased state name (`BLOCKED`, `FAILED`).
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceStatement {
    pub subject: Subject,
    pub depth: Option<usize>,
    pub direction: Direction,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateStatement {
    pub subject: Subject,
    pub at: TimeExpr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffStatement {
    pub subject: Option<Subject>,
    pub from: TimeExpr,
    pub to: TimeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhatIsStatement {
    pub subject: Subject,
}
