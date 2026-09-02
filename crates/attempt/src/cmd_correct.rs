//! `attempt correct` and `attempt retract`: write `Correction` /
//! `Retraction` events (RFC 0003 §8) into the live database and report what
//! the projection changes.
//!
//! Both commands are facts about the log, written by AttemptDB itself:
//! `provider = "attemptdb"`, `provider_event_name = "Correction" |
//! "Retraction"`, landed in the target's session (the canonical session id
//! is overridden so the event is scoped with the work it describes). Free
//! text (`--note`) is content and is dropped at ingest in `metadata_only`
//! mode; only `note_chars` survives. Before writing, the command re-projects
//! the stream with and without the new event and prints the difference, so
//! `--dry-run` is an honest preview.
//!
//! Wiring (not done here): add `Correct(crate::cmd_correct::CorrectArgs)` and
//! `Retract(crate::cmd_correct::RetractArgs)` to `cli::Command`, and
//! `Command::Correct(a) => cmd_correct::correct(&cli, a)` /
//! `Command::Retract(a) => cmd_correct::retract(&cli, a)` to `main`.

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::render::{print_json, truncate};
use anyhow::{Context, Result, bail};
use attemptdb_capture::ingest;
use attemptdb_core::event::{EventContent, Provider};
use attemptdb_core::{AgentId, AttemptId, Event, EventId, EventKind, SessionId, TurnId};
use attemptdb_project::{
    Attempt, CORRECTABLE_OUTCOMES, CorrectionType, Projection, RetractionReason,
    RetractionTargetType, Turn, WorkUnit, project,
};
use attemptdb_storage::{Database, ScanFilter};
use clap::Args;
use serde_json::{Map, Value};
use std::process::ExitCode;

