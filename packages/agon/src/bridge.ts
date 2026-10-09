import { type ChildProcess, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const MAX_FRAME_SIZE = 8 * 1024 * 1024; // 8 MiB
const packageRoot = fileURLToPath(new URL("..", import.meta.url));

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

export interface AgonRepairResult {
	outcome: "Passed" | "Flaky" | "NoChange" | "Fixed" | "HumanRequired";
	mutation?: string;
	proof?: Record<string, unknown>;
	form?: string;
	why?: string;
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

export function resolveAgonDaemon(): string {
	if (process.env.AGON_DAEMON && existsSync(process.env.AGON_DAEMON)) {
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
	private process: ChildProcess | null;
	private nextId: number;
	private pendingRequests: Map<number, PendingRequest>;
	private incomingBuffer: Buffer;
	private isClosed: boolean;
	private repairEventListeners: Set<(event: string, data: unknown) => void>;

	constructor(options: AgonBridgeOptions) {
		this.cwd = options.cwd;
		this.daemonPath = options.daemonPath ?? resolveAgonDaemon();
		this.env = options.env;
		this.onLog = options.onLog;
		this.onEvent = options.onEvent;
		this.process = null;
		this.nextId = 1;
		this.pendingRequests = new Map();
		this.incomingBuffer = Buffer.alloc(0);
		this.isClosed = false;
		this.repairEventListeners = new Set();
	}

	async start(): Promise<void> {
		if (this.process) return;
		this.isClosed = false;

		const child = spawn(this.daemonPath, ["--root", this.cwd], {
			cwd: this.cwd,
			env: { ...process.env, ...this.env },
			stdio: ["pipe", "pipe", "pipe"],
			shell: false,
		});

		this.process = child;

		child.stdout?.on("data", (chunk: Buffer) => {
			this.handleIncomingData(chunk);
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
	}

	private handleIncomingData(chunk: Buffer): void {
		this.incomingBuffer = Buffer.concat([this.incomingBuffer, chunk]);

		while (this.incomingBuffer.length >= 4) {
			const frameLen = this.incomingBuffer.readUInt32BE(0);
			if (frameLen > MAX_FRAME_SIZE) {
				const err = new Error(`Frame size ${frameLen} exceeds maximum limit of ${MAX_FRAME_SIZE} bytes`);
				this.handleProcessTermination(err);
				return;
			}

			if (this.incomingBuffer.length < 4 + frameLen) {
				return;
			}

			const frameBytes = this.incomingBuffer.subarray(4, 4 + frameLen);
			this.incomingBuffer = this.incomingBuffer.subarray(4 + frameLen);

			try {
				const parsed: unknown = JSON.parse(frameBytes.toString("utf8"));
				if (isEventFrame(parsed)) {
					this.onEvent?.(parsed.event, parsed.data);
					for (const listener of this.repairEventListeners) {
						listener(parsed.event, parsed.data);
					}
				} else {
					this.handleResponse(parsed as DaemonResponse);
				}
			} catch (e) {
				const err = new Error(`Failed to decode daemon message: ${e instanceof Error ? e.message : String(e)}`);
				this.handleProcessTermination(err);
				return;
			}
		}
	}

	private handleResponse(response: DaemonResponse): void {
		const pending = this.pendingRequests.get(response.id);
		if (!pending) return;

		this.pendingRequests.delete(response.id);
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
			if (pending.onAbort && pending.signal) {
				pending.signal.removeEventListener("abort", pending.onAbort);
			}
			pending.reject(error);
		}
		this.pendingRequests.clear();
		this.repairEventListeners.clear();

		if (this.process) {
			try {
				this.process.kill();
			} catch {
				// Ignore
			}
			this.process = null;
		}
	}

	async request<T>(op: string, params: Record<string, unknown> = {}, signal?: AbortSignal): Promise<T> {
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

			if (signal) {
				const onAbort = () => {
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
						this.pendingRequests.delete(id);
						reject(err);
					}
				});
			} catch (err) {
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

	async verify(checkId?: string, signal?: AbortSignal): Promise<AgonVerifyResult> {
		return this.request<AgonVerifyResult>("verify", checkId ? { check_id: checkId } : {}, signal);
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
			// Ignore error on shutdown
		} finally {
			this.close();
		}
	}

	close(): void {
		this.isClosed = true;
		if (this.process) {
			try {
				this.process.kill();
			} catch {
				// Ignore
			}
			this.process = null;
		}
		for (const pending of this.pendingRequests.values()) {
			pending.reject(new Error("Agon daemon bridge closed"));
		}
		this.pendingRequests.clear();
		this.repairEventListeners.clear();
	}
}
