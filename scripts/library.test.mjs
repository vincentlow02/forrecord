import assert from "node:assert/strict";
import test from "node:test";
import { captureNotes, formatDuration, selectRecordings } from "../ui/library-core.js";

const records = [
  { name: "Product call", startedAt: "2026-02-03", durationFrames: 48_000, sampleRate: 48_000 },
  { name: "Design Interview", startedAt: "2026-02-01", durationFrames: 32_000, sampleRate: 16_000 },
  { name: "Customer Interview", startedAt: "2026-02-02", durationFrames: 96_000, sampleRate: 48_000 },
];

test("search normalizes case and width without mutating records", () => {
  assert.deepEqual(selectRecordings(records, " ＩＮＴＥＲＶＩＥＷ  ", "newest"), [records[2], records[1]]);
  assert.deepEqual(selectRecordings(records, "absent", "newest"), []);
  assert.equal(records[0].name, "Product call");
});

test("sorts by date or actual duration with a deterministic tiebreak", () => {
  assert.deepEqual(selectRecordings(records, "", "oldest"), [records[1], records[2], records[0]]);
  assert.deepEqual(selectRecordings(records, "", "longest"), [records[2], records[1], records[0]]);
  assert.deepEqual(selectRecordings(records, "", "shortest"), [records[0], records[2], records[1]]);
  assert.equal(formatDuration(59_520_000), "01:02:00");
});

test("distinguishes diagnostic indications from inserted silence and legacy warnings", () => {
  const notes = captureNotes({
    diagnostics: { driverXruns: 2, timestampEstimates: 1, droppedSourceFrames: 0, silenceFrames: 16, overlapFrames: 0 },
  }, "Me", 16_000);
  assert.deepEqual(notes, ["Me · 2 WASAPI discontinuities", "Me · 1 packet timestamps estimated", "Me · 1.0 ms padded with silence"]);
  assert.match(captureNotes({ discontinuities: 3 }, "Me", 16_000)[0], /cannot distinguish/);
});
