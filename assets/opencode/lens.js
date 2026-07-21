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

  return {
    "tool.execute.before": async (input, output) => {
      const res = runHook("PreToolUse", {
        ...base(input.sessionID),
        tool_name: toolName(input.tool),
        tool_input: output.args ?? {},
      });
      const out = res && res.hookSpecificOutput;
      if (!out) return;
      if (out.permissionDecision === "deny") {
        throw new Error(out.permissionDecisionReason || "denied by lens routing");
      }
      if (out.permissionDecision === "allow" && out.updatedInput && output.args) {
        Object.assign(output.args, out.updatedInput);
      }
      // additionalContext has no injection channel here; dropped.
    },

    "tool.execute.after": async (input, output) => {
      runHook("PostToolUse", {
        ...base(input.sessionID),
        tool_name: toolName(input.tool),
        tool_input: {},
        tool_response: output && output.output != null ? output.output : null,
      });
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
      runHook("UserPromptSubmit", {
        ...base(msg && msg.sessionID),
        prompt: text,
      });
    },

    event: async ({ event }) => {
      if (!event || typeof event.type !== "string") return;
      if (event.type === "session.created") {
        const info = event.properties && event.properties.info;
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
