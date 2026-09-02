//! Countable signals in tool output: how many tests passed and failed.
//!
//! A console shows "18/20 tests" only when it can count, and the count
//! must be metadata — the server never sees the output the number came
//! from. So the hook adapter reads the runner's summary line here, once,
//! and writes three integers (`tests_passed`, `tests_failed`,
//! `tests_skipped`) into `attrs`. Nothing else from the output leaves the
//! device.
//!
//! Recognised summaries (the last few kilobytes of the output are scanned,
//! every match is summed, so a multi-crate `cargo test` adds up):
//!
//! - cargo: `test result: ok. 18 passed; 2 failed; 1 ignored; …`
//! - nextest: `Summary [ 1.2s] 20 tests run: 18 passed, 2 failed`
//! - jest: `Tests: 2 failed, 18 passed, 1 skipped, 21 total`
//! - vitest: `Tests  18 passed | 2 failed | 1 skipped (21)`
//! - pytest: `==== 18 passed, 2 failed, 1 skipped in 3.2s ====`
//! - mocha: `18 passing` / `2 failing` / `1 pending`
//! - rspec: `20 examples, 2 failures, 1 pending`
//! - phpunit: `OK (18 tests, 40 assertions)` / `Tests: 20, Assertions: 40, Failures: 2.`
//! - dotnet: `Passed!  - Failed: 0, Passed: 18, Skipped: 0, Total: 18`
//! - go test -v: `--- PASS:` / `--- FAIL:` / `--- SKIP:` lines

/// Test counts read from one tool output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
}

impl TestCounts {
    pub fn is_empty(&self) -> bool {
        self.passed == 0 && self.failed == 0 && self.skipped == 0
    }

    fn add(&mut self, o: TestCounts) {
        self.passed += o.passed;
        self.failed += o.failed;
        self.skipped += o.skipped;
    }
}

/// Only the tail is read: summaries come last, and a 64 KiB output is
/// mostly the tests' own logging.
const TAIL: usize = 8 * 1024;

