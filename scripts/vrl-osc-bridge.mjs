#!/usr/bin/env node

import { createSocket } from "node:dgram";
import { appendFile, mkdir, readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

const DEFAULT_CONFIG = {
  websiteBaseUrl: "https://dev.vrlewds.com",
  creatorUsername: "",
  streamKey: "",
  allowInsecureHttp: false,
  oscListen: {
    host: "127.0.0.1",
    port: 9001,
  },
  forwardTargets: [],
  inputs: [
    {
      address: "/avatar/parameters/SPS_Contact",
      weight: 1,
      curve: "linear",
      invert: false,
      deadzone: 0.02,
      min: 0,
      max: 1,
    },
  ],
  output: {
    emitHz: 20,
    attackMs: 55,
    releaseMs: 220,
    emaAlpha: 0.35,
    minDelta: 0.015,
    heartbeatMs: 1000,
  },
  relay: {
    sessionPath: "/api/sps/session",
    ingestPath: "/api/sps/ingest",
  },
  debug: {
    logOsc: false,
    logUnmappedOnly: false,
    logConfiguredOnly: false,
    logRelay: false,
  },
  discovery: {
    enabled: false,
    filePath: "scripts/vrl-osc-discovered.txt",
    includeArgTypes: false,
  },
};

function parseArgs(argv) {
  const result = {
    configPath: "scripts/vrl-osc-bridge.example.json",
    debugOsc: false,
    debugUnmappedOnly: false,
    debugConfiguredOnly: false,
    debugRelay: false,
    discoveryPath: "",
    discoveryIncludeArgTypes: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--help" || arg === "-h") {
      printHelp();
      process.exit(0);
    }
    if (arg === "--config") {
      const value = argv[i + 1];
      if (!value) throw new Error("Missing value for --config");
      i += 1;
      result.configPath = value;
      continue;
    }
    if (arg === "--debug-osc") {
      result.debugOsc = true;
      continue;
    }
    if (arg === "--debug-unmapped") {
      result.debugOsc = true;
      result.debugUnmappedOnly = true;
      result.debugConfiguredOnly = false;
      continue;
    }
    if (arg === "--debug-configured") {
      result.debugOsc = true;
      result.debugUnmappedOnly = false;
      result.debugConfiguredOnly = true;
      continue;
    }
    if (arg === "--debug-relay") {
      result.debugRelay = true;
      continue;
    }
    if (arg === "--discover-osc") {
      const value = argv[i + 1];
      if (!value) throw new Error("Missing value for --discover-osc");
      i += 1;
      result.discoveryPath = value;
      continue;
    }
    if (arg === "--discover-osc-with-types") {
      result.discoveryIncludeArgTypes = true;
      continue;
    }
    throw new Error(`Unknown argument: ${arg}`);
  }
  return result;
}

function printHelp() {
  process.stdout.write(
    [
      "Usage: node scripts/vrl-osc-bridge.mjs --config <path>",
      "",
      "Reads local OSC (VRChat), forwards raw packets to local UDP targets,",
      "normalizes mapped SPS intensity, and securely relays to VRLewds.",
      "",
      "Flags:",
      "  --config <path>   JSON config file path",
      "  --debug-osc       Log all incoming OSC messages",
      "  --debug-unmapped  Log only OSC messages that are not mapped",
      "  --debug-configured Log only OSC messages that match configured input addresses",
      "  --debug-relay     Log successful relay ingest acknowledgements",
      "  --discover-osc <path> Write unique OSC inputs to a file (deduplicated)",
      "  --discover-osc-with-types Include OSC type tags in discovery entries",
      "  --help            Show this help",
      "",
    ].join("\n"),
  );
}

function deepMerge(base, incoming) {
  if (!incoming || typeof incoming !== "object" || Array.isArray(incoming)) return base;
  const next = { ...base };
  for (const [key, value] of Object.entries(incoming)) {
    if (Array.isArray(value)) {
      next[key] = value.slice();
      continue;
    }
    if (value && typeof value === "object") {
      next[key] = deepMerge(
        base[key] && typeof base[key] === "object" && !Array.isArray(base[key]) ? base[key] : {},
        value,
      );
      continue;
    }
    next[key] = value;
  }
  return next;
}

