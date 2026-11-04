// The only JavaScript: copy and share buttons, confirmations and polling. Everything
// works without it; it just saves some taps and refreshes. (Dialogs open
// through the command/commandfor attributes; the fallback below is for
// older browsers.)
"use strict";
(function () {
  // Make a copy/share button wide enough for "Copied!" up front, so clicking
  // it never resizes it.
  function reserve(button) {
    var label = button.querySelector("span") || button, old = label.textContent, w = button.offsetWidth;
    label.textContent = "Copied!";
    button.style.minWidth = Math.max(w, button.offsetWidth) + "px";
    label.textContent = old;
  }
  document.querySelectorAll("[data-copy],[data-share]").forEach(reserve);
  function copy(text, button) {
    function done() {
      // Only the text changes, so the button keeps its icon.
      var label = button.querySelector("span") || button, old = label.textContent;
      label.textContent = "Copied!";
      setTimeout(function () { label.textContent = old; }, 1600);
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
    // Share sheet where the browser has one (it needs https); else copy the link.
    var s = e.target.closest("[data-share]");
    if (s) {
      e.preventDefault();
      var url = s.getAttribute("data-share");
      if (navigator.share) {
        navigator.share({ title: s.getAttribute("data-share-title"), text: s.getAttribute("data-share-text"), url: url })
          .catch(function (err) { if (err && err.name !== "AbortError") copy(url, s); });
      } else copy(url, s);
    }
    if (e.target.matches("input[readonly]")) e.target.select();
  });
  document.addEventListener("click", function (e) {
    // Tapping the dim area behind a dialog closes it.
    if (e.target.tagName === "DIALOG") e.target.close();
    if ("commandForElement" in HTMLButtonElement.prototype) return;
    var b = e.target.closest("[commandfor]");
    var d = b && document.getElementById(b.getAttribute("commandfor"));
    if (d && d.showModal && b.getAttribute("command") === "show-modal") d.showModal();
  });
  document.addEventListener("submit", function (e) {
    var f = e.target.closest("form[data-confirm]");
    if (f && !window.confirm(f.getAttribute("data-confirm"))) e.preventDefault();
  });
  // Inputs that only apply while a checkbox is ticked (e.g. the host's email).
  document.querySelectorAll("[data-enable-with]").forEach(function (input) {
    var box = input.form && input.form.elements[input.getAttribute("data-enable-with")];
    if (!box) return;
    function sync() { input.disabled = !box.checked; }
    box.addEventListener("change", sync);
    sync();
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
