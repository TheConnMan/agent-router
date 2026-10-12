// `agent-router nudge`: send one follow-up user message to an existing T3 thread.
//
// Run by the router as `node --input-type=module -e <this> -- <t3-thread lib dir> <thread id>`,
// with the message on stdin. Every T3 call goes through the t3-thread launcher's own lib modules,
// so auth, origin resolution, the orchestration protocol, the version gate, the run status
// vocabulary, and the message.dispatch payload have exactly one owner.
//
// On a verdict it prints one JSON line, {"outcome": "sent" | "not_found" | "active", "reason"},
// and exits 0. Anything else is a runtime error: a message on stderr and a nonzero exit.
import { randomUUID } from "node:crypto";
import path from "node:path";
import { pathToFileURL } from "node:url";

const [libDir, threadId] = process.argv.slice(1);
const lib = (name) => import(pathToFileURL(path.join(libDir, name)).href);

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8");
}

async function main() {
  const { resolveContext, missingOriginError } = await lib("context.mjs");
  const { connect, takeSnapshot } = await lib("rpc.mjs");
  const { withT3Session } = await lib("t3cli.mjs");
  const { buildMessageDispatch } = await lib("commands.mjs");
  const { TERMINAL_RUN_STATUSES } = await lib("completion.mjs");
  const { ensureCompatibleServer } = await lib("version.mjs");

  const text = await readStdin();
  const ctx = resolveContext({});
  const origin = ctx.origin;
  if (!origin) throw missingOriginError(ctx.baseDir);

  return withT3Session({ bin: ctx.bin, baseDir: ctx.baseDir, ttlMs: 10 * 60_000, label: "agent-router-nudge" }, async (token) => {
    const client = await connect(origin, token);
    try {
      await ensureCompatibleServer(client, { label: "agent-router nudge" });
      const snapshot = await takeSnapshot(client, "orchestration.subscribeShell", {});
      const thread = (snapshot.threads ?? []).find((t) => t.id === threadId);
      if (!thread || thread.deletedAt != null) return { outcome: "not_found", reason: `T3 has no thread ${threadId}` };
      if (thread.archivedAt != null) return { outcome: "not_found", reason: `T3 thread ${threadId} is archived` };
      // A thread's status is "idle" before its first run and its latest run's status after. Any
      // run status short of terminal (queued, preparing, starting, running, waiting) means work is
      // in flight. Best effort, not atomic: message.dispatch carries no idle precondition, so a run
      // another client starts between this snapshot and the dispatch below is not detected.
      const status = thread.status;
      if (thread.activeRunId != null || (status !== "idle" && !TERMINAL_RUN_STATUSES.includes(status))) {
        return { outcome: "active", reason: `T3 thread ${threadId} has an active run (status ${status})` };
      }
      await client.call(
        "orchestration.dispatchCommand",
        buildMessageDispatch({ threadId, messageId: randomUUID(), text, modelSelection: thread.modelSelection }),
      );
      return { outcome: "sent", reason: `sent a follow-up message to T3 thread ${threadId} (thread was ${status})` };
    } finally {
      client.close();
    }
  });
}

main().then(
  (result) => {
    process.stdout.write(`${JSON.stringify(result)}\n`);
    process.exit(0);
  },
  (err) => {
    process.stderr.write(`agent-router nudge: ${err?.message ?? err}\n`);
    process.exit(1);
  },
);