async function loadConfig(configPath) {
  const fullPath = resolve(configPath);
  const raw = await readFile(fullPath, "utf8");
  const userConfig = JSON.parse(raw);
  return {
    fullPath,
    value: deepMerge(DEFAULT_CONFIG, userConfig),
  };
}

function clamp01(value) {
  if (!Number.isFinite(value)) return 0;
  if (value < 0) return 0;
  if (value > 1) return 1;
  return value;
}

function clamp(value, min, max) {
  if (!Number.isFinite(value)) return min;
  if (value < min) return min;
  if (value > max) return max;
  return value;
}

function normalizeUrlConfig(config) {
  const rawBase = String(config.websiteBaseUrl || "").trim();
  if (!rawBase) throw new Error("config.websiteBaseUrl is required");
  let base;
  try {
    base = new URL(rawBase);
  } catch {
    throw new Error("config.websiteBaseUrl must be a valid URL");
  }
  const isLocalHost = base.hostname === "127.0.0.1" || base.hostname === "localhost";
  if (base.protocol !== "https:" && !(isLocalHost || config.allowInsecureHttp === true)) {
    throw new Error("websiteBaseUrl must use HTTPS unless allowInsecureHttp=true for local testing");
  }
  return base;
}

function normalizeMappings(config) {
  if (!Array.isArray(config.inputs) || config.inputs.length === 0) {
    throw new Error("config.inputs must contain at least one OSC mapping");
  }
  return config.inputs
    .map((entry) => {
      const address = typeof entry.address === "string" ? entry.address.trim() : "";
      if (!address || !address.startsWith("/")) return null;
      const weight = Number.isFinite(Number(entry.weight)) ? Number(entry.weight) : 1;
      const deadzone = clamp(Number(entry.deadzone ?? 0), 0, 1);
      const invert = Boolean(entry.invert);
      const curve = typeof entry.curve === "string" ? entry.curve.trim() : "linear";
      const min = Number.isFinite(Number(entry.min)) ? Number(entry.min) : 0;
      const max = Number.isFinite(Number(entry.max)) ? Number(entry.max) : 1;
      return {
        address,
        weight,
        deadzone,
        invert,
        curve,
        min,
        max: max <= min ? min + 1 : max,
      };
    })
    .filter(Boolean);
}

function normalizeForwardTargets(config) {
  if (!Array.isArray(config.forwardTargets)) return [];
  return config.forwardTargets
    .map((entry) => {
      const host = typeof entry.host === "string" ? entry.host.trim() : "";
      const port = Number(entry.port);
      if (!host || !Number.isInteger(port) || port < 1 || port > 65535) return null;
      return { host, port };
    })
    .filter(Boolean);
}

function normalizeOutputConfig(config) {
  const output = config.output && typeof config.output === "object" ? config.output : {};
  return {
    emitHz: clamp(Number(output.emitHz ?? 20), 2, 60),
    attackMs: clamp(Number(output.attackMs ?? 55), 10, 2000),
    releaseMs: clamp(Number(output.releaseMs ?? 220), 10, 5000),
    emaAlpha: clamp(Number(output.emaAlpha ?? 0.35), 0.01, 1),
    minDelta: clamp(Number(output.minDelta ?? 0.015), 0, 1),
    heartbeatMs: clamp(Number(output.heartbeatMs ?? 1000), 200, 10_000),
  };
}

