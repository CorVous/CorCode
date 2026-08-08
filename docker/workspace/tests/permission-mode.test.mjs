import { resolveSettings, filterEscalatingDefaultMode } from "@anthropic-ai/claude-agent-sdk";
import { resolvePermissionMode } from "@agentclientprotocol/claude-agent-acp/dist/acp-agent.js";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const INSTALLED_MANAGED_SETTINGS = "/etc/claude-code/managed-settings.json";
const SETTINGS_PATH = process.env.CORCODE_MANAGED_SETTINGS ?? INSTALLED_MANAGED_SETTINGS;

let failures = 0;

function pass(name) {
  console.log(`ok   ${name}`);
}

function fail(name, detail) {
  console.error(`FAIL ${name}: ${detail}`);
  failures += 1;
}

function skip(name, why) {
  console.error(`SKIPPED ${name}: ${why}`);
}

function expectEqual(name, actual, expected) {
  if (actual === expected) {
    pass(name);
  } else {
    fail(name, `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

/**
 * The mode a session would open in, resolved the way the adapter resolves it:
 * the SDK's merge of every settings tier, the CLI's trust filter over that,
 * then the adapter's own mapping onto a mode id.
 */
async function permissionModeFromConfigDir(configDir) {
  process.env.CLAUDE_CONFIG_DIR = configDir;
  const complaints = [];
  const resolved = filterEscalatingDefaultMode(await resolveSettings({ cwd: configDir }));
  const mode = resolvePermissionMode(resolved.permissions?.defaultMode, {
    error: (...parts) => complaints.push(parts.join(" ")),
  });
  return { mode, complaints };
}

async function inAConfigDir(name, body) {
  const dir = mkdtempSync(join(tmpdir(), `corcode-${name}-`));
  try {
    await body(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
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

/**
 * A trusted tier standing in for the managed one wherever this runs off a
 * checkout: the trust filter strips escalating modes only from repo-committed
 * `project` settings, so user and managed agree on what survives it.
 */
await inAConfigDir("seeded-tier", async (configDir) => {
  writeFileSync(join(configDir, "settings.json"), JSON.stringify(settings));
  const { mode, complaints } = await permissionModeFromConfigDir(configDir);
  expectEqual("the shipped settings resolve to auto mode", mode, "auto");
  expectEqual("the adapter accepts the shipped mode without complaint", complaints.join("; "), "");
});

const managedTierAlone = "the installed managed tier carries auto with no other tier to help";
if (SETTINGS_PATH === INSTALLED_MANAGED_SETTINGS) {
  await inAConfigDir("empty-tier", async (configDir) => {
    const { mode } = await permissionModeFromConfigDir(configDir);
    expectEqual(managedTierAlone, mode, "auto");
  });
} else {
  skip(managedTierAlone, `this run reads ${SETTINGS_PATH}, not the installed file`);
}

if (failures > 0) {
  console.error(`${failures} test(s) failed`);
  process.exit(1);
}
console.log("all permission-mode tests passed");
