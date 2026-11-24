"use strict";
// Upload page: each picked file is sent as the raw body of one PUT (no multipart), with a progress bar.
// Names and messages are inserted with textContent, never as HTML.
const el = (tag, text, cls) => { const e = document.createElement(tag); if (text !== undefined) e.textContent = text; if (cls) e.className = cls; return e; };

function human(b) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (b >= 1000 && i < u.length - 1) { b /= 1000; i++; }
  return i ? `${b.toFixed(1)} ${u[i]}` : `${b} B`;
}

function send(file, row) {
  return new Promise((resolve) => {
    const bar = el("progress"); bar.max = 1; bar.value = 0;
    const note = el("span", human(file.size), "muted");
    row.append(bar, note);
    const xhr = new XMLHttpRequest();
    xhr.open("PUT", `up?name=${encodeURIComponent(file.name)}`);
    xhr.upload.onprogress = (e) => { if (e.lengthComputable) bar.value = e.loaded / e.total; };
    xhr.onload = () => {
      let msg = "sent";
      try { const r = JSON.parse(xhr.responseText); msg = r.ok ? `saved as ${r.name}` : r.error; } catch (_) { msg = `error ${xhr.status}`; }
      note.textContent = msg;
      row.classList.add(xhr.status === 200 ? "ok" : "bad");
      resolve();
    };
    xhr.onerror = () => { note.textContent = "connection lost"; row.classList.add("bad"); resolve(); };
    xhr.send(file);
  });
}

document.addEventListener("DOMContentLoaded", () => {
  const form = document.getElementById("up");
  if (!form) return;
  const list = document.getElementById("progress");
  const max = Number(list.dataset.max || 0);
  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const files = [...document.getElementById("pick").files];
    for (const f of files) {
      const row = el("li");
      row.append(el("span", f.name, "name"));
      list.append(row);
      if (max && f.size > max) { row.append(el("span", `too large (limit ${human(max)})`, "muted")); row.classList.add("bad"); continue; }
      await send(f, row);
    }
  });
});
