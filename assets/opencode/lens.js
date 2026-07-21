// lens lifecycle bridge for opencode.
//
// Adapts opencode's plugin hooks/events to the `lens hook opencode <event>`
// stdin/stdout contract (the same binary that serves the lens_* MCP tools),
// so routing, adoption counters, and session continuity work like the Claude
// Code hooks. Installed to <config dir>/plugins/lens.js by
// `lens session install --client opencode` / `lens setup --client opencode`;
// the __LENS_BIN__ / __LENS_ROUTING__ placeholders are filled at install time.
//
// Fail-open by design: any error or timeout in the bridge yields `{}` and the
// tool call proceeds. The only intentional interruption is a lens routing
// deny, surfaced by throwing (opencode blocks the call and shows the reason).
import { spawnSync } from "node:child_process";

const LENS_BIN = "__LENS_BIN__";
const LENS_ROUTING = "__LENS_ROUTING__";

// opencode tool ids are lowercase; lens's classifiers expect Claude Code
// names. MCP tools (lens_*) pass through unchanged.
const TOOL_NAMES = {
  bash: "Bash",
  read: "Read",
  grep: "Grep",
  glob: "Glob",
  list: "LS",
  edit: "Edit",
  patch: "Edit",
  multiedit: "MultiEdit",
  write: "Write",
  webfetch: "WebFetch",
  task: "Task",
  todowrite: "TodoWrite",
  todoread: "TodoRead",
};

function runHook(event, payload) {
  try {
    const res = spawnSync(LENS_BIN, ["hook", "opencode", event], {
      input: JSON.stringify(payload),
      env: { ...process.env, LENS_HOST: "opencode", LENS_ROUTING },
      timeout: 10000,
      encoding: "utf8",
    });
    if (res.status !== 0 || !res.stdout) return {};
    return JSON.parse(res.stdout);
  } catch {
    return {};
  }
}

export const LensPlugin = async ({ directory }) => {
  const base = (sessionID) => ({
    session_id: sessionID || undefined,
    cwd: directory,
  });
  const toolName = (t) => TOOL_NAMES[t] ?? t;
  // The additionalContext a hook returns, or "" (routing nudges, supersession
  // notices, prompt-intent steers all arrive through this field).
  const hookCtx = (res) =>
    (res &&
      res.hookSpecificOutput &&
      typeof res.hookSpecificOutput.additionalContext === "string" &&
      res.hookSpecificOutput.additionalContext) ||
    "";
  // opencode's after-callback carries no tool args, but lens's PostToolUse
  // handlers key on them (file_path/command/... drive continuity events and
  // editpath marking). Stash {args, ctx} per callID in before, replay in
  // after: `ctx` is PreToolUse additionalContext, which has no pre-call
  // injection channel in opencode and is delivered via the tool result below.
  // Capped: a denied or errored call never reaches after, so entries would
  // otherwise accumulate for the life of the session.
  const pendingArgs = new Map();

  return {
    "tool.execute.before": async (input, output) => {
      const res = runHook("PreToolUse", {
        ...base(input.sessionID),
        tool_name: toolName(input.tool),
        tool_input: output.args ?? {},
      });
      const out = res && res.hookSpecificOutput;
      if (out) {
        if (out.permissionDecision === "deny") {
          throw new Error(out.permissionDecisionReason || "denied by lens routing");
        }
        if (out.permissionDecision === "allow" && out.updatedInput && output.args) {
          Object.assign(output.args, out.updatedInput);
        }
      }
      if (input.callID != null) {
        pendingArgs.set(input.callID, { args: output.args ?? {}, ctx: hookCtx(res) });
        if (pendingArgs.size > 256) {
          pendingArgs.delete(pendingArgs.keys().next().value);
        }
      }
    },

    "tool.execute.after": async (input, output) => {
      const pending = input.callID != null ? pendingArgs.get(input.callID) : undefined;
      if (input.callID != null) pendingArgs.delete(input.callID);
      const res = runHook("PostToolUse", {
        ...base(input.sessionID),
        tool_name: toolName(input.tool),
        tool_input: (pending && pending.args) ?? input.args ?? {},
        tool_response: output && output.output != null ? output.output : null,
      });
      // The tool result string is opencode's only injection channel for tool
      // context: append the stashed PreToolUse nudge (the model sees it one
      // step later than on Claude Code, but it lands) and any PostToolUse
      // notice, labeled so the model can tell them from real tool output.
      const notes = [(pending && pending.ctx) || "", hookCtx(res)].filter(Boolean);
      if (notes.length && output && typeof output.output === "string") {
        output.output += "\n\n[lens] " + notes.join("\n[lens] ");
      }
    },

    "chat.message": async (_input, output) => {
      const msg = output && output.message;
      const text =
        (output &&
          Array.isArray(output.parts) &&
          output.parts
            .filter((p) => p && p.type === "text" && typeof p.text === "string")
            .map((p) => p.text)
            .join("\n")) ||
        "";
      const res = runHook("UserPromptSubmit", {
        ...base(msg && msg.sessionID),
        prompt: text,
      });
      // Deliver UserPromptSubmit additionalContext (e.g. the find/trace
      // tool-mapping nudge) as an extra text part on the outgoing message.
      // Best-effort and fail-open: never let injection break the message.
      const ctx = hookCtx(res);
      if (ctx && output && Array.isArray(output.parts)) {
        try {
          output.parts.push({ type: "text", text: ctx, synthetic: true });
        } catch {
          // fail-open
        }
      }
    },

    event: async ({ event }) => {
      if (!event || typeof event.type !== "string") return;
      if (event.type === "session.created") {
        const info = event.properties && event.properties.info;
        // `event` has no output channel, so SessionStart additionalContext
        // (the routing guide) cannot be delivered here; the call still runs
        // for its side effects (session row, counters). Guide delivery for
        // opencode is the bench's prompt-stamp / a future system-transform.
        runHook("SessionStart", {
          ...base(info && info.id),
          source: "startup",
        });
      } else if (event.type === "session.compacted") {
        runHook("PreCompact", base(event.properties && event.properties.sessionID));
      }
    },
  };
};
