/**
 * Cascade omp extension: host a reduced collab room on the configured relay
 * and register the session with cascaded so the GTK client can join.
 */
import * as os from "node:os";
import { timingSafeEqual } from "node:crypto";
import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

const DEFAULT_CASCADE_URL = "https://wickrunner.com:7701";
const DEFAULT_RELAY = "wss://wickrunner.com:8443";
const COLLAB_PROTO = 3;
const ROOM_KEY_BYTES = 32;
const WRITE_TOKEN_BYTES = 16;
const ROOM_ID_BYTES = 16;
const ENVELOPE_HEADER_LENGTH = 4;
const AES_ALGORITHM = "AES-GCM";
const IV_LENGTH = 12;
const HOST_PEER_BROADCAST = 0;
const CONNECT_TIMEOUT_MS = 15_000;
const BACKOFF_BASE_MS = 1_000;
const BACKOFF_MAX_MS = 30_000;
const FATAL_CLOSE: Record<number, string> = {
	4001: "room closed",
	4004: "no such room",
	4009: "a host is already connected for this room",
	4029: "room is full",
};

const FORWARD_EVENTS = [
	"message_start",
	"message_update",
	"message_end",
	"tool_execution_start",
	"tool_execution_update",
	"tool_execution_end",
	"turn_start",
	"turn_end",
	"agent_start",
	"agent_end",
] as const;

type CollabFrame = Record<string, unknown> & { t: string };

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

function envTruthy(value: string | undefined): boolean {
	if (!value) return false;
	const v = value.trim().toLowerCase();
	return v !== "" && v !== "0" && v !== "false" && v !== "no" && v !== "off";
}

function b64url(bytes: Uint8Array): string {
	return Buffer.from(bytes).toString("base64url");
}

function randomBytes(n: number): Uint8Array {
	const out = new Uint8Array(n);
	crypto.getRandomValues(out);
	return out;
}

function asStrict(bytes: Uint8Array): Uint8Array<ArrayBuffer> {
	if (bytes.buffer instanceof ArrayBuffer && bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength) {
		return bytes as Uint8Array<ArrayBuffer>;
	}
	const copy = new Uint8Array(bytes.byteLength);
	copy.set(bytes);
	return copy;
}

async function importRoomKey(raw: Uint8Array): Promise<CryptoKey> {
	return crypto.subtle.importKey("raw", asStrict(raw), AES_ALGORITHM, false, ["encrypt", "decrypt"]);
}

async function seal(key: CryptoKey, frame: CollabFrame): Promise<Uint8Array> {
	const iv = new Uint8Array(IV_LENGTH);
	crypto.getRandomValues(iv);
	const plaintext = textEncoder.encode(JSON.stringify(frame));
	const ciphertext = new Uint8Array(await crypto.subtle.encrypt({ name: AES_ALGORITHM, iv }, key, plaintext));
	const out = new Uint8Array(IV_LENGTH + ciphertext.byteLength);
	out.set(iv, 0);
	out.set(ciphertext, IV_LENGTH);
	return out;
}

async function openSealed(key: CryptoKey, data: Uint8Array): Promise<CollabFrame> {
	if (data.byteLength <= IV_LENGTH) throw new Error("Sealed frame too short");
	const iv = asStrict(data.subarray(0, IV_LENGTH));
	const ciphertext = asStrict(data.subarray(IV_LENGTH));
	const plaintext = new Uint8Array(await crypto.subtle.decrypt({ name: AES_ALGORITHM, iv }, key, ciphertext));
	return JSON.parse(textDecoder.decode(plaintext)) as CollabFrame;
}

function packEnvelope(peerId: number, sealed: Uint8Array): Uint8Array {
	const out = new Uint8Array(ENVELOPE_HEADER_LENGTH + sealed.byteLength);
	new DataView(out.buffer).setUint32(0, peerId, false);
	out.set(sealed, ENVELOPE_HEADER_LENGTH);
	return out;
}

function unpackEnvelope(data: Uint8Array): { peerId: number; payload: Uint8Array } | null {
	if (data.byteLength < ENVELOPE_HEADER_LENGTH) return null;
	const peerId = new DataView(data.buffer, data.byteOffset, ENVELOPE_HEADER_LENGTH).getUint32(0, false);
	return { peerId, payload: data.subarray(ENVELOPE_HEADER_LENGTH) };
}

function relayOrigin(relayUrl: string): string {
	const url = new URL(relayUrl);
	const scheme = url.protocol === "http:" || url.protocol === "ws:" ? "ws:" : "wss:";
	const port = url.port ? `:${url.port}` : "";
	return `${scheme}//${url.hostname}${port}`;
}

