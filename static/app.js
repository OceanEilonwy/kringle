// The only JavaScript: copy buttons, confirmations and polling. Everything
// works without it; it just saves some taps and refreshes.
"use strict";
(function () {
  function copy(text, button) {
    function done() {
      var old = button.textContent;
      button.textContent = "Copied!";
      setTimeout(function () { button.textContent = old; }, 1600);
    }
    function fallback() {
      var t = document.createElement("textarea");
      t.value = text;
      t.setAttribute("readonly", "");
      t.style.position = "fixed";
      t.style.opacity = "0";
      document.body.appendChild(t);
      t.select();
      try { document.execCommand("copy"); done(); } catch (e) {}
      t.remove();
    }
    // Routers usually serve plain http, where the clipboard API is off.
    if (navigator.clipboard && window.isSecureContext) navigator.clipboard.writeText(text).then(done, fallback);
    else fallback();
  }
  document.addEventListener("click", function (e) {
    var b = e.target.closest("[data-copy]");
    if (b) { e.preventDefault(); copy(b.getAttribute("data-copy"), b); }
    if (e.target.matches("input[readonly]")) e.target.select();
  });
  document.addEventListener("submit", function (e) {
    var f = e.target.closest("form[data-confirm]");
    if (f && !window.confirm(f.getAttribute("data-confirm"))) e.preventDefault();
  });
  function every(ms, fn) { setInterval(function () { if (!document.hidden) fn(); }, ms); }
  // Swap in fresh copies of a few elements (e.g. who's joined) from the same page.
  document.querySelectorAll("[data-poll]").forEach(function (el) {
    var url = el.getAttribute("data-poll"), ids = el.getAttribute("data-poll-ids").split(" ");
    every(10000, function () {
      fetch(url, { cache: "no-store" }).then(function (r) { return r.ok ? r.text() : null; }).then(function (html) {
        if (!html) return;
        var doc = new DOMParser().parseFromString(html, "text/html");
        ids.forEach(function (id) {
          var cur = document.getElementById(id), next = doc.getElementById(id);
          if (cur && next) cur.replaceWith(document.importNode(next, true));
        });
      }).catch(function () {});
    });
  });
  // Reload once names are drawn so the gift tag appears.
  document.querySelectorAll("[data-reload-when]").forEach(function (el) {
    var url = el.getAttribute("data-reload-when");
    every(10000, function () {
      fetch(url, { cache: "no-store" }).then(function (r) { if (r.status === 200) location.reload(); }).catch(function () {});
    });
  });
})();
