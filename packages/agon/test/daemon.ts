import { resolveAgonDaemon } from "../src/bridge.ts";

export function getTestDaemon(): string {
	return resolveAgonDaemon();
}
