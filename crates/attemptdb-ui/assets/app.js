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