function normalizeConfig(config, cliOptions = {}) {
  const baseUrl = normalizeUrlConfig(config);
  const creatorUsername = String(config.creatorUsername || "").trim();
  const streamKey = String(config.streamKey || "").trim();
  if (!creatorUsername) throw new Error("config.creatorUsername is required");
  if (!streamKey) throw new Error("config.streamKey is required");

  const oscHost = String(config.oscListen?.host || "127.0.0.1").trim();
  const oscPort = Number(config.oscListen?.port ?? 9001);
  if (!oscHost) throw new Error("config.oscListen.host is required");
  if (!Number.isInteger(oscPort) || oscPort < 1 || oscPort > 65535) {
    throw new Error("config.oscListen.port must be a valid UDP port");
  }

  const debugConfig =
    config.debug && typeof config.debug === "object" && !Array.isArray(config.debug)
      ? config.debug
      : {};
  const debugOscFromConfig = Boolean(debugConfig.logOsc);
  const debugUnmappedFromConfig = Boolean(debugConfig.logUnmappedOnly);
  const debugConfiguredFromConfig = Boolean(debugConfig.logConfiguredOnly);
  const debugRelayFromConfig = Boolean(debugConfig.logRelay);
  const debugOscFromCli = Boolean(cliOptions.debugOsc);
  const debugUnmappedFromCli = Boolean(cliOptions.debugUnmappedOnly);
  const debugConfiguredFromCli = Boolean(cliOptions.debugConfiguredOnly);
  const debugRelayFromCli = Boolean(cliOptions.debugRelay);
  const requestedUnmappedOnly = debugUnmappedFromCli || debugUnmappedFromConfig;
  const requestedConfiguredOnly = debugConfiguredFromCli || debugConfiguredFromConfig;
  const logConfiguredOnly = requestedConfiguredOnly;
  const logUnmappedOnly = requestedUnmappedOnly && !requestedConfiguredOnly;
  const logOsc = debugOscFromCli || debugOscFromConfig || logUnmappedOnly || logConfiguredOnly;
  const logRelay = debugRelayFromCli || debugRelayFromConfig;
  const discoveryConfig =
    config.discovery && typeof config.discovery === "object" && !Array.isArray(config.discovery)
      ? config.discovery
      : {};
  const discoveryPathRaw =
    (typeof cliOptions.discoveryPath === "string" && cliOptions.discoveryPath.trim()
      ? cliOptions.discoveryPath.trim()
      : typeof discoveryConfig.filePath === "string"
        ? discoveryConfig.filePath.trim()
        : "") || "scripts/vrl-osc-discovered.txt";
  const discoveryEnabled =
    (typeof cliOptions.discoveryPath === "string" && cliOptions.discoveryPath.trim().length > 0) ||
    Boolean(discoveryConfig.enabled);
  const discoveryIncludeArgTypes =
    Boolean(cliOptions.discoveryIncludeArgTypes) || Boolean(discoveryConfig.includeArgTypes);

  return {
    baseUrl,
    creatorUsername,
    streamKey,
    oscListen: {
      host: oscHost,
      port: oscPort,
    },
    relay: {
      sessionPath: String(config.relay?.sessionPath || "/api/sps/session"),
      ingestPath: String(config.relay?.ingestPath || "/api/sps/ingest"),
    },
    debug: {
      logOsc,
      logUnmappedOnly,
      logConfiguredOnly,
      logRelay,
    },
    discovery: {
      enabled: discoveryEnabled,
      filePath: resolve(discoveryPathRaw),
      includeArgTypes: discoveryIncludeArgTypes,
    },
    forwardTargets: normalizeForwardTargets(config),
    mappings: normalizeMappings(config),
    output: normalizeOutputConfig(config),
  };
}

function align4(value) {
  return (value + 3) & ~3;
}

function readOscString(packet, offset) {
  let cursor = offset;
  while (cursor < packet.length && packet[cursor] !== 0) {
    cursor += 1;
  }
  if (cursor >= packet.length) return null;
  const value = packet.toString("utf8", offset, cursor);
  const nextOffset = align4(cursor + 1);
  if (nextOffset > packet.length) return null;
  return { value, nextOffset };
}

function parseOscMessage(packet, offset = 0) {
  const addressPart = readOscString(packet, offset);
  if (!addressPart) return null;
  const typePart = readOscString(packet, addressPart.nextOffset);
  if (!typePart) return null;
  const typeTag = typePart.value;
  if (!typeTag.startsWith(",")) return null;

  const args = [];
  let cursor = typePart.nextOffset;
  for (const type of typeTag.slice(1)) {
    if (type === "i") {
      if (cursor + 4 > packet.length) return null;
      args.push(packet.readInt32BE(cursor));
      cursor += 4;
      continue;
    }
    if (type === "f") {
      if (cursor + 4 > packet.length) return null;
      args.push(packet.readFloatBE(cursor));
      cursor += 4;
      continue;
    }
    if (type === "T") {
      args.push(true);
      continue;
    }
    if (type === "F") {
      args.push(false);
      continue;
    }
    if (type === "s") {
      const part = readOscString(packet, cursor);
      if (!part) return null;
      args.push(part.value);
      cursor = part.nextOffset;
      continue;
    }
    return null;
  }

  return {
    address: addressPart.value,
    args,
    argType: typeTag.slice(1, 2) || "",
    argTypes: typeTag.slice(1),
  };
}