/** Compact collab link: `host[:port]/r/<roomId>.<base64url(key[∥writeToken])>` */
function formatCollabLink(relayUrl: string, roomId: string, key: Uint8Array, writeToken?: Uint8Array): string {
	const origin = relayOrigin(relayUrl);
	const secret = writeToken ? Buffer.concat([key, writeToken]) : Buffer.from(key);
	const compact = origin.startsWith("wss://") ? origin.slice("wss://".length) : origin;
	return `${compact}/r/${roomId}.${secret.toString("base64url")}`;
}

function verifyWriteToken(expected: Uint8Array, token: string | undefined): boolean {
	if (!token) return false;
	const bytes = Buffer.from(token, "base64url");
	return bytes.byteLength === expected.byteLength && timingSafeEqual(bytes, expected);
}

class MiniCollabHost {
	wsUrl: string;
	joinHandle: string;
	viewHandle: string;
	#key: CryptoKey;
	#writeToken: Uint8Array;
	#onFrame: (frame: CollabFrame, fromPeer: number) => void;
	#log: ExtensionAPI["logger"];
	#ws: WebSocket | null = null;
	#closed = false;
	#attempt = 0;
	#retryTimer: ReturnType<typeof setTimeout> | undefined;
	#sendChain: Promise<void> = Promise.resolve();
	#recvChain: Promise<void> = Promise.resolve();

	constructor(opts: {
		wsUrl: string;
		joinHandle: string;
		viewHandle: string;
		key: CryptoKey;
		writeToken: Uint8Array;
		onFrame: (frame: CollabFrame, fromPeer: number) => void;
		log: ExtensionAPI["logger"];
	}) {
		this.wsUrl = opts.wsUrl;
		this.joinHandle = opts.joinHandle;
		this.viewHandle = opts.viewHandle;
		this.#key = opts.key;
		this.#writeToken = opts.writeToken;
		this.#onFrame = opts.onFrame;
		this.#log = opts.log;
	}

	get writeToken(): Uint8Array {
		return this.#writeToken;
	}

