// The main window draws what the `attempt` binary reports about this
// machine (`setup --dry-run --json`) and asks it to change things. Every
// fact on screen comes from that report; the page decides nothing.

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

const NAMES = { "claude-code": "Claude Code", codex: "Codex CLI", cursor: "Cursor", "gemini-cli": "Gemini CLI" };
const ORDER = ["claude-code", "codex", "cursor", "gemini-cli"];
const HERE = /Mac/.test(navigator.platform) ? "this Mac" : "this machine";

let busy = false;

const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const tilde = (p) => String(p ?? "").replace(/^\/Users\/[^/]+|^\/home\/[^/]+|^[A-Z]:\\Users\\[^\\]+/, "~");
const list = (xs) => (xs.length <= 1 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`);

function setBusy(on, button) {
  busy = on;
  for (const b of document.querySelectorAll("button")) b.disabled = on || b.dataset.off === "1";
  if (button) button.classList.toggle("busy", on);
}

function able(id, on) {
  const b = $(id);
  b.dataset.off = on ? "0" : "1";
  b.disabled = !on;
}

function headline(text, lede, needs) {
  const h = $("headline");
  if (h.textContent !== text) {
    h.classList.add("changing");
    requestAnimationFrame(() => {
      h.textContent = text;
      h.classList.remove("changing");
    });
  }
  $("lede").textContent = lede || "";
  const n = $("needs");
  n.hidden = !needs;
  n.textContent = needs || "";
}

// One branch per agent AttemptDB knows, in a fixed order, so the picture
// keeps its shape as agents come and go.
function branches(report) {
  const dry = report ? report.dry_run : true;
  const actions = Object.fromEntries(((report && report.hooks.actions) || []).map((a) => [a.agent, a]));
  const checks = Object.fromEntries(((report && report.agents) || []).map((a) => [a.agent, a]));
  const rows = ORDER.map((id) => {
    const a = actions[id];
    const c = checks[id];
    const where = a ? tilde(a.config_path) : c && c.detected ? tilde(c.config_path) : "";
    let cls = "off";
    let mark = "not detected";
    if (a) {
      const k = a.outcome.kind;
      if (k === "already_current") { cls = "live"; mark = "capturing"; }
      else if (k === "installed") { cls = dry ? "todo" : "live"; mark = dry ? "will be wired" : "wired"; }
      else if (k === "updated") { cls = dry ? "todo" : "live"; mark = dry ? "will be updated" : "updated"; }
      else if (k === "skipped") { cls = "warn"; mark = `skipped, ${a.outcome.detail}`; }
      else if (k === "failed") { cls = "fail"; mark = `failed, ${a.outcome.detail}`; }
      if (c && c.detected && ["untrusted", "disabled"].includes(c.state) && cls === "live") { cls = "warn"; mark = c.state === "untrusted" ? "waiting for your trust" : "disabled in the agent"; }
    }
    return `<li class="branch ${cls}"><span class="name">${esc(NAMES[id])}</span><span class="where path">${esc(where)}</span><span class="mark">${esc(mark)}</span></li>`;
  });
  $("branches").innerHTML = rows.join("");
}

function daemonText(d, dry) {
  if (!d) return ["", ""];
  if (d.error) return [`failed: ${d.error}`, "fail"];
  if (d.running) return [d.registered ? `running, pid ${d.pid}` : `running, pid ${d.pid}, not registered as a service`, ""];
  if (d.skipped) return [`not registered (${d.skipped})`, ""];
  if (d.registered) return ["registered, not running", "warn"];
  return [dry ? "will be registered at login" : "not running", ""];
}

function facts(p) {
  const r = p.report;
  const inst = p.binaries.installed;
  const bund = p.binaries.bundled;
  const rows = [];
  if (inst && inst.supports_setup) rows.push(["attempt", `${esc(inst.version || "")}<span class="path">${esc(tilde(inst.path))}</span>`]);
  else if (bund) rows.push(["attempt", `not installed yet, this app carries ${esc(bund.version || "?")}<span class="path">${esc(tilde(p.install_dir))}</span>`]);
  else rows.push(["attempt", "not found, and this build carries none", "fail"]);
  if (r) {
    const db = r.database;
    rows.push(["database", `${db.existed ? "ready" : "will be created"}, ${esc(db.capture_mode.replace("_", " "))}<span class="path">${esc(tilde(db.path))}</span>`]);
    const [t, cls] = daemonText(r.daemon, r.dry_run);
    rows.push(["daemon", esc(t), cls]);
  }
  if (p.error) rows.push(["error", esc(p.error), "fail"]);
  $("facts").innerHTML = rows.map(([k, v, cls]) => `<span class="k">${esc(k)}</span><span class="v ${cls || ""}">${v}</span>`).join("");
}

function render(p) {
  $("cmd").textContent = p.install_command;
  const r = p.report;
  const inst = p.binaries.installed && p.binaries.installed.supports_setup ? p.binaries.installed : null;
  const bund = p.binaries.bundled;
  branches(r);
  facts(p);

  const wired = r ? (r.hooks.actions || []).filter((a) => a.outcome.kind === "already_current").map((a) => NAMES[a.agent]) : [];
  const detected = r ? (r.hooks.actions || []).length : 0;
  const allCurrent = !!(r && inst && r.database.existed && detected > 0 && wired.length === detected && (r.daemon.running || r.daemon.skipped));
  const needs = r && r.needs_you && r.needs_you.length ? r.needs_you.join(" ") : "";

  if (!r && !bund && !inst) {
    headline("Nothing to install", "This build of the app carries no attempt binary. Use the terminal command below.", "");
  } else if (!r) {
    headline(`Can't read ${HERE}`, p.error ? "" : "attempt did not answer.", "");
  } else if (allCurrent) {
    headline("Capturing", `${list(wired)} ${wired.length === 1 ? "sends" : "send"} every session here. Open the timeline to see what ${wired.length === 1 ? "it" : "they"} tried.`, needs);
  } else if (!inst) {
    headline(`Not set up on ${HERE}`, `Setup puts attempt in ${tilde(p.install_dir)}, creates your local database, and wires ${detected ? `the ${detected === 1 ? "agent" : `${detected} agents`} found here` : "the agents it finds"}. Nothing leaves the machine.`, needs);
  } else if (detected === 0) {
    headline("No coding agents found", "AttemptDB looks for Claude Code, Codex, Cursor and Gemini CLI. Install one and check again.", needs);
  } else {
    const todo = (r.hooks.actions || []).filter((a) => ["installed", "updated"].includes(a.outcome.kind)).map((a) => NAMES[a.agent]);
    headline(wired.length ? "Partly wired" : `Ready to set up ${HERE}`, todo.length ? `Setup will wire ${list(todo)}${r.database.existed ? "" : " and create your local database"}.` : "Setup will finish what is missing.", needs);
  }

  // The primary action follows the state: wire the machine, then open the door.
  const setup = $("btn-setup");
  setup.textContent = inst ? `Set up ${HERE}` : "Install and set up";
  setup.hidden = allCurrent;
  $("btn-timeline").classList.toggle("primary", allCurrent);
  $("btn-timeline").classList.toggle("quiet", !allCurrent);
  able("btn-setup", !!(bund || inst) && !allCurrent);
  able("btn-timeline", !!(r && r.database.existed && inst));
  able("btn-doctor", !!inst);
  able("btn-uninstall", !!inst);
  able("btn-recheck", true);
  able("btn-copy", true);
  able("output-close", true);
  able("btn-uninstall-yes", true);
  able("btn-uninstall-no", true);
}

