//! The shareable summary card: one sanitized SVG showing what the agents
//! tried, sized for a README, an issue or a social preview (1200×630).
//!
//! The card is an *image*, so it is sanitized by construction rather than by
//! flag: it never prints prompt, command or tool-output text, never prints a
//! path that is not repository-relative, and never prints a database path or
//! a home directory. What it does print — outcomes, failure classes,
//! providers, counts and short ids — is the content-free metadata the
//! projection is built from.
//!
//! There is no PNG encoder here on purpose: rasterising text needs a font
//! rasteriser, and the single self-contained binary is worth more than the
//! convenience. `attempt ui export card.svg` writes SVG; converting it is a
//! one-liner with any browser or `rsvg-convert`.

use crate::html::esc;
use attemptdb_project::{Attempt, AttemptOutcome, Projection, WorkUnit};
use std::fmt::Write as _;

pub const WIDTH: f64 = 1200.0;
pub const HEIGHT: f64 = 630.0;

const MARGIN: f64 = 64.0;
const CHIP_H: f64 = 62.0;
const CHIP_GAP: f64 = 34.0;
const ROW_GAP: f64 = 18.0;
/// Rough advance width of the label font at 17 px, used to size chips.
const CHAR_W: f64 = 8.3;

/// What to draw.
#[derive(Clone, Debug)]
pub struct CardOptions {
    /// Project name in the header. `None` prints "a local project".
    pub project: Option<String>,
    /// Human window label, e.g. `since 2026-08-28`.
    pub window: Option<String>,
    /// Append the "Built with AttemptDB" line.
    pub attribution: bool,
}

impl Default for CardOptions {
    fn default() -> Self {
        Self {
            project: None,
            window: None,
            attribution: true,
        }
    }
}

fn fit(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let mut s: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    s.push('…');
    s
}

/// Repository-relative paths only: an absolute or home path never reaches an
/// image that is meant to be shared.
fn shareable_path(p: &str) -> bool {
    !p.starts_with('/') && !p.starts_with('~') && !p.contains(':') && !p.starts_with("..")
}

fn outcome_style(o: AttemptOutcome) -> (&'static str, &'static str) {
    match o {
        AttemptOutcome::Succeeded => ("#3fb950", "✓ succeeded"),
        AttemptOutcome::Failed => ("#f85149", "✗ failed"),
        AttemptOutcome::Superseded => ("#a371f7", "↻ superseded"),
        AttemptOutcome::Abandoned => ("#d29922", "… abandoned"),
        AttemptOutcome::InProgress => ("#2f81f7", "▶ in progress"),
        AttemptOutcome::Unknown => ("#8b949e", "? unknown"),
    }
}

/// The work unit worth showing: the one with the most attempts among those
/// that contain a failure, else the most recently updated.
fn story_unit(p: &Projection) -> Option<&WorkUnit> {
    p.work_units
        .iter()
        .max_by_key(|w| (w.failure_count > 0, w.attempts.len(), w.updated_at))
}

fn attempts_of<'a>(w: &WorkUnit, p: &'a Projection) -> Vec<&'a Attempt> {
    let mut v: Vec<&Attempt> = w
        .attempts
        .iter()
        .filter_map(|id| p.attempts.iter().find(|a| a.attempt_id == *id))
        .collect();
    v.sort_by_key(|a| (a.started_at, a.attempt_id));
    v
}