	connect(): Promise<void> {
		this.#closed = false;
		this.#attempt = 0;
		return new Promise((resolve, reject) => {
			let settled = false;
			const timeout = setTimeout(() => {
				if (settled) return;
				settled = true;
				this.close();
				reject(new Error("timed out connecting to collab relay"));
			}, CONNECT_TIMEOUT_MS);
			const onFirstOpen = (): void => {
				if (settled) return;
				settled = true;
				clearTimeout(timeout);
				resolve();
			};
			this.#openSocket(onFirstOpen, (err) => {
				if (settled) return;
				settled = true;
				clearTimeout(timeout);
				this.close();
				reject(err);
			});
		});
	}

	send(frame: CollabFrame, targetPeer = HOST_PEER_BROADCAST): void {
		this.#sendChain = this.#sendChain
			.then(async () => {
				if (this.#closed) return;
				const sealed = await seal(this.#key, frame);
				const envelope = packEnvelope(targetPeer, sealed);
				const ws = this.#ws;
				if (ws && ws.readyState === WebSocket.OPEN) {
					ws.send(envelope);
				}
			})
			.catch((err: unknown) => {
				this.#log.debug("cascade-omp-plugin: send failed", { error: String(err) });
			});
	}

	close(): void {
		this.#closed = true;
		if (this.#retryTimer !== undefined) {
			clearTimeout(this.#retryTimer);
			this.#retryTimer = undefined;
		}
		const ws = this.#ws;
		this.#ws = null;
		if (ws) {
			try {
				ws.close(1000);
			} catch {
				// already closing
			}
		}
	}

	#openSocket(onFirstOpen?: () => void, onFirstFail?: (err: Error) => void): void {
		const ws = new WebSocket(`${this.wsUrl}?role=host`);
		ws.binaryType = "arraybuffer";
		this.#ws = ws;
		let opened = false;
		ws.onopen = () => {
			if (this.#ws !== ws) return;
			opened = true;
			this.#attempt = 0;
			onFirstOpen?.();
		};
		ws.onmessage = (event: MessageEvent) => {
			if (this.#ws !== ws) return;
			this.#handleMessage(ws, event.data);
		};
		ws.onerror = () => {
			// close carries the reason
		};
		ws.onclose = (event: CloseEvent) => {
			if (this.#ws !== ws) return;
			this.#ws = null;
			const fatal = FATAL_CLOSE[event.code];
			if (fatal) {
				this.#closed = true;
				this.#log.warn("cascade-omp-plugin: collab relay fatal close", { code: event.code, reason: fatal });
				if (!opened) onFirstFail?.(new Error(fatal));
				return;
			}
			if (this.#closed) return;
			if (!opened) {
				onFirstFail?.(new Error(event.reason || `connection lost (code ${event.code})`));
				return;
			}
			this.#scheduleRetry();
		};
	}

	#handleMessage(ws: WebSocket, data: unknown): void {
		if (typeof data === "string") {
			try {
				JSON.parse(data);
			} catch {
				this.#log.debug("cascade-omp-plugin: ignoring malformed control message");
			}
			return;
		}
		const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data instanceof Uint8Array ? data : null;
		if (!bytes) return;
		const envelope = unpackEnvelope(bytes);
		if (!envelope) return;
		this.#recvChain = this.#recvChain
			.then(async () => {
				if (this.#ws !== ws) return;
				let frame: CollabFrame;
				try {
					frame = await openSealed(this.#key, envelope.payload);
				} catch {
					this.#log.warn("cascade-omp-plugin: bad key or corrupted collab frame; closing");
					this.close();
					return;
				}
				if (this.#ws !== ws) return;
				this.#onFrame(frame, envelope.peerId);
			})
			.catch((err: unknown) => {
				this.#log.debug("cascade-omp-plugin: frame handler failed", { error: String(err) });
			});
	}

	#scheduleRetry(): void {
		if (this.#closed) return;
		const base = Math.min(BACKOFF_BASE_MS * 2 ** this.#attempt, BACKOFF_MAX_MS);
		this.#attempt++;
		const delay = base * (0.75 + Math.random() * 0.5);
		this.#retryTimer = setTimeout(() => {
			this.#retryTimer = undefined;
			if (this.#closed) return;
			this.#openSocket();
		}, delay);
	}
}

async function httpJson(
	url: string,
	method: string,
	token: string,
	body: unknown,
): Promise<{ ok: boolean; status: number; text: string }> {
	const res = await fetch(url, {
		method,
		headers: {
			"content-type": "application/json",
			"X-Cascade-Token": token,
		},
		body: JSON.stringify(body),
	});
	const text = await res.text().catch(() => "");
	return { ok: res.ok, status: res.status, text };
}

async function registerTerminal(
	cascadeUrl: string,
	token: string,
	payload: Record<string, unknown>,
	log: ExtensionAPI["logger"],
): Promise<void> {
	const url = `${cascadeUrl.replace(/\/+$/, "")}/register-terminal`;
	let result = await httpJson(url, "POST", token, payload);
	if (!result.ok) {
		log.warn("cascade-omp-plugin: register-terminal failed, retrying once", {
			status: result.status,
			body: result.text.slice(0, 500),
		});
		result = await httpJson(url, "POST", token, payload);
	}
	if (!result.ok) {
		throw new Error(`register-terminal failed: HTTP ${result.status} ${result.text.slice(0, 200)}`);
	}
}

async function unregisterTerminal(
	cascadeUrl: string,
	token: string,
	sessionId: string,
	log: ExtensionAPI["logger"],
): Promise<void> {
	const url = `${cascadeUrl.replace(/\/+$/, "")}/register-terminal`;
	try {
		const result = await httpJson(url, "DELETE", token, { session_id: sessionId, pid: process.pid });
		if (!result.ok) {
			log.warn("cascade-omp-plugin: unregister-terminal failed", { status: result.status });
		}
	} catch (err) {
		log.warn("cascade-omp-plugin: unregister-terminal error", { error: String(err) });
	}
}

export default function (pi: ExtensionAPI): void {
	const log = pi.logger;
	let missingTokenLogged = false;
	let host: MiniCollabHost | null = null;
	let lastCtx: ExtensionContext | null = null;
	const peers = new Map<number, { name: string; canWrite: boolean }>();
	let registeredSessionId: string | null = null;
	let cascadeUrl = DEFAULT_CASCADE_URL;
	let cascadeToken = "";
	let shuttingDown = false;

	const safe = (label: string, fn: () => void | Promise<void>): void => {
		try {
			const result = fn();
			if (result && typeof result.then === "function") {
				void result.catch((err: unknown) => {
					log.warn(`cascade-omp-plugin: ${label} failed`, { error: String(err) });
				});
			}
		} catch (err) {
			log.warn(`cascade-omp-plugin: ${label} failed`, { error: String(err) });
		}
	};

	const handleGuestFrame = (frame: CollabFrame, fromPeer: number): void => {
		safe("guest-frame", () => {
			switch (frame.t) {
				case "hello": {
					const proto = Number(frame.proto);
					if (proto !== COLLAB_PROTO) {
						host?.send(
							{
								t: "error",
								message: `protocol mismatch: host speaks v${COLLAB_PROTO}, guest sent v${proto}`,
							},
							fromPeer,
						);
						return;
					}
					const name = String(frame.name ?? "").trim().slice(0, 64) || `guest-${fromPeer}`;
					const canWrite = host ? verifyWriteToken(host.writeToken, frame.writeToken as string | undefined) : false;
					peers.set(fromPeer, { name, canWrite });
					const ctx = lastCtx;
					const header = {
						type: "session",
						id: ctx?.sessionManager.getSessionId() ?? "",
						title: pi.getSessionName(),
						timestamp: new Date().toISOString(),
						cwd: ctx?.cwd ?? process.cwd(),
					};
					host?.send(
						{
							t: "welcome",
							proto: COLLAB_PROTO,
							header,
							state: {
								isStreaming: !ctx?.isIdle(),
								queuedMessageCount: 0,
								sessionName: pi.getSessionName(),
								cwd: header.cwd,
								participants: [
									{ name: os.userInfo().username, role: "host" },
									{ name, role: "guest", readOnly: canWrite ? undefined : true },
								],
							},
							agents: [],
							entryCount: 0,
							readOnly: canWrite ? undefined : true,
						},
						fromPeer,
					);
					host?.send({ t: "snapshot-chunk", entries: [], final: true }, fromPeer);
					log.info("cascade-omp-plugin: guest joined", { name, canWrite, fromPeer });
					return;
				}
				case "prompt": {
					const peer = peers.get(fromPeer);
					if (!peer?.canWrite) {
						host?.send({ t: "error", message: "prompting is disabled on a read-only link" }, fromPeer);
						return;
					}
					const text = String(frame.text ?? "");
					if (!text) return;
					pi.sendUserMessage(text);
					return;
				}
				case "abort": {
					const peer = peers.get(fromPeer);
					if (!peer?.canWrite) {
						host?.send({ t: "error", message: "interrupting is disabled on a read-only link" }, fromPeer);
						return;
					}
					lastCtx?.abort();
					return;
				}
				default:
					log.debug("cascade-omp-plugin: ignoring guest frame", { t: frame.t, fromPeer });
			}
		});
	};

	pi.on("session_start", (_event, ctx) => {
		safe("session_start", async () => {
			if (ctx.mode !== "tui") return;
			if (envTruthy(process.env.CASCADE_DISABLE)) {
				log.debug("cascade-omp-plugin: CASCADE_DISABLE set; skipping");
				return;
			}
			cascadeToken = (process.env.CASCADE_TOKEN ?? "").trim();
			if (!cascadeToken) {
				if (!missingTokenLogged) {
					missingTokenLogged = true;
					log.warn("cascade-omp-plugin: CASCADE_TOKEN unset; not registering this terminal");
				}
				return;
			}
			cascadeUrl = (process.env.CASCADE_URL ?? DEFAULT_CASCADE_URL).trim() || DEFAULT_CASCADE_URL;
			const relay = (process.env.CASCADE_RELAY ?? DEFAULT_RELAY).trim() || DEFAULT_RELAY;
			lastCtx = ctx;
			shuttingDown = false;
			peers.clear();

			const rawKey = randomBytes(ROOM_KEY_BYTES);
			const writeToken = randomBytes(WRITE_TOKEN_BYTES);
			const roomId = b64url(randomBytes(ROOM_ID_BYTES));
			const origin = relayOrigin(relay);
			const wsUrl = `${origin}/r/${roomId}`;
			const cryptoKey = await importRoomKey(rawKey);
			const next = new MiniCollabHost({
				wsUrl,
				joinHandle: formatCollabLink(relay, roomId, rawKey, writeToken),
				viewHandle: formatCollabLink(relay, roomId, rawKey),
				key: cryptoKey,
				writeToken,
				onFrame: handleGuestFrame,
				log,
			});
			host = next;
			await next.connect();
			if (shuttingDown || host !== next) return;

			const sessionId = ctx.sessionManager.getSessionId() || `pid-${process.pid}-${Date.now()}`;
			registeredSessionId = sessionId;
			await registerTerminal(
				cascadeUrl,
				cascadeToken,
				{
					machine: os.hostname(),
					session_id: sessionId,
					join_handle: next.joinHandle,
					view_handle: next.viewHandle,
					cwd: ctx.cwd,
					title: pi.getSessionName(),
					pid: process.pid,
				},
				log,
			);
			log.info("cascade-omp-plugin: registered terminal", { sessionId, relay: origin });
		});
	});

	for (const name of FORWARD_EVENTS) {
		pi.on(name as "agent_start", (event) => {
			safe(`forward ${name}`, () => {
				if (!host) return;
				host.send({ t: "event", event });
			});
		});
	}

	pi.on("session_shutdown", (_event, ctx) => {
		safe("session_shutdown", async () => {
			shuttingDown = true;
			const sessionId = registeredSessionId ?? ctx.sessionManager.getSessionId();
			if (sessionId && cascadeToken) {
				await unregisterTerminal(cascadeUrl, cascadeToken, sessionId, log);
			}
			registeredSessionId = null;
			try {
				host?.send({ t: "bye", reason: "session shutdown" });
			} catch {
				// ignore
			}
			host?.close();
			host = null;
			peers.clear();
			lastCtx = null;
		});
	});
}