async function refresh() {
  try {
    render(await invoke("probe"));
  } catch (e) {
    headline(`Can't read ${HERE}`, String(e), "");
  }
}

function output(title, text) {
  $("output-title").textContent = title;
  $("output-text").textContent = text;
  $("output").hidden = false;
  $("output").scrollIntoView({ block: "nearest" });
}

async function runSetup() {
  if (busy) return;
  const button = $("btn-setup");
  setBusy(true, button);
  $("output").hidden = true;
  try {
    const { report } = await invoke("setup", { captureMode: null });
    if (!report.ok) output("Setup finished with problems", (report.problems || []).join("\n"));
  } catch (e) {
    output("Setup failed", String(e));
  } finally {
    setBusy(false, button);
    await refresh();
  }
}

async function openTimeline() {
  if (busy) return;
  const button = $("btn-timeline");
  setBusy(true, button);
  try {
    await invoke("open_timeline");
  } catch (e) {
    output("The timeline did not open", String(e));
  } finally {
    setBusy(false, button);
  }
}

async function runDoctor() {
  if (busy) return;
  const button = $("btn-doctor");
  setBusy(true, button);
  output("Doctor", "Running. The activity scan reads the whole database, which can take a while on a large one.");
  try {
    const { ok, text } = await invoke("doctor");
    output(ok ? "Doctor found no problems" : "Doctor found something", text);
  } catch (e) {
    output("Doctor failed", String(e));
  } finally {
    setBusy(false, button);
  }
}

async function runUninstall() {
  if (busy) return;
  $("confirm").hidden = true;
  const button = $("btn-uninstall");
  setBusy(true, button);
  try {
    const { ok, text } = await invoke("uninstall");
    output(ok ? "Hooks removed. Your history is still on disk." : "Uninstall reported a problem", text);
  } catch (e) {
    output("Uninstall failed", String(e));
  } finally {
    setBusy(false, button);
    await refresh();
  }
}

async function copyCommand() {
  const text = $("cmd").textContent;
  const b = $("btn-copy");
  try {
    await navigator.clipboard.writeText(text);
    b.textContent = "Copied";
  } catch {
    const range = document.createRange();
    range.selectNodeContents($("cmd"));
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    b.textContent = "Selected";
  }
  setTimeout(() => (b.textContent = "Copy"), 1600);
}

$("btn-setup").addEventListener("click", runSetup);
$("btn-timeline").addEventListener("click", openTimeline);
$("btn-recheck").addEventListener("click", () => !busy && refresh());
$("btn-doctor").addEventListener("click", runDoctor);
$("btn-uninstall").addEventListener("click", () => ($("confirm").hidden = false));
$("btn-uninstall-yes").addEventListener("click", runUninstall);
$("btn-uninstall-no").addEventListener("click", () => ($("confirm").hidden = true));
$("btn-copy").addEventListener("click", copyCommand);
$("output-close").addEventListener("click", () => ($("output").hidden = true));

refresh();
