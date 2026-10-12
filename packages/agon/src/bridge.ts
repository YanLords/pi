import { type ChildProcess, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const MAX_FRAME_SIZE = 8 * 1024 * 1024; // 8 MiB
const packageRoot = fileURLToPath(new URL("..", import.meta.url));

const OPERATION_TIMEOUTS: Record<string, number> = {
	authorize: 2_000,
	hello: 5_000,
	status: 5_000,
	verify: 310_000,
	repair: 900_000,
	shutdown: 2_000,
};

export interface AgonHelloResult {
	protocol: number;
	version: string;
	capabilities: string[];
	agon_version: string;
}

export interface AgonStatusResult {
	form_id: string;
	lock_hash: string;
	active_checks: string[];
	budget: Record<string, unknown>;
	registry_size: number;
	tampering_violations: string[];
}

export interface AgonAuthorizeResult {
	verdict: "allow" | "confirm" | "deny";
	reason?: string;
}

export interface AgonVerifyResult {
	check_id: string;
	passed: boolean;
	exit_code: number | null;
	timed_out: boolean;
	stdout: string;
	stderr: string;
	signature: {
		class: string;
		fields: Record<string, unknown>;
	};
	evidence: Record<string, unknown>;
}

export type AgonHumanReason =
	| { reason: "jev_unavailable"; message: string }
	| { reason: "budget_exhausted"; remaining_mutations: number; max_mutations: number }
	| { reason: "low_confidence"; confidence: number; threshold: number }
	| { reason: "no_candidate_available" }
	| Record<string, unknown>
	| string;

export interface AgonRepairResult {
	outcome: "Passed" | "Flaky" | "NoChange" | "Fixed" | "HumanRequired";
	mutation?: string;
	proof?: Record<string, unknown>;
	form?: string;
	why?: AgonHumanReason;
}

export interface AgonBridgeOptions {
	cwd: string;
	daemonPath?: string;
	env?: NodeJS.ProcessEnv;
	onLog?: (line: string) => void;
	onEvent?: (event: string, data: unknown) => void;
}

interface PendingRequest {
	resolve: (value: unknown) => void;
	reject: (error: Error) => void;
	signal?: AbortSignal;
	onAbort?: () => void;
	timer?: NodeJS.Timeout | null;
}

interface DaemonErrorResponse {
	id: number;
	ok: false;
	error: {
		code: string;
		message: string;
	};
}

interface DaemonOkResponse {
	id: number;
	ok: true;
	result: unknown;
}

type DaemonResponse = DaemonOkResponse | DaemonErrorResponse;

interface DaemonEventFrame {
	event: string;
	data: unknown;
}

function isEventFrame(value: unknown): value is DaemonEventFrame {
	return (
		typeof value === "object" &&
		value !== null &&
		"event" in value &&
		typeof (value as DaemonEventFrame).event === "string" &&
		!("id" in value)
	);
}

function isDaemonResponse(value: unknown): value is DaemonResponse {
	return (
		typeof value === "object" &&
		value !== null &&
		"id" in value &&
		typeof (value as { id: unknown }).id === "number" &&
		"ok" in value &&
		typeof (value as { ok: unknown }).ok === "boolean"
	);
}

export function killDaemonTree(child: ChildProcess | null): void {
	if (!child || !child.pid || child.killed) return;
	const pid = child.pid;
	if (process.platform === "win32") {
		try {
			spawn("taskkill", ["/pid", String(pid), "/T", "/F"], { stdio: "ignore" });
		} catch {
			try {
				child.kill();
			} catch {
				// Ignore
			}
		}
	} else {
		try {
			process.kill(-pid, "SIGKILL");
		} catch {
			try {
				child.kill("SIGKILL");
			} catch {
				// Ignore
			}
		}
	}
}

export function resolveAgonDaemon(): string {
	if (
		process.env.AGON_ALLOW_DAEMON_OVERRIDE === "1" &&
		process.env.AGON_DAEMON &&
		existsSync(process.env.AGON_DAEMON)
	) {
		return process.env.AGON_DAEMON;
	}

	const platform = process.platform === "win32" ? "windows" : process.platform;
	const arch = process.arch;
	const exeName = platform === "windows" ? "agon.exe" : "agon";
	const packaged = join(packageRoot, "bin", `agon-${platform}-${arch}`, exeName);
	if (existsSync(packaged)) {
		return packaged;
	}

	const releaseBuild = join(
		packageRoot,
		"daemon",
		"target",
		"release",
		platform === "windows" ? "agon-bridge.exe" : "agon-bridge",
	);
	if (existsSync(releaseBuild)) {
		return releaseBuild;
	}

	const debugBuild = join(
		packageRoot,
		"daemon",
		"target",
		"debug",
		platform === "windows" ? "agon-bridge.exe" : "agon-bridge",
	);
	if (existsSync(debugBuild)) {
		return debugBuild;
	}

	throw new Error('Agon daemon binary not found. Run "npm run build:daemon" to compile it.');
}

export function encodeFrame(payload: unknown): Buffer {
	const json = Buffer.from(JSON.stringify(payload), "utf8");
	if (json.length > MAX_FRAME_SIZE) {
		throw new Error(`Frame payload exceeds maximum size of ${MAX_FRAME_SIZE} bytes`);
	}
	const header = Buffer.alloc(4);
	header.writeUInt32BE(json.length, 0);
	return Buffer.concat([header, json]);
}

export class AgonBridge {
	readonly cwd: string;
	readonly daemonPath: string;
	readonly env: NodeJS.ProcessEnv | undefined;
	readonly onLog: ((line: string) => void) | undefined;
	readonly onEvent: ((event: string, data: unknown) => void) | undefined;
	private process: ChildProcess | null = null;
	private nextId = 1;
	private pendingRequests = new Map<number, PendingRequest>();
	private incomingChunks: Buffer[] = [];
	private incomingTotalLength = 0;
	private isClosed = false;
	private repairEventListeners = new Set<(event: string, data: unknown) => void>();
	private startPromise: Promise<void> | null = null;

	constructor(options: AgonBridgeOptions) {
		this.cwd = options.cwd;
		this.daemonPath = options.daemonPath ?? resolveAgonDaemon();
		this.env = options.env;
		this.onLog = options.onLog;
		this.onEvent = options.onEvent;
	}

	get isRunning(): boolean {
		return !this.isClosed && this.process !== null;
	}

	async start(): Promise<void> {
		if (this.process && !this.isClosed) return;
		if (this.startPromise) return this.startPromise;

		this.startPromise = (async () => {
			this.isClosed = false;
			this.incomingChunks = [];
			this.incomingTotalLength = 0;

			const child = spawn(this.daemonPath, ["--root", this.cwd], {
				cwd: this.cwd,
				env: { ...process.env, ...this.env },
				stdio: ["pipe", "pipe", "pipe"],
				shell: false,
				detached: process.platform !== "win32",
			});

			this.process = child;

			child.stdout?.on("data", (chunk: Buffer) => {
				this.handleIncomingData(chunk);
			});

			child.stdout?.on("end", () => {
				if (!this.isClosed && this.pendingRequests.size > 0) {
					this.handleProcessTermination(new Error("Agon daemon bridge stream ended unexpectedly (EOF)"));
				}
			});

			child.stderr?.on("data", (chunk: Buffer) => {
				const text = chunk.toString("utf8");
				this.onLog?.(text);
			});

			child.on("error", (err) => {
				this.handleProcessTermination(err);
			});

			child.on("exit", (code, signal) => {
				const err = new Error(
					`Agon daemon exited unexpectedly with code ${code ?? "unknown"} and signal ${signal ?? "none"}`,
				);
				this.handleProcessTermination(err);
			});

			await this.hello();
		})().finally(() => {
			this.startPromise = null;
		});

		return this.startPromise;
	}

	private peekFrameHeader(): number {
		const first = this.incomingChunks[0];
		if (first.length >= 4) {
			return first.readUInt32BE(0);
		}
		const headerBuf = Buffer.allocUnsafe(4);
		let offset = 0;
		for (const chunk of this.incomingChunks) {
			const toCopy = Math.min(4 - offset, chunk.length);
			chunk.copy(headerBuf, offset, 0, toCopy);
			offset += toCopy;
			if (offset === 4) break;
		}
		return headerBuf.readUInt32BE(0);
	}

	private consumeFrame(frameLen: number): Buffer {
		const totalNeeded = 4 + frameLen;
		this.incomingTotalLength -= totalNeeded;

		if (this.incomingChunks[0].length >= totalNeeded) {
			const first = this.incomingChunks[0];
			const payload = first.subarray(4, totalNeeded);
			const remainder = first.subarray(totalNeeded);
			if (remainder.length > 0) {
				this.incomingChunks[0] = remainder;
			} else {
				this.incomingChunks.shift();
			}
			return payload;
		}

		const frameBuffer = Buffer.allocUnsafe(frameLen);
		let skippedHeader = 0;
		let written = 0;

		while (this.incomingChunks.length > 0 && written < frameLen) {
			const chunk = this.incomingChunks[0];
			let chunkOffset = 0;

			if (skippedHeader < 4) {
				const skip = Math.min(4 - skippedHeader, chunk.length);
				skippedHeader += skip;
				chunkOffset += skip;
			}

			if (chunkOffset < chunk.length && written < frameLen) {
				const toCopy = Math.min(frameLen - written, chunk.length - chunkOffset);
				chunk.copy(frameBuffer, written, chunkOffset, chunkOffset + toCopy);
				written += toCopy;
				chunkOffset += toCopy;
			}

			if (chunkOffset === chunk.length) {
				this.incomingChunks.shift();
			} else {
				this.incomingChunks[0] = chunk.subarray(chunkOffset);
				break;
			}
		}

		return frameBuffer;
	}

	private handleIncomingData(chunk: Buffer): void {
		this.incomingChunks.push(chunk);
		this.incomingTotalLength += chunk.length;

		while (true) {
			if (this.incomingTotalLength < 4) {
				return;
			}

			const frameLen = this.peekFrameHeader();
			if (frameLen > MAX_FRAME_SIZE) {
				const err = new Error(`Frame size ${frameLen} exceeds maximum limit of ${MAX_FRAME_SIZE} bytes`);
				this.handleProcessTermination(err);
				return;
			}

			if (this.incomingTotalLength < 4 + frameLen) {
				return;
			}

			const frameBytes = this.consumeFrame(frameLen);

			try {
				const parsed: unknown = JSON.parse(frameBytes.toString("utf8"));
				if (isEventFrame(parsed)) {
					this.onEvent?.(parsed.event, parsed.data);
					for (const listener of this.repairEventListeners) {
						listener(parsed.event, parsed.data);
					}
				} else if (isDaemonResponse(parsed)) {
					this.handleResponse(parsed);
				} else {
					const err = new Error("Agon protocol violation: unrecognized frame payload");
					this.handleProcessTermination(err);
					return;
				}
			} catch (e) {
				const err = new Error(`Failed to decode daemon message: ${e instanceof Error ? e.message : String(e)}`);
				this.handleProcessTermination(err);
				return;
			}
		}
	}

	private handleResponse(response: DaemonResponse): void {
		if (typeof response.id !== "number" || !this.pendingRequests.has(response.id)) {
			const err = new Error(
				`Agon protocol violation: received response with unexpected or duplicate request ID ${String(response.id)}`,
			);
			this.handleProcessTermination(err);
			return;
		}

		const pending = this.pendingRequests.get(response.id)!;
		this.pendingRequests.delete(response.id);
		if (pending.timer) {
			clearTimeout(pending.timer);
			pending.timer = null;
		}
		if (pending.onAbort && pending.signal) {
			pending.signal.removeEventListener("abort", pending.onAbort);
		}

		if (response.ok) {
			pending.resolve(response.result);
		} else {
			const err = new Error(`${response.error.code}: ${response.error.message}`);
			pending.reject(err);
		}
	}

	private handleProcessTermination(error: Error): void {
		if (this.isClosed) return;
		this.isClosed = true;

		for (const pending of this.pendingRequests.values()) {
			if (pending.timer) {
				clearTimeout(pending.timer);
				pending.timer = null;
			}
			if (pending.onAbort && pending.signal) {
				pending.signal.removeEventListener("abort", pending.onAbort);
			}
			pending.reject(error);
		}
		this.pendingRequests.clear();
		this.repairEventListeners.clear();
		this.incomingChunks = [];
		this.incomingTotalLength = 0;

		if (this.process) {
			killDaemonTree(this.process);
			this.process = null;
		}
	}

	async request<T>(
		op: string,
		params: Record<string, unknown> = {},
		signal?: AbortSignal,
		timeoutMs?: number,
	): Promise<T> {
		if (this.isClosed || !this.process?.stdin) {
			throw new Error("Agon daemon bridge is closed or not running");
		}

		if (signal?.aborted) {
			throw new Error("Operation aborted");
		}

		const id = this.nextId++;
		const frame = encodeFrame({ id, op, ...params });

		return new Promise<T>((resolve, reject) => {
			const pending: PendingRequest = {
				resolve: (val) => resolve(val as T),
				reject,
				signal,
			};

			const timeoutDuration = timeoutMs ?? OPERATION_TIMEOUTS[op] ?? 30_000;
			if (timeoutDuration > 0 && Number.isFinite(timeoutDuration)) {
				const timer = setTimeout(() => {
					pending.timer = null;
					this.pendingRequests.delete(id);
					const timeoutErr = new Error(`Agon operation '${op}' timed out after ${timeoutDuration}ms`);
					this.handleProcessTermination(timeoutErr);
					reject(timeoutErr);
				}, timeoutDuration);
				timer.unref?.();
				pending.timer = timer;
			}

			if (signal) {
				const onAbort = () => {
					if (pending.timer) {
						clearTimeout(pending.timer);
						pending.timer = null;
					}
					this.pendingRequests.delete(id);
					reject(new Error("Operation aborted"));
				};
				pending.onAbort = onAbort;
				signal.addEventListener("abort", onAbort, { once: true });
			}

			this.pendingRequests.set(id, pending);

			try {
				this.process?.stdin?.write(frame, (err) => {
					if (err) {
						if (pending.timer) {
							clearTimeout(pending.timer);
							pending.timer = null;
						}
						this.pendingRequests.delete(id);
						reject(err);
					}
				});
			} catch (err) {
				if (pending.timer) {
					clearTimeout(pending.timer);
					pending.timer = null;
				}
				this.pendingRequests.delete(id);
				reject(err instanceof Error ? err : new Error(String(err)));
			}
		});
	}

	async hello(): Promise<AgonHelloResult> {
		return this.request<AgonHelloResult>("hello");
	}

	async status(): Promise<AgonStatusResult> {
		return this.request<AgonStatusResult>("status");
	}

	async authorize(action: string, detail: string): Promise<AgonAuthorizeResult> {
		return this.request<AgonAuthorizeResult>("authorize", { action, detail });
	}

	async verify(checkId?: string, signal?: AbortSignal): Promise<AgonVerifyResult[]> {
		const res = await this.request<{ results?: AgonVerifyResult[] } | AgonVerifyResult>(
			"verify",
			checkId ? { check_id: checkId } : {},
			signal,
		);
		if ("results" in res && Array.isArray(res.results)) {
			return res.results;
		}
		return [res as AgonVerifyResult];
	}

	async repair(
		checkId: string,
		onEvent?: (event: string, data: unknown) => void,
		signal?: AbortSignal,
	): Promise<AgonRepairResult> {
		if (onEvent) {
			this.repairEventListeners.add(onEvent);
		}
		try {
			return await this.request<AgonRepairResult>("repair", { check_id: checkId }, signal);
		} finally {
			if (onEvent) {
				this.repairEventListeners.delete(onEvent);
			}
		}
	}

	async shutdown(): Promise<void> {
		if (this.isClosed || !this.process) return;
		try {
			await this.request("shutdown");
		} catch {
			// Ignore error or timeout on shutdown
		} finally {
			this.close();
		}
	}

	close(): void {
		this.isClosed = true;
		for (const pending of this.pendingRequests.values()) {
			if (pending.timer) {
				clearTimeout(pending.timer);
				pending.timer = null;
			}
			if (pending.onAbort && pending.signal) {
				pending.signal.removeEventListener("abort", pending.onAbort);
			}
			pending.reject(new Error("Agon daemon bridge closed"));
		}
		this.pendingRequests.clear();
		this.repairEventListeners.clear();
		this.incomingChunks = [];
		this.incomingTotalLength = 0;

		if (this.process) {
			killDaemonTree(this.process);
			this.process = null;
		}
	}
}
