import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const SETTINGS_PATH =
  process.env.CORCODE_MANAGED_SETTINGS ?? "/etc/claude-code/managed-settings.json";

let failures = 0;

function pass(name) {
  console.log(`ok   ${name}`);
}

function fail(name, detail) {
  console.error(`FAIL ${name}: ${detail}`);
  failures += 1;
}

function expectEqual(name, actual, expected) {
  if (actual === expected) {
    pass(name);
  } else {
    fail(name, `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

/**
 * The adapter resolves settings through the SDK for the workspace it opens,
 * applies the CLI's trust filter, then maps `permissions.defaultMode` onto the
 * mode a session runs in. Seeding a throwaway config dir puts the shipped file
 * in the user tier: the trust filter only strips escalating modes that arrive
 * from repo-committed `project` settings, so user and managed agree here, and
 * an image run additionally reads the installed file at its real path.
 */
async function resolvedPermissionMode(configDir) {
  process.env.CLAUDE_CONFIG_DIR = configDir;
  const { resolveSettings, filterEscalatingDefaultMode } = await import(
    "@anthropic-ai/claude-agent-sdk"
  );
  const { resolvePermissionMode } = await import(
    "@agentclientprotocol/claude-agent-acp/dist/acp-agent.js"
  );
  const complaints = [];
  const resolved = filterEscalatingDefaultMode(await resolveSettings({ cwd: configDir }));
  const mode = resolvePermissionMode(resolved.permissions?.defaultMode, {
    error: (...parts) => complaints.push(parts.join(" ")),
  });
  return { mode, complaints };
}

const settings = JSON.parse(readFileSync(SETTINGS_PATH, "utf8"));

expectEqual(
  "the shipped settings ask the agent nothing — an ask would dead-end a turn the client auto-declines",
  settings.permissions?.ask,
  undefined,
);
expectEqual(
  "the shipped settings lean on no allow rules — auto mode drops them",
  settings.permissions?.allow,
  undefined,
);

const configDir = mkdtempSync(join(tmpdir(), "corcode-settings-"));
try {
  writeFileSync(join(configDir, "settings.json"), JSON.stringify(settings));
  const { mode, complaints } = await resolvedPermissionMode(configDir);
  expectEqual("the shipped settings open a session in auto mode", mode, "auto");
  expectEqual(
    "the adapter accepts the shipped mode without complaint",
    complaints.join("; "),
    "",
  );
} finally {
  rmSync(configDir, { recursive: true, force: true });
}

if (failures > 0) {
  console.error(`${failures} test(s) failed`);
  process.exit(1);
}
console.log("all permission-mode tests passed");