/// Test counts in `output`, when a runner's summary is recognisable.
pub fn test_counts(output: &str) -> Option<TestCounts> {
    let start = output.len().saturating_sub(TAIL);
    let mut start = start;
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    let tail = &output[start..];
    let mut total = TestCounts::default();
    let mut go = TestCounts::default();
    for raw in tail.lines() {
        let line = strip_ansi(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(c) = cargo_line(line)
            .or_else(|| nextest_line(line))
            .or_else(|| jest_line(line))
            .or_else(|| vitest_line(line))
            .or_else(|| pytest_line(line))
            .or_else(|| rspec_line(line))
            .or_else(|| phpunit_line(line))
            .or_else(|| dotnet_line(line))
        {
            total.add(c);
            continue;
        }
        if let Some(c) = mocha_line(line) {
            total.add(c);
            continue;
        }
        if line.starts_with("--- PASS:") {
            go.passed += 1;
        } else if line.starts_with("--- FAIL:") {
            go.failed += 1;
        } else if line.starts_with("--- SKIP:") {
            go.skipped += 1;
        }
    }
    if total.is_empty() {
        total = go;
    }
    (!total.is_empty()).then_some(total)
}

/// Drop ANSI colour sequences (`ESC [ … m`).
fn strip_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The number immediately before `word` in `line` (`18 passed` → 18), for
/// the first occurrence of `word` as a whole word.
fn count_before(line: &str, word: &str) -> Option<u64> {
    let mut search = 0;
    while let Some(pos) = line[search..].find(word) {
        let at = search + pos;
        let after_ok = line[at + word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if after_ok {
            let before = line[..at].trim_end();
            let digits: String = before
                .chars()
                .rev()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if !digits.is_empty()
                && before[..before.len() - digits.len()]
                    .chars()
                    .last()
                    .is_none_or(|c| !c.is_alphanumeric())
            {
                return digits.parse().ok();
            }
        }
        search = at + word.len();
    }
    None
}

/// The number immediately after `word:` (`Passed: 18` → 18).
fn count_after(line: &str, word: &str) -> Option<u64> {
    let at = line.find(word)?;
    let rest = line[at + word.len()..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn cargo_line(line: &str) -> Option<TestCounts> {
    if !line.starts_with("test result:") {
        return None;
    }
    Some(TestCounts {
        passed: count_before(line, "passed").unwrap_or(0),
        failed: count_before(line, "failed").unwrap_or(0),
        skipped: count_before(line, "ignored").unwrap_or(0),
    })
}

fn nextest_line(line: &str) -> Option<TestCounts> {
    if !(line.starts_with("Summary") && line.contains("tests run")) {
        return None;
    }
    Some(TestCounts {
        passed: count_before(line, "passed").unwrap_or(0),
        failed: count_before(line, "failed").unwrap_or(0),
        skipped: count_before(line, "skipped").unwrap_or(0),
    })
}

fn jest_line(line: &str) -> Option<TestCounts> {
    if !(line.starts_with("Tests:") && line.contains("total")) {
        return None;
    }
    Some(TestCounts {
        passed: count_before(line, "passed").unwrap_or(0),
        failed: count_before(line, "failed").unwrap_or(0),
        skipped: count_before(line, "skipped").unwrap_or(0)
            + count_before(line, "todo").unwrap_or(0),
    })
}

fn vitest_line(line: &str) -> Option<TestCounts> {
    // `Tests  18 passed | 2 failed (20)` — no colon after `Tests`.
    if !(line.starts_with("Tests ") && !line.starts_with("Tests:") && line.contains("passed")) {
        return None;
    }
    Some(TestCounts {
        passed: count_before(line, "passed").unwrap_or(0),
        failed: count_before(line, "failed").unwrap_or(0),
        skipped: count_before(line, "skipped").unwrap_or(0),
    })
}

fn pytest_line(line: &str) -> Option<TestCounts> {
    // `=== 18 passed, 2 failed, 1 skipped in 3.20s ===`
    let inner = line.trim_matches('=').trim();
    if !(line.starts_with('=')
        && inner.contains(" in ")
        && (inner.contains("passed") || inner.contains("failed")))
    {
        return None;
    }
    Some(TestCounts {
        passed: count_before(inner, "passed").unwrap_or(0),
        failed: count_before(inner, "failed").unwrap_or(0)
            + count_before(inner, "error").unwrap_or(0)
            + count_before(inner, "errors").unwrap_or(0),
        skipped: count_before(inner, "skipped").unwrap_or(0),
    })
}

fn mocha_line(line: &str) -> Option<TestCounts> {
    let passing = count_before(line, "passing");
    let failing = count_before(line, "failing");
    let pending = count_before(line, "pending");
    if passing.is_none() && failing.is_none() && pending.is_none() {
        return None;
    }
    // One count per line; the caller sums the three lines.
    Some(TestCounts {
        passed: passing.unwrap_or(0),
        failed: failing.unwrap_or(0),
        skipped: pending.unwrap_or(0),
    })
}

fn rspec_line(line: &str) -> Option<TestCounts> {
    if !(line.contains("examples,") || line.contains("example,")) || !line.contains("failure") {
        return None;
    }
    let examples = count_before(line, "examples").or_else(|| count_before(line, "example"))?;
    let failures = count_before(line, "failures")
        .or_else(|| count_before(line, "failure"))
        .unwrap_or(0);
    let pending = count_before(line, "pending").unwrap_or(0);
    Some(TestCounts {
        passed: examples.saturating_sub(failures + pending),
        failed: failures,
        skipped: pending,
    })
}

fn phpunit_line(line: &str) -> Option<TestCounts> {
    if line.starts_with("OK (") {
        let tests = count_before(line, "tests").or_else(|| count_before(line, "test"))?;
        return Some(TestCounts {
            passed: tests,
            ..Default::default()
        });
    }
    if line.starts_with("Tests:") && line.contains("Assertions:") {
        let tests = count_after(line, "Tests:")?;
        let failures =
            count_after(line, "Failures:").unwrap_or(0) + count_after(line, "Errors:").unwrap_or(0);
        let skipped = count_after(line, "Skipped:").unwrap_or(0)
            + count_after(line, "Incomplete:").unwrap_or(0);
        return Some(TestCounts {
            passed: tests.saturating_sub(failures + skipped),
            failed: failures,
            skipped,
        });
    }
    None
}

fn dotnet_line(line: &str) -> Option<TestCounts> {
    if !((line.starts_with("Passed!") || line.starts_with("Failed!")) && line.contains("Total:")) {
        return None;
    }
    Some(TestCounts {
        passed: count_after(line, "Passed:").unwrap_or(0),
        failed: count_after(line, "Failed:").unwrap_or(0),
        skipped: count_after(line, "Skipped:").unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(p: u64, f: u64, s: u64) -> Option<TestCounts> {
        Some(TestCounts {
            passed: p,
            failed: f,
            skipped: s,
        })
    }

    #[test]
    fn cargo_sums_every_binary() {
        let out = "running 3 tests\ntest a ... ok\n\ntest result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n\nrunning 20 tests\ntest result: FAILED. 18 passed; 2 failed; 0 ignored; 0 measured\n";
        assert_eq!(test_counts(out), c(21, 2, 1));
    }

    #[test]
    fn jest_vitest_pytest_mocha_rspec_phpunit_dotnet_nextest_go() {
        assert_eq!(
            test_counts("Tests:       2 failed, 18 passed, 1 skipped, 21 total\nTime: 3s"),
            c(18, 2, 1)
        );
        assert_eq!(
            test_counts(
                " ✓ src/a.test.ts (3)\n Test Files  1 failed | 4 passed (5)\n      Tests  18 passed | 2 failed | 1 skipped (21)\n"
            ),
            c(18, 2, 1)
        );
        assert_eq!(
            test_counts("========== 18 passed, 2 failed, 1 skipped in 3.20s =========="),
            c(18, 2, 1)
        );
        assert_eq!(test_counts("===== 5 passed in 0.10s ====="), c(5, 0, 0));
        assert_eq!(
            test_counts("\n  18 passing (2s)\n  1 pending\n  2 failing\n"),
            c(18, 2, 1)
        );
        assert_eq!(
            test_counts("Finished in 1.2 seconds\n21 examples, 2 failures, 1 pending"),
            c(18, 2, 1)
        );
        assert_eq!(test_counts("OK (18 tests, 40 assertions)"), c(18, 0, 0));
        assert_eq!(
            test_counts("Tests: 21, Assertions: 40, Failures: 2, Skipped: 1."),
            c(18, 2, 1)
        );
        assert_eq!(
            test_counts(
                "Passed!  - Failed:     0, Passed:    18, Skipped:     1, Total:    19, Duration: 1 s"
            ),
            c(18, 0, 1)
        );
        assert_eq!(
            test_counts("     Summary [   1.234s] 20 tests run: 18 passed, 2 failed"),
            c(18, 2, 0)
        );
        assert_eq!(
            test_counts(
                "=== RUN   TestA\n--- PASS: TestA (0.00s)\n--- FAIL: TestB (0.01s)\n--- SKIP: TestC\nFAIL\n"
            ),
            c(1, 1, 1)
        );
    }

    #[test]
    fn ansi_and_noise_do_not_matter_and_prose_does_not_count() {
        assert_eq!(
            test_counts("\u{1b}[32mTests:\u{1b}[0m 18 passed, 18 total"),
            c(18, 0, 0)
        );
        assert_eq!(
            test_counts("I passed the file to the function and it failed"),
            None
        );
        assert_eq!(test_counts("Compiling foo\nFinished dev profile"), None);
        assert_eq!(test_counts(""), None);
    }

    #[test]
    fn only_the_tail_is_read() {
        let mut out = "test result: ok. 99 passed; 0 failed; 0 ignored\n".to_string();
        out.push_str(&"x".repeat(TAIL + 10));
        out.push_str("\ntest result: ok. 1 passed; 0 failed; 0 ignored\n");
        assert_eq!(test_counts(&out), c(1, 0, 0));
    }
}
