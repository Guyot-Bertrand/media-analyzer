// Configuration WebdriverIO : pilote l'application Tauri réelle via tauri-driver (WebKitWebDriver).
// Lancée par e2e/run.mjs, qui fixe MA_BENCH_APP, MA_BENCH_FILE et l'affichage.
import { spawn, type ChildProcess } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const app = process.env.MA_BENCH_APP ?? path.resolve(here, "../src-tauri/target/debug/transport-bench");
let driver: ChildProcess | undefined;

export const config: WebdriverIO.Config = {
  runner: "local",
  specs: ["./transport.e2e.ts"],
  maxInstances: 1,
  hostname: "127.0.0.1",
  port: 4444,
  capabilities: [{ maxInstances: 1, "tauri:options": { application: app } } as WebdriverIO.Capabilities],
  logLevel: "warn",
  reporters: ["spec"],
  framework: "mocha",
  // Un parcours complet sur uhd_big dure plusieurs minutes (ouverture de 4 Go comprise).
  mochaOpts: { ui: "bdd", timeout: 900_000 },
  waitforTimeout: 60_000,
  beforeSession: () => {
    const bin = process.env.TAURI_DRIVER ?? path.resolve(os.homedir(), ".cargo/bin/tauri-driver");
    driver = spawn(bin, [], { stdio: [null, process.stdout, process.stderr] });
  },
  afterSession: () => {
    driver?.kill();
  },
};
