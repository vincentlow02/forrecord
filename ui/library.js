import { captureNotes, formatDuration, selectRecordings, trackLabels } from "./library-core.js";

const $ = (id) => document.getElementById(id);
const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const sizeFormat = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
const list = $("history-list");
const message = $("history-notice");
const confirmation = $("confirm-dialog");

let sessions = [];
let selected = null;
let busy = false;
let request = 0;
let playback = null;
let playbackRequest = 0;

function text(tag, className, copy) {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = copy;
  return element;
}

function button(label, action, style = "") {
  const element = text("button", `button ${style}`, label);
  element.type = "button";
  element.addEventListener("click", action);
  return element;
}

function announce(copy, error = false) {
  message.textContent = copy;
  message.classList.toggle("error", error);
  if (error) message.scrollIntoView({ block: "nearest" });
}

function setBusy(value) {
  busy = value;
  list.setAttribute("aria-busy", value);
}

function bytesLabel(bytes) {
  if (bytes >= 1024 ** 3) return `${sizeFormat.format(bytes / 1024 ** 3)} GB`;
  if (bytes >= 1024 ** 2) return `${sizeFormat.format(bytes / 1024 ** 2)} MB`;
  return `${sizeFormat.format(bytes / 1024)} KB`;
}

function releasePlayer() {
  playbackRequest++;
  if (!playback) return;
  const { audio, ticket } = playback;
  playback = null;
  audio.pause();
  audio.removeAttribute("src");
  audio.load();
  invoke("release_playback", { ticket }).catch(() => {});
}

async function playTrack(session, track, audio, trackButtons, status) {
  if (busy) return;
  if (playback?.track === track && playback.audio === audio) {
    if (audio.paused) await audio.play().catch(() => { status.textContent = "Playback could not start. Try again."; });
    return;
  }
  const position = playback?.audio === audio ? audio.currentTime : 0;
  releasePlayer();
  const current = playbackRequest;
  for (const [index, control] of trackButtons.entries()) control.setAttribute("aria-pressed", String(index === track));
  audio.hidden = false;
  status.textContent = `Loading ${trackLabels[track]}…`;
  try {
    const url = await invoke("prepare_playback", { id: session.id, track });
    const ticket = new URL(url).pathname.slice(1);
    if (current !== playbackRequest) {
      await invoke("release_playback", { ticket });
      return;
    }
    playback = { audio, ticket, track };
    if (position > 0) {
      audio.addEventListener("loadedmetadata", () => {
        if (current === playbackRequest && Number.isFinite(audio.duration)) {
          audio.currentTime = Math.min(position, audio.duration);
        }
      }, { once: true });
    }
    audio.src = url;
    await audio.play();
    if (current === playbackRequest) status.textContent = trackLabels[track];
  } catch (error) {
    if (current === playbackRequest) {
      status.textContent = error.name === "NotAllowedError" ? "Press Play to listen." : `Could not play ${trackLabels[track]}. ${error}`;
    }
  }
}

function playerFor(session) {
  const player = text("section", "library-player", "");
  player.setAttribute("aria-label", "Audio preview");
  const audio = document.createElement("audio");
  audio.controls = true;
  audio.preload = "metadata";
  audio.crossOrigin = "anonymous";
  audio.hidden = true;
  audio.setAttribute("aria-label", "Recording playback controls");
  const status = text("p", "muted player-status", "");
  status.setAttribute("role", "status");
  const chooser = text("div", "preview-tracks", "");
  const trackButtons = trackLabels.map((label, index) => {
    const control = button(label, () => playTrack(session, index, audio, trackButtons, status), `preview-track track-tone-${index}`);
    control.setAttribute("aria-pressed", "false");
    control.setAttribute("aria-label", `Preview ${label}`);
    control.disabled = !session.availableTracks[index];
    return control;
  });
  chooser.append(...trackButtons);
  audio.addEventListener("error", () => {
    if (audio.hasAttribute("src")) status.textContent = "This track could not be played.";
  });
  player.append(chooser, audio, status);
  return player;
}

function actionDialog(session, rename) {
  const input = $("confirm-name");
  const action = $("confirm-action");
  $("confirm-title").textContent = rename ? "Rename recording" : "Delete recording?";
  $("confirm-copy").textContent = rename ? "" : `“${session.name}” and its audio files will be permanently deleted.`;
  $("confirm-copy").hidden = rename;
  input.value = rename ? session.name : "";
  input.hidden = !rename;
  input.required = rename;
  action.classList.toggle("danger", !rename);
  action.textContent = rename ? "Rename" : "Delete";
  confirmation.returnValue = "";
  return new Promise((resolve) => {
    confirmation.addEventListener("close", () => {
      resolve(confirmation.returnValue === "confirm" ? (rename ? input.value : true) : null);
    }, { once: true });
    confirmation.showModal();
    if (rename) {
      input.focus();
      input.select();
    } else {
      $("confirm-form").querySelector("[value=cancel]").focus();
    }
  });
}

async function manageSession(session, kind) {
  if (busy) return;
  setBusy(true);
  try {
    const action = { kind };
    if (kind === "rename") {
      action.name = await actionDialog(session, true);
      if (action.name === null) return;
    }
    if (kind === "delete") {
      if (!await actionDialog(session, false)) return;
      releasePlayer();
    }
    await invoke("recording_file_action", { id: session.id, action });
    if (kind === "rename" || kind === "delete") {
      await load(kind === "rename" ? session.id : null);
      $("history-heading").focus();
    }
  } catch (error) {
    announce(String(error), true);
  } finally {
    setBusy(false);
  }
}