#[derive(Args, Debug)]
pub struct CorrectArgs {
    /// What to correct: an `att_` (attempt) or `trn_` (turn) id, full or a unique prefix.
    pub target: String,
    /// New outcome for an attempt: succeeded, failed, abandoned, superseded.
    #[arg(long, value_name = "OUTCOME")]
    pub outcome: Option<String>,
    /// Content-free failure class to record with the outcome (e.g. wrong_fix).
    #[arg(long, value_name = "CLASS")]
    pub failure_class: Option<String>,
    /// Free-text note. For an attempt it is attached as a note; for a turn it becomes the objective.
    /// Content: dropped at ingest in metadata_only mode (only its length is kept).
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
    /// Show the projected change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct RetractArgs {
    /// Retract a whole session (`ses_` id, provider session id, or unique prefix).
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
    /// Retract one attempt (`att_` id or unique prefix); its tool calls go with it.
    #[arg(long, value_name = "ID")]
    pub attempt: Option<String>,
    /// Retract one event (`ev_` id or unique prefix).
    #[arg(long, value_name = "ID")]
    pub event: Option<String>,
    /// Why: benchmark, test, duplicate, mistaken_import, privacy, other.
    #[arg(long, value_name = "REASON")]
    pub reason: String,
    /// Free-text note (content; dropped at ingest in metadata_only mode).
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
    /// Do not ask for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Show the projected change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// One projected value that the new event changes.
#[derive(Debug, serde::Serialize)]
struct Change {
    entity: String,
    field: String,
    before: String,
    after: String,
}

enum CorrectionTarget<'a> {
    Attempt(&'a Attempt),
    Turn(&'a Turn),
}

fn open_writer(cli: &Cli, ctx: &Ctx) -> Result<Database> {
    if cli.snapshot.is_some() {
        bail!(
            "corrections and retractions are written to the live database; `--snapshot` is read-only"
        );
    }
    if !Database::exists(&ctx.locator.db_dir) {
        bail!(
            "no database at {}\n  run `attempt init` first (or `attempt init --local` for a project-local database)",
            ctx.locator.db_dir.display()
        );
    }
    // Reads go through whichever handle we can get (the daemon may hold the
    // writer lock); the actual write goes through `ingest::write_events`.
    let (db, _import, _read_only) = ingest::open_fresh(&ctx.locator, false)
        .with_context(|| format!("opening {}", ctx.locator.db_dir.display()))?;
    Ok(db)
}

/// Resolve an id given in full, with or without its prefix, or as a hex
/// prefix of at least four characters that matches exactly one candidate.
fn resolve_id<T>(spec: &str, prefix: &str, what: &str, candidates: &[T]) -> Result<T>
where
    T: Copy + std::fmt::Display,
{
    let raw = spec.trim();
    let rest = raw.strip_prefix(prefix).unwrap_or(raw);
    let needle: String = rest
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if needle.len() < 4 || !needle.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{what} id {spec:?} must be {prefix}<uuid> or at least four hex digits of it");
    }
    let hex = |id: &T| id.to_string().replace('-', "");
    let matches: Vec<T> = candidates
        .iter()
        .filter(|c| hex(c).starts_with(&needle))
        .copied()
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => bail!("no {what} matches {spec:?}"),
        n => bail!(
            "ambiguous {what} id {spec:?}: {n} candidates ({} ...)",
            matches
                .iter()
                .take(3)
                .map(|m| format!("{prefix}{m}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Build an AttemptDB-authored event in the session of `template`.
fn attemptdb_event(
    db: &Database,
    ctx: &Ctx,
    template: &Event,
    kind: EventKind,
    name: &str,
    note: Option<&str>,
) -> Event {
    let mut ev = Event::new(
        db.device_id(),
        Provider::Other("attemptdb".into()),
        name,
        kind,
        template.project.clone(),
        template.provider_session_id.clone(),
        ctx.config.capture_mode,
        env!("CARGO_PKG_VERSION"),
    );
    ev.session_id = template.session_id;
    ev.agent.agent_id = AgentId::derive(&["session", &template.session_id.to_string()]);
    if let Some(n) = note {
        ev.attrs
            .insert("note_chars".into(), Value::from(n.chars().count() as u64));
        let mut extra = Map::new();
        extra.insert("note".into(), Value::from(n));
        ev.content = Some(EventContent {
            extra,
            ..Default::default()
        });
    }
    // Preview exactly what ingest will keep.
    ev.apply_capture_mode();
    ev
}

fn template_event(events: &[Event], session_id: SessionId) -> Result<Event> {
    events
        .iter()
        .find(|e| e.session_id == session_id)
        .cloned()
        .with_context(|| format!("no event of session ses_{session_id} is loaded"))
}

fn fmt_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "—".into())
}

fn unit_of(p: &Projection, attempt: AttemptId) -> Option<&WorkUnit> {
    p.work_unit_of_attempt(attempt)
}

fn push_change(
    changes: &mut Vec<Change>,
    entity: &str,
    field: &str,
    before: String,
    after: String,
) {
    if before != after {
        changes.push(Change {
            entity: entity.to_string(),
            field: field.to_string(),
            before,
            after,
        });
    }
}

fn attempt_changes(
    before: &Projection,
    after: &Projection,
    id: AttemptId,
    changes: &mut Vec<Change>,
) {
    let entity = format!("attempt att_{id}");
    let b = before.attempts.iter().find(|a| a.attempt_id == id);
    let a = after.attempts.iter().find(|a| a.attempt_id == id);
    match (b, a) {
        (Some(b), Some(a)) => {
            push_change(
                changes,
                &entity,
                "outcome",
                b.outcome.as_str().into(),
                a.outcome.as_str().into(),
            );
            push_change(
                changes,
                &entity,
                "failure_class",
                fmt_opt(b.failure_class.as_deref()),
                fmt_opt(a.failure_class.as_deref()),
            );
            push_change(
                changes,
                &entity,
                "objective",
                fmt_opt(b.objective.as_deref().map(|o| truncate(o, 60))),
                fmt_opt(a.objective.as_deref().map(|o| truncate(o, 60))),
            );
            push_change(
                changes,
                &entity,
                "note",
                fmt_opt(b.note.as_deref().map(|o| truncate(o, 60))),
                fmt_opt(a.note.as_deref().map(|o| truncate(o, 60))),
            );
            push_change(
                changes,
                &entity,
                "corrected_by",
                fmt_opt(b.corrected.map(|c| format!("ev_{}", c.event_id))),
                fmt_opt(a.corrected.map(|c| format!("ev_{}", c.event_id))),
            );
        }
        (Some(_), None) => push_change(
            changes,
            &entity,
            "presence",
            "projected".into(),
            "retracted".into(),
        ),
        _ => {}
    }
    let (ub, ua) = (unit_of(before, id), unit_of(after, id));
    if let (Some(ub), Some(ua)) = (ub, ua) {
        let entity = format!("work unit wu_{}", ua.work_unit_id);
        push_change(
            changes,
            &entity,
            "phase",
            ub.phase.as_str().into(),
            ua.phase.as_str().into(),
        );
        push_change(
            changes,
            &entity,
            "status",
            ub.status.as_str().into(),
            ua.status.as_str().into(),
        );
        push_change(
            changes,
            &entity,
            "failed_attempts",
            ub.failure_count.to_string(),
            ua.failure_count.to_string(),
        );
        push_change(
            changes,
            &entity,
            "attempts",
            ub.attempts.len().to_string(),
            ua.attempts.len().to_string(),
        );
    }
}

fn count_changes(before: &Projection, after: &Projection, changes: &mut Vec<Change>) {
    let counts = |p: &Projection| {
        [
            ("sessions", p.sessions.len()),
            ("turns", p.turns.len()),
            ("tool calls", p.tool_calls.len()),
            ("attempts", p.attempts.len()),
            ("handoffs", p.handoffs.len()),
            ("work units", p.work_units.len()),
            ("decisions", p.decisions.len()),
            ("signals", p.signals.len()),
        ]
    };
    for ((name, b), (_, a)) in counts(before).into_iter().zip(counts(after)) {
        push_change(changes, "projection", name, b.to_string(), a.to_string());
    }
    push_change(
        changes,
        "projection",
        "retracted events",
        before.stats.retracted_events.to_string(),
        after.stats.retracted_events.to_string(),
    );
}

fn print_changes(changes: &[Change]) {
    if changes.is_empty() {
        println!("projection unchanged (the target may already carry this value)");
        return;
    }
    println!("projection changes:");
    for c in changes {
        println!(
            "  {:<44} {:<18} {} → {}",
            truncate(&c.entity, 44),
            c.field,
            truncate(&c.before, 40),
            truncate(&c.after, 40)
        );
    }
}

fn write(ctx: &Ctx, ev: Event, dry_run: bool) -> Result<Option<EventId>> {
    if dry_run {
        return Ok(None);
    }
    let id = ev.event_id;
    let report = ingest::write_events(&ctx.locator, vec![ev])?;
    if report.accepted != 1 {
        bail!(
            "the event was not accepted (duplicates: {})",
            report.duplicates
        );
    }
    Ok(Some(id))
}

pub fn correct(cli: &Cli, args: &CorrectArgs) -> Result<ExitCode> {
    let ctx = Ctx::new(cli)?;
    let db = open_writer(cli, &ctx)?;
    let events = db.scan(&ScanFilter::default())?;
    let before = project(&events);

    let spec = args.target.trim();
    let target = if spec.starts_with("trn_") {
        let ids: Vec<TurnId> = before.turns.iter().map(|t| t.turn_id).collect();
        let id = resolve_id(spec, "trn_", "turn", &ids)?;
        CorrectionTarget::Turn(
            before
                .turns
                .iter()
                .find(|t| t.turn_id == id)
                .expect("resolved"),
        )
    } else if spec.starts_with("att_") {
        let ids: Vec<AttemptId> = before.attempts.iter().map(|a| a.attempt_id).collect();
        let id = resolve_id(spec, "att_", "attempt", &ids)?;
        CorrectionTarget::Attempt(
            before
                .attempts
                .iter()
                .find(|a| a.attempt_id == id)
                .expect("resolved"),
        )
    } else {
        bail!(
            "target must be an attempt (att_…) or a turn (trn_…) id; `attempt timeline` lists them"
        );
    };

    let (correction_type, session_id, target_text) = match &target {
        CorrectionTarget::Attempt(a) => {
            if args.outcome.is_none() && args.note.is_none() {
                bail!(
                    "give --outcome (succeeded|failed|abandoned|superseded) and/or --note for an attempt"
                );
            }
            if args.failure_class.is_some() && args.outcome.is_none() {
                bail!("--failure-class needs --outcome");
            }
            let ty = if args.outcome.is_some() {
                CorrectionType::AttemptOutcome
            } else {
                CorrectionType::AttemptNote
            };
            (ty, a.session_id, format!("att_{}", a.attempt_id))
        }
        CorrectionTarget::Turn(t) => {
            if args.outcome.is_some() || args.failure_class.is_some() {
                bail!(
                    "--outcome and --failure-class apply to attempts; a turn takes --note as its new objective"
                );
            }
            if args.note.is_none() {
                bail!("give --note <text> as the turn's new objective");
            }
            (
                CorrectionType::TurnObjective,
                t.session_id,
                format!("trn_{}", t.turn_id),
            )
        }
    };
    if let Some(o) = &args.outcome {
        let o = o.trim().to_ascii_lowercase();
        if !CORRECTABLE_OUTCOMES.contains(&o.as_str()) {
            bail!(
                "unknown outcome {:?}; expected one of {}",
                args.outcome.as_deref().unwrap_or_default(),
                CORRECTABLE_OUTCOMES.join(", ")
            );
        }
    }
    if correction_type == CorrectionType::TurnObjective
        && !ctx.config.capture_mode.persists_content_locally()
    {
        eprintln!(
            "warning: capture mode is {}; the objective text is content and will not be stored, only its length",
            ctx.config.capture_mode
        );
    }

    let template = template_event(&events, session_id)?;
    let mut ev = attemptdb_event(
        &db,
        &ctx,
        &template,
        EventKind::Correction,
        "Correction",
        args.note.as_deref(),
    );
    ev.attrs.insert(
        "correction_type".into(),
        Value::from(correction_type.as_str()),
    );
    ev.attrs
        .insert("target".into(), Value::from(target_text.as_str()));
    if let Some(o) = &args.outcome {
        ev.attrs
            .insert("outcome".into(), Value::from(o.trim().to_ascii_lowercase()));
    }
    if let Some(c) = &args.failure_class {
        ev.attrs
            .insert("failure_class".into(), Value::from(c.as_str()));
    }

    let mut with = events.clone();
    with.push(ev.clone());
    let after = project(&with);
    let mut changes = Vec::new();
    match &target {
        CorrectionTarget::Attempt(a) => {
            attempt_changes(&before, &after, a.attempt_id, &mut changes)
        }
        CorrectionTarget::Turn(t) => {
            let entity = format!("turn trn_{}", t.turn_id);
            if let Some(after_turn) = after.turns.iter().find(|x| x.turn_id == t.turn_id) {
                push_change(
                    &mut changes,
                    &entity,
                    "objective",
                    fmt_opt(t.objective.as_deref().map(|o| truncate(o, 60))),
                    fmt_opt(after_turn.objective.as_deref().map(|o| truncate(o, 60))),
                );
            }
            for a in before.attempts.iter().filter(|a| a.turn_id == t.turn_id) {
                attempt_changes(&before, &after, a.attempt_id, &mut changes);
            }
        }
    }
    let status = after
        .corrections
        .iter()
        .find(|c| c.event_id == ev.event_id)
        .map(|c| c.status.as_str())
        .unwrap_or("unknown");
    if status != "applied" {
        bail!("the projection would not apply this correction (status: {status})");
    }

    let written = write(&ctx, ev.clone(), args.dry_run)?;
    if cli.json {
        print_json(&serde_json::json!({
            "event_id": format!("ev_{}", ev.event_id),
            "kind": "correction",
            "correction_type": correction_type.as_str(),
            "target": target_text,
            "outcome": args.outcome,
            "failure_class": args.failure_class,
            "note_chars": ev.attrs.get("note_chars"),
            "note_stored": ev.content.is_some(),
            "status": status,
            "written": written.is_some(),
            "dry_run": args.dry_run,
            "changes": changes,
        }));
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{} correction ev_{} ({} on {})",
        if args.dry_run { "would write" } else { "wrote" },
        ev.event_id,
        correction_type.as_str(),
        target_text
    );
    if args.note.is_some() && ev.content.is_none() {
        println!(
            "note text not stored (capture mode {}); note_chars kept",
            ctx.config.capture_mode
        );
    }
    print_changes(&changes);
    if args.dry_run {
        println!("(dry run — nothing was written)");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn retract(cli: &Cli, args: &RetractArgs) -> Result<ExitCode> {
    let given = [
        args.session.is_some(),
        args.attempt.is_some(),
        args.event.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if given != 1 {
        bail!("give exactly one of --session, --attempt, --event");
    }
    let reason = args.reason.trim().to_ascii_lowercase().replace('-', "_");
    if !RetractionReason::ALL.iter().any(|r| r.as_str() == reason) {
        bail!(
            "unknown reason {:?}; expected one of {}",
            args.reason,
            RetractionReason::ALL
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let ctx = Ctx::new(cli)?;
    let db = open_writer(cli, &ctx)?;
    let events = db.scan(&ScanFilter::default())?;
    let before = project(&events);

    let (target_type, target_text, session_id, description) = if let Some(spec) = &args.session {
        let id = crate::ctx::resolve_session(
            &attemptdb_query::StreamFacts::from_events(events.iter()),
            spec,
        )?;
        if before.retracted_ids.contains_session(&id) {
            bail!("session ses_{id} is already retracted");
        }
        let s = before
            .session(id)
            .with_context(|| format!("session ses_{id} is not projected (only meta events?)"))?;
        (
            RetractionTargetType::Session,
            format!("ses_{id}"),
            id,
            format!(
                "session ses_{} ({}, {}, {} events, {} turns, {} attempts, {} → {})",
                id.short(),
                s.provider.display_name(),
                truncate(&s.project_name, 40),
                s.event_count,
                s.turn_count,
                before.attempts_of(id).count(),
                crate::render::ts_local(s.started_at),
                s.ended_at
                    .map(crate::render::ts_local)
                    .unwrap_or_else(|| "open".into())
            ),
        )
    } else if let Some(spec) = &args.attempt {
        let ids: Vec<AttemptId> = before.attempts.iter().map(|a| a.attempt_id).collect();
        let id = resolve_id(spec, "att_", "attempt", &ids)?;
        let a = before
            .attempts
            .iter()
            .find(|a| a.attempt_id == id)
            .expect("resolved");
        (
            RetractionTargetType::Attempt,
            format!("att_{id}"),
            a.session_id,
            format!(
                "attempt att_{} (turn {} #{}, {}, {}, {} tool calls)",
                id.short(),
                a.turn_index,
                a.index,
                a.outcome.as_str(),
                truncate(&a.approach, 50),
                a.tool_call_ids.len()
            ),
        )
    } else {
        let spec = args.event.as_deref().unwrap_or_default();
        let ids: Vec<EventId> = events.iter().map(|e| e.event_id).collect();
        let id = resolve_id(spec, "ev_", "event", &ids)?;
        let e = events.iter().find(|e| e.event_id == id).expect("resolved");
        if attemptdb_project::is_meta_kind(e.kind) {
            bail!(
                "ev_{id} is a {} event; corrections and retractions cannot be retracted",
                e.kind
            );
        }
        if before.retracted_ids.contains_event(&id) {
            bail!("event ev_{id} is already retracted");
        }
        (
            RetractionTargetType::Event,
            format!("ev_{id}"),
            e.session_id,
            format!(
                "event ev_{} ({} {} {} at {})",
                id.short(),
                e.provider.as_str(),
                e.kind,
                e.tool.as_ref().map(|t| t.name.as_str()).unwrap_or(""),
                crate::render::ts_local(e.observed_at)
            ),
        )
    };

    let template = template_event(&events, session_id)?;
    let mut ev = attemptdb_event(
        &db,
        &ctx,
        &template,
        EventKind::Retraction,
        "Retraction",
        args.note.as_deref(),
    );
    ev.attrs
        .insert("target_type".into(), Value::from(target_type.as_str()));
    ev.attrs
        .insert("target".into(), Value::from(target_text.as_str()));
    ev.attrs
        .insert("reason".into(), Value::from(reason.as_str()));

    let mut with = events.clone();
    with.push(ev.clone());
    let after = project(&with);
    let mut changes = Vec::new();
    count_changes(&before, &after, &mut changes);
    if let Some(spec) = &args.attempt
        && let Ok(id) = resolve_id(
            spec,
            "att_",
            "attempt",
            &before
                .attempts
                .iter()
                .map(|a| a.attempt_id)
                .collect::<Vec<_>>(),
        )
    {
        attempt_changes(&before, &after, id, &mut changes);
    }
    let matched = after
        .retractions
        .iter()
        .find(|r| r.event_id == ev.event_id)
        .map(|r| (r.matched, r.retracted_events))
        .unwrap_or((false, 0));

    if cli.json {
        let written = if args.yes || args.dry_run {
            write(&ctx, ev.clone(), args.dry_run)?
        } else {
            bail!(
                "refusing to retract without confirmation; pass --yes (or --dry-run) with --json"
            );
        };
        print_json(&serde_json::json!({
            "event_id": format!("ev_{}", ev.event_id),
            "kind": "retraction",
            "target_type": target_type.as_str(),
            "target": target_text,
            "reason": reason,
            "note_stored": ev.content.is_some(),
            "matched": matched.0,
            "retracted_events": matched.1,
            "written": written.is_some(),
            "dry_run": args.dry_run,
            "changes": changes,
        }));
        return Ok(ExitCode::SUCCESS);
    }

    println!("retract {description}");
    println!("reason  {reason}");
    println!(
        "effect  {} fact event(s) leave every projection and the sanitized export; the events stay in the log",
        matched.1
    );
    print_changes(&changes);
    if args.dry_run {
        println!("(dry run — nothing was written)");
        return Ok(ExitCode::SUCCESS);
    }
    if !args.yes {
        use std::io::{IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            bail!(
                "refusing to retract without confirmation; pass --yes to confirm non-interactively"
            );
        }
        print!("type 'retract' to confirm: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if line.trim() != "retract" {
            println!("aborted; nothing was written");
            return Ok(ExitCode::from(1));
        }
    }
    let written = write(&ctx, ev.clone(), false)?;
    println!(
        "wrote retraction ev_{} ({} {})",
        written.unwrap_or(ev.event_id),
        target_type.as_str(),
        target_text
    );
    println!(
        "undo is not possible: retractions are facts too; a wrong one can only be documented with `attempt correct`"
    );
    Ok(ExitCode::SUCCESS)
}