function parseOscPacket(packet) {
  const first = readOscString(packet, 0);
  if (!first) return [];
  if (first.value !== "#bundle") {
    const message = parseOscMessage(packet, 0);
    return message ? [message] : [];
  }

  let cursor = first.nextOffset;
  if (cursor + 8 > packet.length) return [];
  cursor += 8;
  const messages = [];
  while (cursor + 4 <= packet.length) {
    const size = packet.readInt32BE(cursor);
    cursor += 4;
    if (size <= 0 || cursor + size > packet.length) break;
    const chunk = packet.subarray(cursor, cursor + size);
    messages.push(...parseOscPacket(chunk));
    cursor += size;
  }
  return messages;
}

function extractNumericArg(args) {
  for (const arg of args) {
    if (typeof arg === "number" && Number.isFinite(arg)) return arg;
    if (typeof arg === "boolean") return arg ? 1 : 0;
  }
  return null;
}

function formatOscArgValue(value) {
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return "NaN";
    return Number.isInteger(value) ? String(value) : value.toFixed(4);
  }
  if (typeof value === "boolean") return value ? "true" : "false";
  if (typeof value === "string") return JSON.stringify(value);
  return String(value);
}

function logOscDebug(state, message, mappingState) {
  if (!state.config.debug.logOsc) return;
  if (state.config.debug.logUnmappedOnly && mappingState === "mapped") return;
  if (state.config.debug.logUnmappedOnly && mappingState === "address-match-non-numeric") return;
  if (state.config.debug.logConfiguredOnly && mappingState === "unmapped") return;

  const typeTag = typeof message.argTypes === "string" && message.argTypes.length > 0 ? message.argTypes : "-";
  const argsText =
    Array.isArray(message.args) && message.args.length > 0
      ? message.args.map((arg) => formatOscArgValue(arg)).join(", ")
      : "no-args";
  process.stdout.write(
    `[osc] ${mappingState.padEnd(24)} ${message.address} [${typeTag}] -> ${argsText}\n`,
  );
}

async function loadDiscoveryEntries(filePath) {
  try {
    const raw = await readFile(filePath, "utf8");
    const entries = raw
      .split(/\r?\n/u)
      .map((line) => line.trim())
      .filter((line) => line.length > 0 && !line.startsWith("#"));
    return new Set(entries);
  } catch (error) {
    if (error?.code === "ENOENT") return new Set();
    throw error;
  }
}

function discoveryEntryFor(message, includeArgTypes) {
  const address = typeof message?.address === "string" ? message.address.trim() : "";
  if (!address || !address.startsWith("/")) return "";
  if (!includeArgTypes) return address;
  const typeTag =
    typeof message?.argTypes === "string" && message.argTypes.trim().length > 0
      ? message.argTypes.trim()
      : "-";
  return `${address}\t[${typeTag}]`;
}

function recordOscDiscovery(state, message) {
  const discovery = state.discovery;
  if (!discovery?.enabled) return;

  const entry = discoveryEntryFor(message, discovery.includeArgTypes);
  if (!entry) return;
  if (discovery.seenEntries.has(entry)) return;

  discovery.seenEntries.add(entry);
  discovery.writeQueue = discovery.writeQueue
    .then(async () => {
      await mkdir(dirname(discovery.filePath), { recursive: true });
      await appendFile(discovery.filePath, `${entry}\n`, "utf8");
      process.stdout.write(`[osc-discovery] + ${entry}\n`);
    })
    .catch((error) => {
      process.stderr.write(
        `[bridge] discovery write failed: ${String(error?.message || error)}\n`,
      );
    });
}

