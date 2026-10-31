// Stands in for the claude CLI so the E2E run makes no model calls. It appends its
// arguments to fake-claude.log next to itself, then fails the way Claude Code does when
// asked to create a session under an id that already has a transcript. T3 runs .mjs
// binaries through node, so this works on every OS.
import fs from "node:fs";
import path from "node:path";

const args = process.argv.slice(2);
if (args.includes("--version")) {
  console.log("2.1.300 (Claude Code)");
  process.exit(0);
}
fs.appendFileSync(path.join(import.meta.dirname, "fake-claude.log"), JSON.stringify(args) + "\n");
const sessionId = args.find((a) => a.startsWith("--session-id="))?.slice("--session-id=".length);
console.error(sessionId ? `Error: Session ID ${sessionId} is already in use.` : "Error: fake claude does not run turns.");
process.exit(1);
