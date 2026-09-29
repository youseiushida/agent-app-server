/**
 * Tests of the approval gate against a mocked pi extension API.
 *
 * Run with Node.js 22.18 or later (type stripping is on by default):
 *   node --test crates/aas-adapter-pi/extension/aas-gate.test.ts
 * or, from this folder, `npm test`.
 */

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, test } from "node:test";

import gate from "./aas-gate.ts";

type Answer = { value: string } | { cancelled: true } | { abort: true };

interface Dialog {
	title: string;
	options: string[];
	request: { v: number; toolCallId: string; toolName: string; input: unknown };
}

/** A pi stand-in: records the dialogs and notifications the gate produces. */
class FakePi {
	handlers = new Map<string, (event: any, ctx: any) => Promise<unknown>>();
	commands = new Map<string, { description: string; handler: (args: string, ctx: any) => Promise<void> }>();
	dialogs: Dialog[] = [];
	notes: { message: string; type: string }[] = [];
	/** Answers given to the next dialogs, in order. */
	answers: Answer[] = [];

	on(event: string, handler: (event: any, ctx: any) => Promise<unknown>) {
		this.handlers.set(event, handler);
	}

	registerCommand(name: string, options: { description: string; handler: (args: string, ctx: any) => Promise<void> }) {
		assert.ok(!this.commands.has(name), `registered twice: ${name}`);
		this.commands.set(name, options);
	}

	/** Runs a command the way pi's `_tryExecuteExtensionCommand` does, with a command context. */
	async command(name: string, args: string, ctx: Partial<CommandCtx>) {
		const command = this.commands.get(name);
		assert.ok(command, `the gate registers /${name}`);
		const full: CommandCtx = {
			isIdle: () => true,
			reload: async () => assert.fail("unexpected reload"),
			fork: async () => assert.fail("unexpected fork"),
			...ctx,
			ui: { notify: (message: string, type: string) => this.notes.push({ message, type }) },
		};
		await command.handler(args, full);
	}

	/** Calls the gate's `tool_call` handler the way pi does. */
	async toolCall(toolName: string, input: unknown, toolCallId = `call_${this.dialogs.length + 1}`) {
		const handler = this.handlers.get("tool_call");
		assert.ok(handler, "the gate registers a tool_call handler");
		const abort = new AbortController();
		const ctx = {
			signal: abort.signal,
			ui: {
				select: async (title: string, options: string[], opts: { signal?: AbortSignal; timeout?: number }) => {
					assert.ok(title.startsWith("aas-gate:"), title);
					assert.equal(opts?.timeout, undefined, "gate dialogs have no timeout");
					assert.equal(opts?.signal, abort.signal, "the dialog follows the turn's abort signal");
					this.dialogs.push({ title, options, request: JSON.parse(title.slice("aas-gate:".length)) });
					const answer = this.answers.shift();
					assert.ok(answer, `no answer scripted for ${title}`);
					if ("abort" in answer) {
						// pi's RPC mode resolves a dialog with `undefined` when the signal fires.
						abort.abort();
						return undefined;
					}
					if ("cancelled" in answer) return undefined;
					return answer.value;
				},
				notify: (message: string, type: string) => this.notes.push({ message, type }),
			},
		};
		return handler({ type: "tool_call", toolCallId, toolName, input }, ctx);
	}

	/** Reports sent by the gate (`aas-gate:` notifications), decoded. */
	reports() {
		return this.notes.map((n) => {
			assert.ok(n.message.startsWith("aas-gate:"), n.message);
			return JSON.parse(n.message.slice("aas-gate:".length));
		});
	}
}

/** The parts of pi's command context the gate uses. */
interface CommandCtx {
	isIdle: () => boolean;
	reload: () => Promise<void>;
	fork: (entryId: string, options: { position: string }) => Promise<{ cancelled: boolean }>;
	ui: { notify: (message: string, type: string) => void };
}

let dir: string;
let modeFile: string;

function setMode(mode: string) {
	fs.writeFileSync(modeFile, JSON.stringify({ mode }));
}

function load(): FakePi {
	const pi = new FakePi();
	gate(pi);
	return pi;
}

const allow = (choice: string, feedback?: string): Answer => ({ value: JSON.stringify({ choice, feedback }) });

beforeEach(() => {
	dir = fs.mkdtempSync(path.join(os.tmpdir(), "aas-gate-"));
	modeFile = path.join(dir, "mode.json");
	process.env.AAS_PI_GATE_FILE = modeFile;
});