function applyCurve(value, curve) {
  const x = clamp01(value);
  if (curve === "easeOutQuad") return 1 - (1 - x) * (1 - x);
  if (curve === "easeInQuad") return x * x;
  if (curve === "easeInOutQuad") {
    return x < 0.5 ? 2 * x * x : 1 - Math.pow(-2 * x + 2, 2) / 2;
  }
  return x;
}

function mapInputValue(rawValue, mapping) {
  const normalized = (rawValue - mapping.min) / (mapping.max - mapping.min);
  let value = clamp01(normalized);
  if (mapping.invert) value = 1 - value;
  if (value < mapping.deadzone) value = 0;
  value = applyCurve(value, mapping.curve);
  return clamp01(value);
}

function buildUrl(baseUrl, path) {
  return new URL(path, baseUrl).toString();
}

async function createRelaySession(state) {
  const url = buildUrl(state.config.baseUrl, state.config.relay.sessionPath);
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      creatorUsername: state.config.creatorUsername,
      streamKey: state.config.streamKey,
    }),
  });
  const text = await response.text();
  let payload = null;
  if (text.trim()) {
    try {
      payload = JSON.parse(text);
    } catch {
      payload = null;
    }
  }

  if (!response.ok) {
    const errorMessage = typeof payload?.error === "string" ? payload.error : `HTTP ${response.status}`;
    throw new Error(`Session auth failed: ${errorMessage}`);
  }

  const token = typeof payload?.token === "string" ? payload.token : "";
  const expiresAtIso = typeof payload?.expiresAt === "string" ? payload.expiresAt : "";
  const streamId = typeof payload?.streamId === "string" ? payload.streamId : "";
  const serverNowMs = Number(payload?.serverNowMs);
  const responseDateHeader = response.headers.get("date");
  const responseDateMs = responseDateHeader ? Date.parse(responseDateHeader) : Number.NaN;
  const effectiveServerNowMs = Number.isFinite(serverNowMs) ? serverNowMs : responseDateMs;
  const maxSkewMs = Number(payload?.maxSkewMs);
  if (!token || !expiresAtIso || !streamId) {
    throw new Error("Session auth returned invalid payload");
  }

  const expiresAtMs = Date.parse(expiresAtIso);
  if (!Number.isFinite(expiresAtMs)) {
    throw new Error("Session auth returned invalid expiresAt");
  }

  state.relayToken = token;
  state.relayTokenExpiresAtMs = expiresAtMs;
  state.streamId = streamId;
  if (Number.isFinite(effectiveServerNowMs)) {
    state.clockOffsetMs = Math.trunc(effectiveServerNowMs - Date.now());
  }
  if (Number.isFinite(maxSkewMs) && maxSkewMs > 0) {
    state.maxSkewMs = Math.trunc(maxSkewMs);
  }
  process.stdout.write(
    `[bridge] session established stream=${streamId} expires=${new Date(expiresAtMs).toISOString()}\n`,
  );
  if (Math.abs(state.clockOffsetMs) > Math.max(1000, Math.floor(state.maxSkewMs / 10))) {
    process.stdout.write(
      `[bridge] clock offset=${state.clockOffsetMs}ms acceptedSkew=${state.maxSkewMs}ms\n`,
    );
  }
}

async function ensureRelaySession(state) {
  const now = Date.now();
  if (
    state.relayToken &&
    state.relayTokenExpiresAtMs - now > 20_000
  ) {
    return;
  }
  await createRelaySession(state);
}

