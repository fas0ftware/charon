"use strict";
// Charon window. Every value from the API is inserted as text; the QR code is an <img> from a data: URL.
const $ = (id) => document.getElementById(id);
const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };
const H = { "X-Charon": "1" };   // required by the local API for anything that changes state

async function api(path, opts = {}) {
  const r = await fetch(path, { ...opts, headers: { ...H, ...(opts.headers || {}) } });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || `error ${r.status}`);
  return data;
}

async function stage(files) {
  for (const f of files) {
    $("msg").textContent = `Adding ${f.name}…`;
    try { await api(`/api/stage?name=${encodeURIComponent(f.name)}`, { method: "PUT", body: f }); }
    catch (e) { $("msg").textContent = `${f.name}: ${e.message}`; return refresh(); }
  }
  $("msg").textContent = "";
  refresh();
}

function render(s) {
  const ul = $("staged"); ul.replaceChildren();
  for (const f of s.staged) { const li = el("li"); li.append(el("span", f.name, "name"), el("span", f.human, "muted")); ul.append(li); }
  const sh = s.share;
  const active = sh && !sh.done;
  $("pick").hidden = !!active;
  $("sharing").hidden = !sh;
  if (!sh) return;
  $("qr").src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(sh.svg);
  const a = $("url"); a.textContent = sh.url; a.href = sh.url;
  $("fp").textContent = sh.fingerprint ? `Certificate SHA-256 ${sh.fingerprint.slice(0, 23)}… (your phone warns once about it)` : "Plain HTTP";
  $("left").textContent = sh.done ? `Done: ${sh.done}.` : `Ends in ${Math.floor(sh.left / 60)}:${String(sh.left % 60).padStart(2, "0")} or when everything was opened ${sh.each === 1 ? "once" : sh.each + " times"}.`;
  $("stop").hidden = !!sh.done;
  $("again").hidden = !sh.done;
  $("qr").classList.toggle("spent", !!sh.done);
  const items = $("items"); items.replaceChildren();
  for (const it of sh.items) {
    const li = el("li", undefined, it.done >= sh.each ? "ok" : "");
    li.append(el("span", it.note ? "Note" : it.name, "name"), el("span", it.done >= sh.each ? "✓ received" : it.done ? `${it.done}/${sh.each}` : "waiting", "muted"));
    items.append(li);
  }
  const log = $("log"); log.replaceChildren();
  for (const l of [...sh.log].reverse()) log.append(el("li", l));
}

async function refresh() {
  try { render(await api("/api/status", { method: "GET", headers: {} })); } catch (e) { $("msg").textContent = e.message; }
}

document.addEventListener("DOMContentLoaded", () => {
  const drop = $("drop");
  $("files").addEventListener("change", (e) => { stage([...e.target.files]); e.target.value = ""; });
  drop.addEventListener("dragover", (e) => { e.preventDefault(); drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", (e) => { e.preventDefault(); drop.classList.remove("over"); stage([...e.dataTransfer.files]); });
  $("go").addEventListener("click", async () => {
    try {
      await api("/api/share", { method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text: $("text").value, minutes: Number($("minutes").value), downloads: Number($("downloads").value) }) });
      $("text").value = "";   // the note now lives only in the share
      $("msg").textContent = "";
      refresh();
    } catch (e) { $("msg").textContent = e.message; }
  });
  $("clear").addEventListener("click", async () => { await api("/api/clear", { method: "POST" }); refresh(); });
  $("stop").addEventListener("click", async () => { await api("/api/stop", { method: "POST" }); setTimeout(refresh, 300); });
  $("again").addEventListener("click", async () => { await api("/api/clear", { method: "POST" }); $("sharing").hidden = true; $("pick").hidden = false; refresh(); });
  refresh();
  setInterval(refresh, 1500);
});