afterEach(() => {
	delete process.env.AAS_PI_GATE_FILE;
	fs.rmSync(dir, { recursive: true, force: true });
});

describe("modes", () => {
	test("auto never asks", async () => {
		setMode("auto");
		const pi = load();
		assert.equal(await pi.toolCall("bash", { command: "rm -rf x" }), undefined);
		assert.equal(await pi.toolCall("write", { path: "a.txt", content: "x" }), undefined);
		assert.equal(pi.dialogs.length, 0);
		assert.equal(pi.notes.length, 0);
	});

	test("ask asks for shell commands and edits, not for other tools", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [allow("allow"), allow("allow"), allow("allow")];
		assert.equal(await pi.toolCall("bash", { command: "echo hi" }, "c1"), undefined);
		assert.equal(await pi.toolCall("powershell", { command: "dir" }, "c2"), undefined);
		assert.equal(await pi.toolCall("edit", { path: "a.txt", oldText: "a", newText: "b" }, "c3"), undefined);
		assert.equal(await pi.toolCall("read", { path: "a.txt" }), undefined);
		assert.equal(await pi.toolCall("grep", { pattern: "x" }), undefined);
		assert.deepEqual(
			pi.dialogs.map((d) => [d.request.toolName, d.request.toolCallId]),
			[
				["bash", "c1"],
				["powershell", "c2"],
				["edit", "c3"],
			],
		);
		assert.deepEqual(pi.dialogs[0].options, ["allow", "allowSession", "deny"]);
		assert.deepEqual(pi.dialogs[0].request, { v: 1, toolCallId: "c1", toolName: "bash", input: { command: "echo hi" } });
	});

	test("askCommands asks for shell commands only", async () => {
		setMode("askCommands");
		const pi = load();
		pi.answers = [allow("allow")];
		assert.equal(await pi.toolCall("write", { path: "a.txt", content: "x" }), undefined);
		assert.equal(await pi.toolCall("edit", { path: "a.txt", oldText: "a", newText: "b" }), undefined);
		assert.equal(await pi.toolCall("bash", { command: "ls" }), undefined);
		assert.deepEqual(
			pi.dialogs.map((d) => d.request.toolName),
			["bash"],
		);
	});

	test("a missing, unreadable or invalid mode file fails closed (ask)", async () => {
		const pi = load();
		pi.answers = [allow("deny"), allow("deny"), allow("deny")];
		// No file.
		assert.ok(await pi.toolCall("edit", { path: "a", oldText: "a", newText: "b" }));
		// Not JSON.
		fs.writeFileSync(modeFile, "{not json");
		assert.ok(await pi.toolCall("edit", { path: "a", oldText: "a", newText: "b" }));
		// Unknown mode.
		setMode("yolo");
		assert.ok(await pi.toolCall("bash", { command: "ls" }));
		assert.equal(pi.dialogs.length, 3);
	});

	test("the mode is read on every call", async () => {
		setMode("auto");
		const pi = load();
		assert.equal(await pi.toolCall("bash", { command: "ls" }), undefined);
		setMode("ask");
		pi.answers = [allow("deny")];
		assert.ok(await pi.toolCall("bash", { command: "ls" }));
		assert.equal(pi.dialogs.length, 1);
	});
});

describe("answers", () => {
	test("deny blocks the tool, with the feedback when given", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [allow("deny"), allow("deny", "use the test script instead")];
		assert.deepEqual(await pi.toolCall("bash", { command: "rm x" }), {
			block: true,
			reason: "The user denied this tool call.",
		});
		assert.deepEqual(await pi.toolCall("bash", { command: "rm y" }), {
			block: true,
			reason: "The user denied this tool call: use the test script instead",
		});
	});

	test("a plain option string is accepted", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [{ value: "allow" }, { value: "deny" }];
		assert.equal(await pi.toolCall("bash", { command: "ls" }), undefined);
		assert.equal((await pi.toolCall("bash", { command: "ls -l" }) as any).block, true);
	});

	test("allowSession allows the same command again, and every edit", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [allow("allowSession"), allow("allow"), allow("allowSession")];
		assert.equal(await pi.toolCall("bash", { command: "npm test" }), undefined);
		assert.equal(await pi.toolCall("bash", { command: "npm test" }), undefined);
		// Another command still asks.
		assert.equal(await pi.toolCall("bash", { command: "npm run build" }), undefined);
		assert.equal(await pi.toolCall("write", { path: "a.txt", content: "x" }), undefined);
		assert.equal(await pi.toolCall("edit", { path: "b.txt", oldText: "a", newText: "b" }), undefined);
		assert.deepEqual(
			pi.dialogs.map((d) => d.request.toolName),
			["bash", "bash", "write"],
		);
	});

	test("a malformed answer blocks the tool", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [{ value: "{broken" }, { value: JSON.stringify({ choice: 3 }) }];
		assert.equal((await pi.toolCall("bash", { command: "ls" }) as any).reason, "The approval request was cancelled.");
		assert.equal((await pi.toolCall("bash", { command: "ls" }) as any).reason, "The approval request was cancelled.");
	});
});