async function pushRelayEvent(state, payload) {
  await ensureRelaySession(state);

  const url = buildUrl(state.config.baseUrl, state.config.relay.ingestPath);
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${state.relayToken}`,
    },
    body: JSON.stringify(payload),
  });

  if (response.status === 401 || response.status === 403) {
    state.relayToken = "";
    state.relayTokenExpiresAtMs = 0;
    throw new Error("Relay token rejected");
  }

  if (response.status === 409) {
    state.relayToken = "";
    state.relayTokenExpiresAtMs = 0;
    throw new Error("Creator stream is offline or rotated");
  }

  if (response.status === 429) {
    throw new Error("Relay event throttled");
  }

  if (!response.ok) {
    const body = await response.text().catch(() => "");
    if (response.status === 400 && body.includes("Stale or invalid event timestamp")) {
      const responseDateHeader = response.headers.get("date");
      const responseDateMs = responseDateHeader ? Date.parse(responseDateHeader) : Number.NaN;
      if (Number.isFinite(responseDateMs)) {
        state.clockOffsetMs = Math.trunc(responseDateMs - Date.now());
      }
      throw new Error("Relay ingest rejected timestamp; re-synced clock offset and will retry");
    }
    throw new Error(`Relay ingest failed: HTTP ${response.status}${body ? ` ${body}` : ""}`);
  }

  if (state.config.debug.logRelay) {
    const nowMs = Date.now();
    const deltaMs = nowMs - Number(payload.ts || 0);
    const shouldLog =
      nowMs - state.lastRelayAckLogAtMs >= 1000 || Number(payload.seq) % 20 === 0;
    if (shouldLog) {
      state.lastRelayAckLogAtMs = nowMs;
      process.stdout.write(
        `[relay] ack seq=${payload.seq} intensity=${Number(payload.intensity).toFixed(3)} deltaMs=${deltaMs}\n`,
      );
    }
  }
}

function calculateCompositeIntensity(state) {
  let weightedSum = 0;
  let totalWeight = 0;
  for (const mapping of state.config.mappings) {
    const sample = state.currentInputs.get(mapping.address);
    if (sample === undefined) continue;
    const weight = Math.abs(mapping.weight);
    weightedSum += sample * weight;
    totalWeight += weight;
  }
  if (totalWeight <= 0) return 0;
  return clamp01(weightedSum / totalWeight);
}

function applySmoothing(state, nowMs) {
  const dt = Math.max(1, nowMs - state.lastTickMs);
  state.lastTickMs = nowMs;

  const target = state.targetIntensity;
  const current = state.currentIntensity;
  const tauMs = target >= current ? state.config.output.attackMs : state.config.output.releaseMs;
  const lerpAlpha = 1 - Math.exp(-dt / Math.max(1, tauMs));
  let next = current + (target - current) * lerpAlpha;
  next = state.config.output.emaAlpha * next + (1 - state.config.output.emaAlpha) * current;
  state.currentIntensity = clamp01(next);

  if (state.currentIntensity >= state.peakIntensity) {
    state.peakIntensity = state.currentIntensity;
  } else {
    state.peakIntensity = Math.max(
      state.currentIntensity,
      state.peakIntensity - 0.025,
    );
  }
}

function shouldEmit(state, nowMs) {
  const delta = Math.abs(state.currentIntensity - state.lastSentIntensity);
  if (delta >= state.config.output.minDelta) return true;
  if (nowMs - state.lastSentAtMs >= state.config.output.heartbeatMs) return true;
  return false;
}

async function onTick(state) {
  const nowMs = Date.now();
  applySmoothing(state, nowMs);
  if (!shouldEmit(state, nowMs)) return;
  if (state.inFlight) return;

  state.inFlight = true;
  state.seq += 1;
  const payload = {
    seq: state.seq,
    ts: nowMs + state.clockOffsetMs,
    intensity: state.currentIntensity,
    peak: state.peakIntensity,
    raw: state.targetIntensity,
    source: {
      address: state.lastSourceAddress,
      argType: state.lastSourceArgType,
    },
  };

  try {
    await pushRelayEvent(state, payload);
    state.lastSentIntensity = state.currentIntensity;
    state.lastSentAtMs = nowMs;
    state.lastError = "";
  } catch (error) {
    const message = String(error?.message || error || "relay error");
    if (state.lastError !== message) {
      state.lastError = message;
      process.stderr.write(`[bridge] ${message}\n`);
    }
  } finally {
    state.inFlight = false;
  }
}

function forwardPacket(state, packet) {
  if (state.config.forwardTargets.length === 0) return;
  for (const target of state.config.forwardTargets) {
    state.forwardSocket.send(packet, target.port, target.host, () => {});
  }
}

function onOscPacket(state, packet) {
  forwardPacket(state, packet);
  const messages = parseOscPacket(packet);
  if (messages.length === 0) return;

  let touched = false;
  for (const message of messages) {
    recordOscDiscovery(state, message);
    let addressMatched = false;
    let messageMapped = false;
    for (const mapping of state.config.mappings) {
      if (mapping.address !== message.address) continue;
      addressMatched = true;
      const numericArg = extractNumericArg(message.args);
      if (numericArg === null) continue;
      const mapped = mapInputValue(numericArg, mapping);
      state.currentInputs.set(mapping.address, mapped);
      state.lastSourceAddress = message.address;
      state.lastSourceArgType = message.argType || "f";
      touched = true;
      messageMapped = true;
    }
    const mappingState = messageMapped
      ? "mapped"
      : addressMatched
        ? "address-match-non-numeric"
        : "unmapped";
    logOscDebug(state, message, mappingState);
  }
  if (!touched) return;
  state.targetIntensity = calculateCompositeIntensity(state);
}

function createRuntimeState(config) {
  return {
    config,
    relayToken: "",
    relayTokenExpiresAtMs: 0,
    streamId: "",
    clockOffsetMs: 0,
    maxSkewMs: 30_000,
    seq: 0,
    targetIntensity: 0,
    currentIntensity: 0,
    peakIntensity: 0,
    lastTickMs: Date.now(),
    lastSentIntensity: 0,
    lastSentAtMs: 0,
    lastError: "",
    inFlight: false,
    currentInputs: new Map(),
    lastSourceAddress: "",
    lastSourceArgType: "f",
    listenSocket: createSocket("udp4"),
    forwardSocket: createSocket("udp4"),
    tickTimer: null,
    lastRelayAckLogAtMs: 0,
    discovery: {
      enabled: config.discovery.enabled,
      filePath: config.discovery.filePath,
      includeArgTypes: config.discovery.includeArgTypes,
      seenEntries: new Set(),
      writeQueue: Promise.resolve(),
    },
  };
}

async function run() {
  const args = parseArgs(process.argv.slice(2));
  const loaded = await loadConfig(args.configPath);
  const config = normalizeConfig(loaded.value, args);
  const state = createRuntimeState(config);
  if (config.discovery.enabled) {
    state.discovery.seenEntries = await loadDiscoveryEntries(config.discovery.filePath);
  }

  process.stdout.write(`[bridge] config loaded from ${loaded.fullPath}\n`);
  process.stdout.write(
    `[bridge] OSC listen ${config.oscListen.host}:${config.oscListen.port} | relay ${config.baseUrl.origin}\n`,
  );
  process.stdout.write(
    `[bridge] mappings=${config.mappings.length} forwardTargets=${config.forwardTargets.length}\n`,
  );
  if (config.debug.logOsc) {
    const debugMode = config.debug.logUnmappedOnly
      ? "unmapped-only"
      : config.debug.logConfiguredOnly
        ? "configured-only"
        : "all messages";
    process.stdout.write(
      `[bridge] OSC debug enabled (${debugMode})\n`,
    );
  }
  if (config.discovery.enabled) {
    process.stdout.write(
      `[bridge] OSC discovery enabled file=${config.discovery.filePath} includeArgTypes=${config.discovery.includeArgTypes} existing=${state.discovery.seenEntries.size}\n`,
    );
  }

  state.listenSocket.on("error", (error) => {
    process.stderr.write(`[bridge] listen socket error: ${String(error?.message || error)}\n`);
  });

  state.listenSocket.on("message", (packet) => {
    onOscPacket(state, packet);
  });

  state.listenSocket.bind(config.oscListen.port, config.oscListen.host);

  const tickMs = Math.max(16, Math.round(1000 / config.output.emitHz));
  state.tickTimer = setInterval(() => {
    void onTick(state);
  }, tickMs);

  let shuttingDown = false;
  const shutdown = async () => {
    if (shuttingDown) return;
    shuttingDown = true;
    process.stdout.write("[bridge] shutting down\n");
    if (state.tickTimer) clearInterval(state.tickTimer);
    state.listenSocket.close();
    state.forwardSocket.close();
    if (state.discovery?.enabled) {
      await state.discovery.writeQueue.catch(() => {});
    }
    process.exit(0);
  };

  process.on("SIGINT", () => {
    void shutdown();
  });
  process.on("SIGTERM", () => {
    void shutdown();
  });
}

run().catch((error) => {
  process.stderr.write(`[bridge] fatal: ${String(error?.message || error)}\n`);
  process.exit(1);
});
