// AgentTimeline progressive enhancement. No inline scripts (strict CSP),
// nothing is injected as markup: everything from the API is rendered
// through textContent.
(function () {
  "use strict";

  function el(tag, text, cls) {
    var e = document.createElement(tag);
    if (text !== undefined && text !== null) e.textContent = String(text);
    if (cls) e.className = cls;
    return e;
  }

  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
  }

  // Scope bar: the project select carries "all projects" as a sentinel.
  var scope = document.querySelector("form.scope");
  if (scope) {
    var sel = scope.querySelector("select[name=project]");
    var all = scope.querySelector("input[name=all]");
    scope.addEventListener("submit", function () {
      if (sel && all) {
        if (sel.value === sel.getAttribute("data-all-value")) {
          all.value = "1";
          sel.value = "";
        } else {
          all.value = "";
        }
      }
    });
  }

  function cellText(v) {
    if (v === null || v === undefined) return "";
    if (Array.isArray(v)) return v.map(cellText).join(", ");
    if (typeof v === "object") return JSON.stringify(v);
    return String(v);
  }

  // Rows as key/value records (explanations) — every value via textContent.
  function renderRecords(container, rows, notes) {
    clear(container);
    if (!rows.length) {
      container.appendChild(el("p", "(no rows)", "muted"));
    }
    rows.forEach(function (row) {
      var dl = el("dl", null, "kv");
      Object.keys(row).forEach(function (k) {
        var v = row[k];
        if (v === null || v === "" || (Array.isArray(v) && !v.length)) return;
        dl.appendChild(el("dt", k));
        var dd = el("dd");
        if (typeof v === "string" && /^(att|ses|ev)_[0-9a-f-]{8,}$/.test(v)) {
          var a = el("a", v.slice(0, v.indexOf("_") + 9), "id");
          var page = v.slice(0, 3) === "ev_" ? "evidence" : v.slice(0, 4) === "att_" ? "attempt" : "session";
          a.href = "/" + page + "/" + encodeURIComponent(v) + window.location.search.replace(/[?&]at=[^&]*/, "").replace(/^&/, "?");
          a.title = v;
          dd.appendChild(a);
        } else if (Array.isArray(v)) {
          v.forEach(function (x, i) {
            if (i) dd.appendChild(document.createTextNode(" "));
            var c = el("code", cellText(x));
            dd.appendChild(c);
          });
        } else {
          dd.textContent = cellText(v);
        }
        dl.appendChild(dd);
      });
      container.appendChild(dl);
    });
    if (notes && notes.length) {
      var ul = el("ul", null, "notes");
      notes.forEach(function (n) { ul.appendChild(el("li", n)); });
      container.appendChild(ul);
    }
  }

  function renderTable(container, columns, rows, notes) {
    clear(container);
    if (!rows.length) {
      container.appendChild(el("p", "(no rows)", "muted"));
    } else {
      var wrap = el("div", null, "scroll");
      var table = el("table");
      var thead = el("thead");
      var tr = el("tr");
      columns.forEach(function (c) { tr.appendChild(el("th", c)); });
      thead.appendChild(tr);
      table.appendChild(thead);
      var tbody = el("tbody");
      rows.forEach(function (row) {
        var r = el("tr");
        columns.forEach(function (c) { r.appendChild(el("td", cellText(row[c]))); });
        tbody.appendChild(r);
      });
      table.appendChild(tbody);
      wrap.appendChild(table);
      container.appendChild(wrap);
      container.appendChild(el("p", "(" + rows.length + " row" + (rows.length === 1 ? "" : "s") + ")", "muted small"));
    }
    if (notes && notes.length) {
      var ul = el("ul", null, "notes");
      notes.forEach(function (n) { ul.appendChild(el("li", n)); });
      container.appendChild(ul);
    }
  }

  function showError(container, message) {
    clear(container);
    container.appendChild(el("pre", message, "error"));
  }

  // "Copy continuation brief": the text is rendered server-side into a data
  // attribute, so the clipboard gets exactly what the page shows.
  document.querySelectorAll("button.copy-brief").forEach(function (b) {
    b.addEventListener("click", function () {
      var text = b.getAttribute("data-brief") || "";
      var done = function (ok) {
        var before = b.textContent;
        b.textContent = ok ? "copied" : "press ⌘/Ctrl+C";
        setTimeout(function () { b.textContent = before; }, 1500);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(function () { done(true); }, function () { fallback(); });
      } else {
        fallback();
      }
      function fallback() {
        var ta = document.createElement("textarea");
        ta.value = text;
        ta.setAttribute("readonly", "readonly");
        ta.style.position = "fixed";
        ta.style.left = "-9999px";
        document.body.appendChild(ta);
        ta.select();
        var ok = false;
        try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
        document.body.removeChild(ta);
        done(ok);
      }
    });
  });

  // -------------------------------------------------------------------
  // Live invalidation: /api/live announces a revision, and each region
  // marked data-live refetches only its own resource. Nothing is injected
  // as markup — every value goes in through textContent.
  // -------------------------------------------------------------------
  var liveWrap = document.getElementById("live-wrap");
  var regions = document.querySelectorAll("[data-live]");
  if (liveWrap && window.EventSource) {
    var stateEl = document.getElementById("live-state");
    var pauseBtn = document.getElementById("live-pause");
    var scopeQs = liveWrap.getAttribute("data-scope") || "";
    var paused = false;
    var pending = 0;
    var revision = null;
    liveWrap.hidden = false;

    function api(path) {
      return path + scopeQs;
    }
    function setState(text, stale) {
      if (!stateEl) return;
      stateEl.textContent = text;
      stateEl.className = "live-state" + (stale ? " stale" : "");
    }
    function flash(node) {
      node.classList.remove("updated");
      void node.offsetWidth;
      node.classList.add("updated");
    }
    function noteReload(region) {
      if (region.querySelector("p.live-note")) return;
      var p = el("p", null, "live-note muted");
      p.appendChild(document.createTextNode("new events since this page was rendered — "));
      var a = el("a", "reload");
      a.href = window.location.href;
      p.appendChild(a);
      region.appendChild(p);
    }
    function badge(text, cls) {
      var b = el("span", text, "badge badge-" + cls);
      return b;
    }
    function link(href, text, cls) {
      var a = el("a", text, cls);
      a.href = href;
      return a;
    }
    function shortId(id) {
      var i = id.indexOf("_");
      return i < 0 ? id.slice(0, 8) : id.slice(0, i + 9);
    }
    function renderOverview(region, body) {
      var grid = region.querySelector(".live-grid");
      var sessions = body.active_sessions || [];
      if (!grid) {
        if (!sessions.length) return;
        noteReload(region);
        return;
      }
      clear(grid);
      if (!sessions.length) {
        grid.appendChild(el("p", "No session is open right now.", "muted"));
      }
      sessions.forEach(function (s) {
        var card = el("article", null, "live-session");
        var head = el("header");
        head.appendChild(el("span", s.provider_name || "?", "provider"));
        head.appendChild(document.createTextNode(" "));
        head.appendChild(link("/session/" + encodeURIComponent(s.session_id) + scopeQs, shortId(s.session_id), "id"));
        head.appendChild(document.createTextNode(" "));
        head.appendChild(el("span", s.project_name || "", "project"));
        card.appendChild(head);
        var turn = el("p");
        turn.appendChild(document.createTextNode(
          s.turn_index === null || s.turn_index === undefined ? "no turn yet" : "turn " + s.turn_index + " "
        ));
        if (s.turn_status) turn.appendChild(badge(s.turn_status, s.turn_status === "in_progress" ? "live" : "muted"));
        var tools = s.in_flight_tools || [];
        turn.appendChild(document.createTextNode(tools.length ? " · running " + tools.join(", ") : " · no tool in flight"));
        card.appendChild(turn);
        card.appendChild(el("p", "last event " + (s.last_activity_at || ""), "muted small"));
        if (s.blocked) {
          var b = el("p");
          b.appendChild(badge("blocked", "fail"));
          card.appendChild(b);
        }
        grid.appendChild(card);
      });
      flash(region);
    }
    function renderAttention(region, body) {
      var list = region.querySelector("ol.atn-list");
      var items = body.items || [];
      var count = body.total === undefined ? items.length : body.total;
      var navCount = document.querySelector("nav .nav-count");
      if (navCount) navCount.textContent = String(count);
      if (!list) return;
      // Only the count and the wait times are refreshed in place; a change
      // in membership asks for a reload rather than re-rendering evidence
      // links from JSON.
      var ids = [];
      items.forEach(function (i) { ids.push(i.attention_id); });
      var same = list.children.length === ids.length;
      if (same) {
        for (var i = 0; i < ids.length; i++) {
          if (list.children[i].id !== ids[i]) { same = false; break; }
        }
      }
      if (!same) { noteReload(region); return; }
      items.forEach(function (item, i) {
        var meta = list.children[i].querySelector(".atn-meta .waiting");
        if (meta) meta.textContent = "waiting " + humanMs(item.waiting_ms);
      });
      flash(region);
    }
    function humanMs(ms) {
      if (ms === null || ms === undefined) return "";
      var s = Math.floor(ms / 1000);
      if (s < 60) return s + "s";
      var m = Math.floor(s / 60);
      if (m < 60) return m + "m " + (s % 60) + "s";
      var h = Math.floor(m / 60);
      if (h < 24) return h + "h " + (m % 60) + "m";
      return Math.floor(h / 24) + "d " + (h % 24) + "h";
    }
    function refresh() {
      if (!regions.length) return;
      regions.forEach(function (region) {
        var kind = region.getAttribute("data-live");
        var url = kind === "attention" ? api("/api/attention") : api("/api/overview");
        fetch(url, { credentials: "same-origin" })
          .then(function (r) { return r.ok ? r.json() : null; })
          .then(function (body) {
            if (!body) return;
            if (kind === "attention") renderAttention(region, body);
            else renderOverview(region, body);
          })
          .catch(function () { /* the next revision will try again */ });
      });
    }
    if (pauseBtn) {
      pauseBtn.addEventListener("click", function () {
        paused = !paused;
        pauseBtn.textContent = paused ? "resume" : "pause";
        if (!paused && pending) { pending = 0; refresh(); }
        setState(paused ? "paused" + (pending ? " · " + pending + " waiting" : "") : "live", paused);
      });
    }
    var src = new EventSource(api("/api/live"));
    src.addEventListener("change", function (ev) {
      var data = {};
      try { data = JSON.parse(ev.data); } catch (e) { return; }
      if (data.revision === revision) return;
      var first = revision === null;
      revision = data.revision;
      if (first) { setState("live", false); return; }
      if (paused) {
        pending++;
        setState("paused · " + pending + " waiting", true);
        return;
      }
      setState("live · updated", false);
      refresh();
    });
    src.addEventListener("error", function () {
      setState("reconnecting…", true);
    });
    setState("connecting…", true);
  }

  // /state: slider <-> datetime input, live fetch.
  var stateForm = document.getElementById("state-form");
  if (stateForm) {
    var slider = document.getElementById("state-slider");
    var atInput = document.getElementById("state-at");
    var result = document.getElementById("state-result");
    var stmt = document.getElementById("state-statement");
    var live = document.getElementById("state-live");
    var api = stateForm.getAttribute("data-api");
    var timer = null;

    function toLocalValue(ms) {
      return new Date(ms).toISOString().slice(0, 19);
    }
    function fromInput() {
      var v = atInput.value;
      if (!v) return null;
      var t = Date.parse(v.length === 16 ? v + ":00Z" : v + "Z");
      return isNaN(t) ? null : t;
    }
    function fetchState(ms) {
      var iso = new Date(ms).toISOString().replace(/\.\d{3}Z$/, "Z");
      var url = api + (api.indexOf("?") >= 0 ? "&" : "?") + "at=" + encodeURIComponent(iso);
      live.textContent = "loading…";
      fetch(url, { credentials: "same-origin" })
        .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, body: j }; }); })
        .then(function (res) {
          if (!res.ok) { showError(result, res.body.error || "error"); live.textContent = ""; return; }
          stmt.textContent = res.body.statement;
          renderRecords(result, res.body.rows || [], res.body.notes || []);
          live.textContent = "live · " + iso;
        })
        .catch(function (e) { showError(result, String(e)); live.textContent = ""; });
    }
    function schedule(ms) {
      if (timer) clearTimeout(timer);
      timer = setTimeout(function () { fetchState(ms); }, 150);
    }
    slider.addEventListener("input", function () {
      var ms = Number(slider.value);
      atInput.value = toLocalValue(ms);
      schedule(ms);
    });
    atInput.addEventListener("change", function () {
      var ms = fromInput();
      if (ms === null) return;
      slider.value = String(Math.min(Math.max(ms, Number(slider.min)), Number(slider.max)));
      schedule(ms);
    });
    stateForm.addEventListener("submit", function (ev) {
      // Keep the URL shareable: the server renders `?at=<rfc3339>`.
      var ms = fromInput();
      if (ms !== null) atInput.value = new Date(ms).toISOString().replace(/\.\d{3}Z$/, "Z");
      void ev;
    });
  }

  // /query: examples fill the textarea, Ctrl+Enter submits, fetch renders
  // without a reload when JS is available.
  var queryForm = document.getElementById("query-form");
  if (queryForm) {
    var ta = document.getElementById("query-statement");
    var api = queryForm.getAttribute("data-api");
    document.querySelectorAll("a.example").forEach(function (a) {
      a.addEventListener("click", function (ev) {
        ev.preventDefault();
        ta.value = a.getAttribute("data-statement");
        ta.focus();
      });
    });
    ta.addEventListener("keydown", function (ev) {
      if ((ev.ctrlKey || ev.metaKey) && ev.key === "Enter") {
        ev.preventDefault();
        runQuery();
      }
    });
    queryForm.addEventListener("submit", function (ev) {
      if (!window.fetch) return;
      ev.preventDefault();
      runQuery();
    });
    function resultBox() {
      var box = document.getElementById("query-result");
      if (!box) {
        box = el("section", null, "card");
        box.id = "query-result";
        queryForm.parentNode.parentNode.insertBefore(box, queryForm.parentNode.nextSibling);
      }
      return box;
    }
    function runQuery() {
      var statement = ta.value;
      var format = queryForm.querySelector("select[name=format]").value;
      var box = resultBox();
      clear(box);
      box.appendChild(el("p", "running…", "muted small"));
      fetch(api, {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ statement: statement, format: format })
      })
        .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, body: j }; }); })
        .then(function (res) {
          if (!res.ok) { showError(box, res.body.error || "error"); return; }
          var head = el("p", null, "muted small");
          head.appendChild(el("code", res.body.statement));
          head.appendChild(document.createTextNode(" · " + res.body.row_count + " row" + (res.body.row_count === 1 ? "" : "s")));
          clear(box);
          box.appendChild(head);
          var out = el("div");
          box.appendChild(out);
          if (format === "json") {
            renderPre(out, JSON.stringify(res.body.rows, null, 2), res.body.notes);
          } else if (format === "csv") {
            renderPre(out, res.body.text || "", res.body.notes);
          } else if (res.body.kind === "explanation") {
            renderRecords(out, res.body.rows || [], res.body.notes || []);
          } else {
            renderTable(out, res.body.columns || [], res.body.rows || [], res.body.notes || []);
          }
          var url = new URL(window.location.href);
          url.searchParams.set("statement", statement);
          url.searchParams.set("format", format);
          history.replaceState(null, "", url.toString());
        })
        .catch(function (e) { showError(box, String(e)); });
    }
    function renderPre(container, text, notes) {
      clear(container);
      container.appendChild(el("pre", text, "json"));
      if (notes && notes.length) {
        var ul = el("ul", null, "notes");
        notes.forEach(function (n) { ul.appendChild(el("li", n)); });
        container.appendChild(ul);
      }
    }
  }
})();
