import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { agonExtension } from "./extension.ts";

export default function (pi: ExtensionAPI): void {
	agonExtension(pi);
}

export { AgonBridge, resolveAgonDaemon } from "./bridge.ts";
export { agonExtension } from "./extension.ts";
