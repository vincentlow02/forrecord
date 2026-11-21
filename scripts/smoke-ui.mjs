import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";

const [port, screenshot] = process.argv.slice(2);

async function eventually(check, failure, milliseconds = 12_000) {
  const deadline = Date.now() + milliseconds;
  do {
    const result = await check();
    if (result) return result;
    await delay(100);
  } while (Date.now() < deadline);
  throw new Error(failure);
}

const target = await eventually(async () => {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/json/list`, {
      signal: AbortSignal.timeout(1000),
    });
    const targets = await response.json();
    return targets.find((item) => item.type === "page" && item.title === "NexQ Recorder");
  } catch {
    return null;
  }
}, "NexQ Recorder WebView did not start.", 60_000);

const socket = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", reject, { once: true });
});
let nextId = 0;
const messages = new Map();
socket.addEventListener("message", ({ data }) => {
  const { id, error, result } = JSON.parse(data);
  const handler = messages.get(id);
  if (!handler) return;
  messages.delete(id);
  if (error) handler.reject(new Error(error.message));
  else handler.resolve(result);
});

function command(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timeout = setTimeout(() => {
      messages.delete(id);
      reject(new Error(`WebView did not respond to ${method}.`));
    }, 8000);
    messages.set(id, {
      resolve: (value) => { clearTimeout(timeout); resolve(value); },
      reject: (error) => { clearTimeout(timeout); reject(error); },
    });
    socket.send(JSON.stringify({ id, method, params }));
  });
}

async function evaluate(expression) {
  const response = await command("Runtime.evaluate", {
    expression, returnByValue: true, awaitPromise: true,
  });
  if (response.exceptionDetails) {
    const { exception, text } = response.exceptionDetails;
    throw new Error(exception?.description ?? exception?.value ?? text);
  }
  return response.result.value;
}

try {
  await eventually(() => evaluate(
    'window.__TAURI__ && document.querySelector("#record-button")?.disabled === false && document.querySelector("#phase").dataset.state === "idle"'
  ), "Recorder controls and IPC did not initialize.");
  const state = await evaluate('window.__TAURI__.core.invoke("recording_status")');
  assert.equal(state.phase, "idle");
  assert.equal(state.tracks.length, 2);
  await assert.rejects(
    evaluate('window.__TAURI__.core.invoke("recording_action", { action: "pause" })'),
    /There is no active recording/
  );
  assert.equal(await evaluate('document.querySelectorAll(".wave span").length'), 60);
  const sizes = [[1120, 705], [890, 570], [747, 470], [560, 355], [420, 350]];
  for (const [width, height] of sizes) {
    await command("Emulation.setDeviceMetricsOverride", {
      width, height, deviceScaleFactor: 1, mobile: false,
    });
    const fits = await evaluate(`(() => {
      const record = document.querySelector("#record-button").getBoundingClientRect();
      return record.bottom <= innerHeight && document.documentElement.scrollWidth <= innerWidth;
    })()`);
    assert.ok(fits, `Recording controls overflow at ${width} × ${height}.`);
  }
  await command("Emulation.setDeviceMetricsOverride", {
    width: 1120, height: 705, deviceScaleFactor: 1, mobile: false,
  });
  if (screenshot) {
    const { data } = await command("Page.captureScreenshot", { format: "png" });
    await writeFile(screenshot, Buffer.from(data, "base64"));
  }
  await evaluate('document.querySelector("#settings-button").click()');
  await eventually(() => evaluate(
    'document.querySelector("#settings-dialog").open && Boolean(document.querySelector("#recordings-folder").value)'
  ), "Settings dialog did not load actual recording preferences.");
  assert.equal(await evaluate('document.querySelector("#mic-device").options[0].text'), "Windows default");
  await evaluate('document.querySelector("#settings-dialog").close()');
  await evaluate('document.querySelector("#history-button").click()');
  await eventually(() => evaluate(`(() => {
    const dialog = document.querySelector("#history-dialog");
    const notice = document.querySelector("#history-notice");
    return dialog.open && notice.textContent !== "Loading recordings…";
  })()`), "History dialog did not load.");
  assert.equal(await evaluate('document.querySelector("#history-notice").classList.contains("error")'), false);
  console.log("Packaged WebView, Rust IPC, recorder layout, Settings and History passed.");
} finally {
  socket.close();
}
