/**
 * aas-gate — approval gate installed by agent-app-server.
 *
 * pi never asks before running tools. This extension adds approvals for the tools that
 * change the machine, and asks through `ctx.ui.select`, which pi's RPC mode forwards to
 * agent-app-server as an `extension_ui_request`.
 *
 * Protocol with the adapter (both sides are ours; nothing here is inferred):
 * - The permission mode is read from the JSON file named by AAS_PI_GATE_FILE on every
 *   tool call: {"mode": "ask" | "askCommands" | "auto"}. A missing or unreadable file means
 *   "ask" (fail closed).
 * - The dialog title is `aas-gate:` followed by a JSON object
 *   {"v":1,"toolCallId":…,"toolName":…,"input":…}. The dialog has no timeout: it waits for
 *   the user (agent-app-server's own interaction lifecycle decides when it expires).
 * - The answer (`value`) is a JSON object {"choice": "allow"|"allowSession"|"deny",
 *   "feedback"?: string}. A plain option string is accepted too. A cancelled or aborted
 *   dialog blocks the tool.
 * - When the dialog closes, the gate reports how through `ctx.ui.notify` with the message
 *   `aas-gate:` + {"v":1,"event":"dialogClosed","toolCallId":…,"reason":"answered"|"aborted"}.
 *   "aborted" means pi closed the dialog because the turn was aborted (its abort signal
 *   fired), so no answer from agent-app-server was used. pi's RPC mode does not tell the
 *   client when it closes a dialog by itself; this report is how the adapter learns it.
 *
 * "allowSession" allows the exact same shell command, or every edit/write, for the rest of
 * this pi process.
 */

import * as fs from "node:fs";

type Mode = "ask" | "askCommands" | "auto";

const SHELL_TOOLS = new Set(["bash", "powershell"]);
const EDIT_TOOLS = new Set(["edit", "write"]);
const TITLE_PREFIX = "aas-gate:";
const PROTOCOL_VERSION = 1;

function readMode(): Mode {
	const file = process.env.AAS_PI_GATE_FILE;
	if (!file) return "ask";
	try {
		const parsed = JSON.parse(fs.readFileSync(file, "utf8"));
		if (parsed && (parsed.mode === "ask" || parsed.mode === "askCommands" || parsed.mode === "auto")) {
			return parsed.mode;
		}
	} catch {
		// fall through: fail closed
	}
	return "ask";
}

function parseAnswer(value: unknown): { choice?: string; feedback?: string } {
	if (typeof value !== "string") return {};
	if (value.startsWith("{")) {
		try {
			const parsed = JSON.parse(value);
			return {
				choice: typeof parsed.choice === "string" ? parsed.choice : undefined,
				feedback: typeof parsed.feedback === "string" && parsed.feedback.length > 0 ? parsed.feedback : undefined,
			};
		} catch {
			return {};
		}
	}
	return { choice: value };
}

export default function (pi: any) {
	const allowedCommands = new Set<string>();
	let editsAllowed = false;

	pi.on("tool_call", async (event: any, ctx: any) => {
		const mode = readMode();
		if (mode === "auto") return undefined;

		const toolName = String(event.toolName);
		const isShell = SHELL_TOOLS.has(toolName);
		const isEdit = EDIT_TOOLS.has(toolName);
		if (!isShell && !(isEdit && mode === "ask")) return undefined;

		const command = isShell ? String(event.input?.command ?? "") : "";
		if (isShell && allowedCommands.has(command)) return undefined;
		if (isEdit && editsAllowed) return undefined;

		const request = { v: PROTOCOL_VERSION, toolCallId: event.toolCallId, toolName, input: event.input };
		const answer = await ctx.ui.select(TITLE_PREFIX + JSON.stringify(request), ["allow", "allowSession", "deny"], {
			signal: ctx.signal,
		});
		const aborted = ctx.signal?.aborted === true;
		ctx.ui.notify(
			TITLE_PREFIX +
				JSON.stringify({
					v: PROTOCOL_VERSION,
					event: "dialogClosed",
					toolCallId: event.toolCallId,
					reason: aborted ? "aborted" : "answered",
				}),
			"info",
		);
		// pi resolves an aborted dialog with `undefined`; the check only makes that explicit.
		const { choice, feedback }: { choice?: string; feedback?: string } = aborted ? {} : parseAnswer(answer);

		if (choice === "allow") return undefined;
		if (choice === "allowSession") {
			if (isShell) allowedCommands.add(command);
			else editsAllowed = true;
			return undefined;
		}
		const reason =
			choice === "deny"
				? feedback
					? `The user denied this tool call: ${feedback}`
					: "The user denied this tool call."
				: "The approval request was cancelled.";
		return { block: true, reason };
	});
}