/// Render the card. The result is a complete, self-contained SVG document.
pub fn render(p: &Projection, options: &CardOptions) -> String {
    let project = options
        .project
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("a local project");
    let unit = story_unit(p);
    let chain: Vec<&Attempt> = unit.map(|w| attempts_of(w, p)).unwrap_or_default();
    let failures = p.attempts.iter().filter(|a| a.outcome.is_failure()).count();
    let providers: Vec<&str> = {
        let mut v: Vec<&str> = Vec::new();
        for s in &p.sessions {
            let name = s.provider.display_name();
            if !v.contains(&name) {
                v.push(name);
            }
        }
        v
    };
    let paths: Vec<&str> = unit
        .map(|w| {
            w.paths
                .iter()
                .map(String::as_str)
                .filter(|s| shareable_path(s))
                .take(3)
                .collect()
        })
        .unwrap_or_default();

    let mut svg = String::new();
    let _ = write!(
        svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-label="AttemptDB summary for {label}">
<title>{label}</title>
<defs><style>
  .bg {{ fill: #0d1117; }}
  text {{ font-family: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", sans-serif; }}
  .mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
  .brand {{ fill: #e6edf3; font-size: 26px; font-weight: 700; }}
  .project {{ fill: #7d8590; font-size: 22px; }}
  .head {{ fill: #e6edf3; font-size: 42px; font-weight: 700; }}
  .sub {{ fill: #7d8590; font-size: 19px; }}
  .chip-label {{ fill: #e6edf3; font-size: 17px; font-weight: 600; }}
  .chip-note {{ fill: #8b949e; font-size: 14px; }}
  .arrow {{ fill: #7d8590; font-size: 22px; }}
  .stat-n {{ fill: #e6edf3; font-size: 30px; font-weight: 700; }}
  .stat-k {{ fill: #7d8590; font-size: 14px; letter-spacing: .04em; }}
  .foot {{ fill: #7d8590; font-size: 16px; }}
  .rule {{ stroke: #21262d; stroke-width: 1; }}
</style></defs>
<rect class="bg" width="{w}" height="{h}" rx="18"/>
<text class="brand" x="{m}" y="70">AttemptDB<tspan class="project" dx="14">{project}</tspan></text>
<text class="head" x="{m}" y="150">What the agents tried</text>
<text class="sub" x="{m}" y="186">{sub}</text>
"##,
        w = WIDTH,
        h = HEIGHT,
        m = MARGIN,
        label = esc(&format!("AttemptDB · {project}")),
        project = esc(&fit(project, 46)),
        sub = esc(&fit(
            &match unit {
                Some(_) => format!(
                    "the attempt path of one work unit{}",
                    options
                        .window
                        .as_deref()
                        .map(|w| format!(" · {w}"))
                        .unwrap_or_default()
                ),
                None => "no work unit was projected in this scope".to_string(),
            },
            96
        ))
    );

    // The attempt chain.
    let mut x = MARGIN;
    let mut y = 240.0;
    let mut drawn = 0usize;
    for (i, a) in chain.iter().enumerate() {
        if drawn == 8 {
            break;
        }
        let (colour, label) = outcome_style(a.outcome);
        let note = a
            .failure_class
            .clone()
            .unwrap_or_else(|| fit(&a.approach, 30));
        let text_chars = label.chars().count().max(note.chars().count() + 2);
        let width = 30.0 + text_chars as f64 * CHAR_W;
        if x + width > WIDTH - MARGIN {
            x = MARGIN;
            y += CHIP_H + ROW_GAP;
            if y + CHIP_H > 400.0 {
                break;
            }
        }
        if i > 0 && x > MARGIN {
            let supersedes = chain[i - 1].superseded_by == Some(a.attempt_id);
            let _ = write!(
                svg,
                "<text class=\"arrow\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                x - CHIP_GAP + 6.0,
                y + CHIP_H / 2.0 + 7.0,
                if supersedes { "⇒" } else { "→" }
            );
        }
        let _ = write!(
            svg,
            "<g><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{width:.1}\" height=\"{CHIP_H}\" rx=\"10\" fill=\"#161b22\" stroke=\"{colour}\" stroke-width=\"2\"/><text class=\"chip-label\" x=\"{tx:.1}\" y=\"{ty1:.1}\" fill=\"{colour}\">{label}</text><text class=\"chip-note mono\" x=\"{tx:.1}\" y=\"{ty2:.1}\">{note}</text></g>",
            tx = x + 15.0,
            ty1 = y + 26.0,
            ty2 = y + 47.0,
            label = esc(label),
            note = esc(&fit(&note, 32)),
        );
        x += width + CHIP_GAP;
        drawn += 1;
    }
    if chain.len() > drawn {
        let _ = write!(
            svg,
            "<text class=\"sub\" x=\"{:.1}\" y=\"{:.1}\">+{} more</text>",
            x,
            y + CHIP_H / 2.0 + 7.0,
            chain.len() - drawn
        );
    }

    // Paths the work touched.
    if !paths.is_empty() {
        let _ = write!(
            svg,
            "<text class=\"chip-note mono\" x=\"{m}\" y=\"420\">{}</text>",
            esc(&fit(&paths.join("   "), 118)),
            m = MARGIN
        );
    }

    // Stats.
    let stats: [(String, &str); 5] = [
        (p.sessions.len().to_string(), "SESSIONS"),
        (p.attempts.len().to_string(), "ATTEMPTS"),
        (failures.to_string(), "FAILED"),
        (p.handoffs.len().to_string(), "HANDOFFS"),
        (p.commits.len().to_string(), "COMMITS"),
    ];
    let _ = write!(
        svg,
        "<line class=\"rule\" x1=\"{m}\" y1=\"455\" x2=\"{x2}\" y2=\"455\"/>",
        m = MARGIN,
        x2 = WIDTH - MARGIN
    );
    for (i, (n, k)) in stats.iter().enumerate() {
        let sx = MARGIN + i as f64 * 190.0;
        let _ = write!(
            svg,
            "<text class=\"stat-n\" x=\"{sx:.1}\" y=\"505\">{}</text><text class=\"stat-k\" x=\"{sx:.1}\" y=\"530\">{}</text>",
            esc(n),
            esc(k)
        );
    }

    // Footer: who produced it, and the one sentence that must travel with it.
    let _ = write!(
        svg,
        "<text class=\"foot\" x=\"{m}\" y=\"578\">{}</text><text class=\"foot\" x=\"{m}\" y=\"602\">{}</text>",
        esc(&fit(
            &format!(
                "{} · {}",
                if providers.is_empty() {
                    "no agent observed".to_string()
                } else {
                    providers.join(" · ")
                },
                crate::TAGLINE
            ),
            108
        )),
        esc(if options.attribution {
            "Built with AttemptDB — the database for what agents tried"
        } else {
            ""
        }),
        m = MARGIN
    );
    svg.push_str("</svg>\n");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_of_nothing_still_renders() {
        let p = attemptdb_project::project(&[]);
        let svg = render(&p, &CardOptions::default());
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>\n"));
        assert!(svg.contains("no work unit was projected"));
    }

    #[test]
    fn the_demo_story_becomes_a_chain() {
        let events = crate::demo::events(attemptdb_core::Timestamp::now());
        let p = attemptdb_project::project(&events);
        let svg = render(
            &p,
            &CardOptions {
                project: Some("example/attemptdb".into()),
                window: Some("last 2 hours".into()),
                attribution: true,
            },
        );
        assert!(svg.contains("example/attemptdb"));
        assert!(
            svg.contains("✗ failed"),
            "the failed attempt is on the card"
        );
        assert!(svg.contains("Built with AttemptDB"));
        // No prompt text, ever.
        assert!(!svg.contains("Cut the 0.3.0 release"));
        assert!(!svg.contains("cargo test"));
        assert!(!svg.contains("/home/"));
    }

    #[test]
    fn only_repository_relative_paths_are_shareable() {
        assert!(shareable_path("crates/attemptdb-ui/src/card.rs"));
        assert!(!shareable_path("/home/alice/.config/notes.txt"));
        assert!(!shareable_path("~/notes.txt"));
        assert!(!shareable_path("C:/Users/alice/notes.txt"));
    }
}