describe("dialog closure reports", () => {
	test("an answered dialog is reported as answered", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [allow("allow"), { cancelled: true }];
		await pi.toolCall("bash", { command: "ls" }, "c1");
		// A dialog dismissed by agent-app-server (`cancelled: true`) was answered by it too.
		assert.deepEqual(await pi.toolCall("bash", { command: "ls" }, "c2"), {
			block: true,
			reason: "The approval request was cancelled.",
		});
		assert.deepEqual(pi.reports(), [
			{ v: 1, event: "dialogClosed", toolCallId: "c1", reason: "answered" },
			{ v: 1, event: "dialogClosed", toolCallId: "c2", reason: "answered" },
		]);
		assert.ok(pi.notes.every((n) => n.type === "info"));
	});

	test("a dialog closed by the turn's abort is reported as aborted and blocks the tool", async () => {
		setMode("ask");
		const pi = load();
		pi.answers = [{ abort: true }];
		assert.deepEqual(await pi.toolCall("bash", { command: "sleep 100" }, "c9"), {
			block: true,
			reason: "The approval request was cancelled.",
		});
		assert.deepEqual(pi.reports(), [{ v: 1, event: "dialogClosed", toolCallId: "c9", reason: "aborted" }]);
	});

	test("tools that are not gated produce no report", async () => {
		setMode("ask");
		const pi = load();
		await pi.toolCall("read", { path: "a" });
		setMode("auto");
		await pi.toolCall("bash", { command: "ls" });
		assert.equal(pi.notes.length, 0);
	});
});

describe("commands", () => {
	test("/reload runs pi's reload while the agent is idle, and nothing after it", async () => {
		const pi = load();
		assert.equal(pi.commands.get("reload")?.description, "Reload extensions, skills, prompts, themes, and context files");
		let reloads = 0;
		await pi.command("reload", "", {
			reload: async () => {
				reloads += 1;
			},
		});
		assert.equal(reloads, 1);
		assert.equal(pi.notes.length, 0, "the stale context is not used after the reload");
	});

	test("/reload refuses while the agent is busy, with pi's own words", async () => {
		const pi = load();
		await pi.command("reload", "", { isIdle: () => false });
		assert.deepEqual(pi.notes, [{ message: "Wait for the current response to finish before reloading.", type: "warning" }]);
	});

	test("/aas-gate-fork forks at the entry with the position, and reports nothing once it happened", async () => {
		const pi = load();
		const forks: [string, string][] = [];
		const fork = async (entryId: string, options: { position: string }) => {
			forks.push([entryId, options.position]);
			return { cancelled: false };
		};
		await pi.command("aas-gate-fork", "41c51822 at", { fork });
		await pi.command("aas-gate-fork", " 343ba349   before ", { fork });
		assert.deepEqual(forks, [
			["41c51822", "at"],
			["343ba349", "before"],
		]);
		assert.equal(pi.notes.length, 0);
	});

	test("/aas-gate-fork reports why it did not fork", async () => {
		const pi = load();
		await pi.command("aas-gate-fork", "nope at", {
			fork: async () => {
				throw new Error("Invalid entry ID for forking");
			},
		});
		await pi.command("aas-gate-fork", "41c51822 at", { fork: async () => ({ cancelled: true }) });
		await pi.command("aas-gate-fork", "41c51822 sideways", {});
		await pi.command("aas-gate-fork", "", {});
		assert.deepEqual(
			pi.reports().map((r) => [r.event, r.error]),
			[
				["forkFailed", "Invalid entry ID for forking"],
				["forkFailed", "an extension cancelled the fork"],
				["forkFailed", 'usage: /aas-gate-fork <entryId> <at|before>, got "41c51822 sideways"'],
				["forkFailed", 'usage: /aas-gate-fork <entryId> <at|before>, got ""'],
			],
		);
		assert.ok(pi.reports().every((r) => r.v === 1));
	});
});
