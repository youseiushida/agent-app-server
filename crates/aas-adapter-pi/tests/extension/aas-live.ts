// Test extension for the live tests of `aas-adapter-pi` (and for recording the replay fixtures
// `tests/fixtures/agent_*.jsonl`). It makes pi do what extensions do on their own, through pi's
// public extension API (pi 0.85.1, docs/extensions.md): start runs by itself
// (`pi.sendMessage(..., { triggerTurn: true })`, `pi.sendUserMessage`), ask outside any run
// (`ctx.ui.confirm` from a timer), start a run from an `agent_settled` handler, and start a run
// of its own while pi is still checking a prompt (the adapter's race with a busy agent).
//
// Commands (all return at once; what they start happens later, outside the command's turn):
//   /aas-later <ms> [text]       after <ms>, a displayed custom message that triggers a run
//   /aas-later-user <ms> [text]  after <ms>, a user message sent by the extension (a run)
//   /aas-ask-later <ms>          after <ms>, a confirm dialog; the answer is reported by notify
//   /aas-chain [text]            the next `agent_settled` starts another run
// Input:
//   a prompt starting with `aas-race` starts a run of the extension before pi checks whether it
//   is busy, so pi refuses the prompt because that run is going on.

export default function (pi: any) {
	const custom = (text: string) =>
		pi.sendMessage({ customType: "aas-live", content: text, display: true }, { triggerTurn: true });

	const parse = (args: string, fallback: string): [number, string] => {
		const [ms, ...rest] = args.trim().split(/\s+/);
		return [Number(ms) || 0, rest.join(" ") || fallback];
	};

	pi.registerCommand("aas-later", {
		description: "aas live test: start a run by itself later (custom message)",
		handler: async (args: string) => {
			const [ms, text] = parse(args, "Reply with exactly: WOKE");
			setTimeout(() => custom(text), ms);
		},
	});

	pi.registerCommand("aas-later-user", {
		description: "aas live test: send a user message by itself later",
		handler: async (args: string) => {
			const [ms, text] = parse(args, "Reply with exactly: WOKE-USER");
			setTimeout(() => {
				pi.sendUserMessage(text);
			}, ms);
		},
	});

	pi.registerCommand("aas-ask-later", {
		description: "aas live test: ask outside any run later",
		handler: async (args: string, ctx: any) => {
			const [ms] = parse(args, "");
			setTimeout(async () => {
				const answer = await ctx.ui.confirm("aas-live", "A dialog outside a turn");
				ctx.ui.notify(`aas-live answered: ${answer}`, "info");
			}, ms);
		},
	});

	let chain: string | undefined;
	pi.registerCommand("aas-chain", {
		description: "aas live test: the next agent_settled starts another run",
		handler: async (args: string) => {
			chain = args.trim() || "Reply with exactly: CHAINED";
		},
	});
	pi.on("agent_settled", async () => {
		if (chain !== undefined) {
			const text = chain;
			chain = undefined;
			custom(text);
		}
	});

	// The race. `turn_start` is emitted after pi has written the run's `agent_start` (pi awaits
	// every listener of `agent_start` first), so pi's refusal of the prompt comes after it.
	let running: (() => void) | undefined;
	pi.on("turn_start", async () => {
		running?.();
		running = undefined;
	});
	pi.on("input", async (event: any) => {
		if (typeof event.text === "string" && event.text.startsWith("aas-race")) {
			const started = new Promise<void>((resolve) => {
				running = resolve;
			});
			custom("Reply with exactly: RACED");
			await started;
		}
		return { action: "continue" };
	});
}
