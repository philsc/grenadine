// Highlights syntax off the page's thread, so a job that stalls inside
// syntect can be killed without freezing the page. The page sends one
// job at a time and terminates this worker when it stops answering.

import init, {
  highlight_load_syntaxes,
  highlight_job,
} from "/grenadine_web.js";

self.onmessage = async (event) => {
  const data = event.data;
  // The first message carries the compiled WebAssembly module from the
  // page, so nothing here is fetched or compiled again.
  if (data instanceof WebAssembly.Module) {
    await init({ module_or_path: data });
    highlight_load_syntaxes();
    self.postMessage(JSON.stringify({ type: "ready" }));
    return;
  }
  const { id, path, lang, text } = data;
  const lines = highlight_job(id, path ?? lang, path !== undefined, text);
  self.postMessage(JSON.stringify({ id, lines }));
};
