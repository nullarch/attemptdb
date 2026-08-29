# Show HN launch draft

> **Do not submit yet.** A Show HN must link to something people can run without
> signing up. Replace every placeholder and verify every claim before launch.

## Recommended title

```text
Show HN: AttemptDB – a database for what AI coding agents tried
```

## Strong alternative titles

```text
Show HN: AttemptDB – Git stores changes; this stores agent attempts
```

```text
Show HN: AttemptDB – a temporal database containing its own build history
```

```text
Show HN: We turned 1.45M coding-agent events into a temporal database
```

Use the first title for clarity. Use the third only when the self-hosting demo
is real and immediately accessible. Use the fourth when the benchmark and the
relationship between the production event stream and the OSS system are fully
documented.

## Recommended first comment

```text
Hi HN — I built VibeMon, a small tool that watches coding-agent activity across
Claude Code, Codex, Cursor, and Gemini. After handling more than 1.45 million
metadata-only events, I kept running into a limitation that traces and Git did
not solve.

Git can tell me what changed. It cannot tell me what an agent tried before the
change, which approaches failed, why it changed direction, or how work moved
between agents.

AttemptDB is my attempt to model that missing history as a database rather than
another dashboard. It stores observed events as an append-only log, then builds
versioned Attempts, WorkUnits, Decisions, Artifacts, and causal links on top.
Inferred state is kept separate from facts, and every answer can point back to
its evidence.

The project is local-first and works without an account. Raw prompts and tool
content stay local by default. The hosted VibeMon service is optional.

The demo is intentionally recursive: AttemptDB contains the history of the
agents that built AttemptDB, including failed and superseded approaches. You
can inspect that history locally and run queries such as:

  WHY work_unit_821 STATUS BLOCKED
  SHOW FAILED ATTEMPTS FOR project = 'attemptdb'
  STATE project AT '2026-08-20T14:00:00Z'

Quick start:

  <one command that actually works>

I would especially like feedback on two things: whether Attempt/WorkUnit is the
right abstraction, and where the line should be between deterministic event
projection and model-based inference.
```

## Shorter first-comment variant

```text
Hi HN — Git stores what changed. I wanted a record of what coding agents tried:
failed approaches, decisions, handoffs, blockers, and the evidence behind the
final result.

AttemptDB is a local-first temporal database for that history. Raw events are
append-only; tasks and status are versioned inferences with links back to their
evidence. It supports Claude Code, Codex, Cursor, and Gemini.

The demo contains AttemptDB's own build history, captured from the agents that
built it. No signup is required:

  <one command that actually works>

I am looking for blunt feedback on the data model and whether the queries are
actually more useful than ordinary traces plus Git.
```

## README hero at launch

```text
AttemptDB
The database for what agents tried.

Git records what changed. AttemptDB records the attempts, decisions, failures,
artifacts, and causal history that explain how agent work reached its result.
```

## Demo sequence

The linked page or terminal recording should prove the thesis in under one
minute:

1. Open the current state of the AttemptDB project.
2. Run `attempt why <blocked-work-unit>`.
3. Follow its evidence to a failed tool/configuration attempt.
4. Open the superseding attempt that fixed it.
5. Time-travel to the project state before the fix.
6. Show that the history came from the agents that built the repository.
7. End with the one-command local install, not a signup form.

## Claims that need evidence before posting

- [ ] The one-command install works on a clean machine.
- [ ] The linked demo requires no account or email.
- [ ] The self-hosted build history is real, not seeded by hand.
- [ ] Every displayed inference links to source events.
- [ ] Privacy defaults match the README exactly.
- [ ] Provider support is verified with real payload fixtures.
- [ ] The 1.45M-event aggregate can be shared publicly.
- [ ] The repository has a license, reproducible tests, and tagged release.
- [ ] A benchmark or replay test demonstrates non-trivial behavior.
- [ ] VibeMon attribution distinguishes HN, GitHub, CLI, and sync activation.

## Reply style on HN

- Answer in the founder's natural voice.
- Be direct about SQLite or any other underlying storage.
- Admit current limitations before commenters discover them.
- Do not paste generated essays into replies.
- Do not ask anyone to upvote or seed supportive comments.
- When someone says “this is just traces plus SQLite,” show one concrete query
  and its evidence path rather than arguing from terminology.