async function exportAudio(session, track) {
  if (busy) return;
  setBusy(true);
  announce("Exporting… Choose a destination in the Windows dialog.");
  try {
    const destination = await invoke("export_recording", { id: session.id, track });
    announce(destination ? `Exported to ${destination}` : "Export canceled.");
  } catch (error) {
    announce(String(error), true);
  } finally {
    setBusy(false);
  }
}

function detailFor(session) {
  const detail = text("div", "library-details", "");
  detail.id = `details-${session.id}`;
  detail.append(playerFor(session));
  if (session.bookmarks.length > 0) {
    const bookmarks = text("div", "bookmarks", "");
    bookmarks.append(text("strong", "", "Bookmarks"));
    for (const frame of session.bookmarks) bookmarks.append(text("span", "", formatDuration(frame, session.sampleRate)));
    detail.append(bookmarks);
  }
  const notes = [...session.issues];
  for (const [index, track] of session.tracks.entries()) {
    notes.push(...captureNotes(track, trackLabels[index], session.sampleRate));
    if (!session.availableTracks[index]) notes.push(`${trackLabels[index]} WAV is unavailable.`);
  }
  if (notes.length) {
    const warnings = text("details", "library-warnings", "");
    warnings.append(text("summary", "", "Capture details"));
    const items = text("ul", "", "");
    for (const note of notes) items.append(text("li", "", note));
    warnings.append(items);
    detail.append(warnings);
  }
  const actions = text("div", "library-actions", "");
  const exportActions = text("div", "library-actions-group", "");
  for (const [index, label] of trackLabels.entries()) {
    const action = button(`Export ${label}`, () => exportAudio(session, index));
    action.disabled = !session.availableTracks[index];
    exportActions.append(action);
  }
  const complete = button("Export complete session", () => exportAudio(session, null), "dark");
  complete.disabled = !session.availableTracks.every(Boolean);
  exportActions.append(complete);
  const fileActions = text("div", "library-actions-group", "");
  fileActions.append(
    button("Open folder", () => manageSession(session, "open_folder")),
    button("Rename", () => manageSession(session, "rename")),
    button("Delete", () => manageSession(session, "delete"), "delete"),
  );
  actions.append(exportActions, fileActions);
  detail.append(actions);
  return detail;
}

function rowFor(session) {
  const row = text("article", "history-item", "");
  const label = ["saved", "recovered", "interrupted"].includes(session.state) ? session.state : "partial";
  const expanded = selected === session.id;
  const toggle = text("button", "recording-row", "");
  toggle.type = "button";
  toggle.id = `recording-${session.id}`;
  toggle.setAttribute("aria-expanded", String(expanded));

  const title = text("span", "recording-name", session.name);
  const information = text("span", "recording-meta", "");
  information.append(
    text("span", "", dateFormat.format(new Date(session.startedAt))),
    text("span", "recording-duration", formatDuration(session.durationFrames, session.sampleRate)),
    text("span", "", bytesLabel(session.bytes)),
  );
  const badge = text("span", `recording-state ${label}`, label);
  const chevron = text("span", "recording-chevron", "");
  chevron.setAttribute("aria-hidden", "true");
  toggle.append(title, information, badge, chevron);
  toggle.addEventListener("click", () => {
    if (busy) return;
    selected = expanded ? null : session.id;
    refresh();
    $(toggle.id)?.focus();
  });

  const header = text("h3", "", "");
  header.append(toggle);
  row.append(header);
  if (expanded) {
    const detail = detailFor(session);
    toggle.setAttribute("aria-controls", detail.id);
    row.append(detail);
  }
  return row;
}

function refresh() {
  releasePlayer();
  const rows = selectRecordings(sessions, $("library-search").value, $("library-sort").value);
  list.replaceChildren(...rows.map(rowFor));
  const count = rows.length;
  $("library-count").textContent = count === sessions.length
    ? `${count} ${count === 1 ? "recording" : "recordings"}`
    : `${count} of ${sessions.length} recordings`;
  $("history-empty").hidden = count > 0;
  $("history-empty").textContent = sessions.length
    ? "No recordings match your search."
    : "Your saved recordings will appear here.";
}

async function load(id = null) {
  const current = ++request;
  selected = id;
  $("history-retry").hidden = true;
  $("history-empty").hidden = true;
  announce("Loading recordings…");
  setBusy(true);
  try {
    const { sessions: items, warnings } = await invoke("list_recordings");
    if (current !== request) return;
    sessions = items;
    refresh();
    announce(warnings.join(" · "), Boolean(warnings.length));
    if (id) $(`recording-${id}`)?.scrollIntoView({ block: "nearest" });
  } catch (error) {
    if (current !== request) return;
    list.replaceChildren();
    $("library-count").textContent = "";
    $("history-retry").hidden = false;
    announce(`Could not load recordings. ${error}`, true);
  } finally {
    if (current === request) setBusy(false);
  }
}

$("library-search").addEventListener("input", () => { if (!busy) refresh(); });
$("library-sort").addEventListener("change", () => { if (!busy) refresh(); });
$("history-retry").addEventListener("click", () => load());

export const library = {
  get busy() { return busy; },
  open(id = null) {
    if (id) $("library-search").value = "";
    list.replaceChildren();
    return load(id);
  },
  close() {
    request++;
    setBusy(false);
    releasePlayer();
    list.replaceChildren();
  },
};
