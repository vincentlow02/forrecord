export const trackLabels = ["Interviewer", "Me"];

export function formatDuration(frames, rate = 16_000) {
  const seconds = Math.max(0, Math.floor(frames / rate) || 0);
  return [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60]
    .map((value) => String(value).padStart(2, "0")).join(":");
}

const normalize = (value) => String(value).normalize("NFKC").toLocaleLowerCase().trim();
const byDate = (first, second) => Date.parse(first.startedAt) - Date.parse(second.startedAt);
const seconds = (item) => item.sampleRate > 0 ? item.durationFrames / item.sampleRate : 0;

export function selectRecordings(sessions, query, order) {
  const term = normalize(query);
  const matches = sessions.filter((session) => normalize(session.name).includes(term));
  const compare = {
    newest: (a, b) => byDate(b, a),
    oldest: byDate,
    longest: (a, b) => seconds(b) - seconds(a) || byDate(b, a),
    shortest: (a, b) => seconds(a) - seconds(b) || byDate(b, a),
  };
  return matches.sort(compare[order] ?? compare.newest);
}

export function captureNotes(track, label, rate) {
  const stats = track.diagnostics;
  if (!stats) {
    return track.discontinuities
      ? [`${label} reported ${track.discontinuities} capture warnings in V0.1. The original log cannot distinguish their causes.`]
      : [];
  }
  const notes = [];
  if (stats.driverXruns) notes.push(`${label} · ${stats.driverXruns} WASAPI discontinuities`);
  if (stats.renderXruns) notes.push(`${label} · ${stats.renderXruns} playback keepalive discontinuities`);
  if (stats.timestampEstimates) notes.push(`${label} · ${stats.timestampEstimates} packet timestamps estimated`);
  if (stats.droppedSourceFrames) notes.push(`${label} · ${stats.droppedSourceFrames} input frames dropped by the buffer`);
  if (stats.silenceFrames) notes.push(`${label} · ${(stats.silenceFrames * 1000 / rate).toFixed(1)} ms padded with silence`);
  if (stats.overlapFrames) notes.push(`${label} · ${(stats.overlapFrames * 1000 / rate).toFixed(1)} ms trimmed from overlapping packets`);
  return notes;
}
