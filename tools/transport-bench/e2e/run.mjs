// Lance WebdriverIO sur l'application du banc, sous Xvfb (défaut) ou sur l'écran de la session.
//
//   npm run e2e -- [--smoke] [--native] [--release] [--file <ts>] [--mode auto|sw] [options wdio]
//
// --smoke    : parcours réduit (10 sauts, 10 pas de chaque côté) ; fichier par défaut hevc_720p_main10
// --native   : écran de la session (Wayland/X11) au lieu de Xvfb 1920×1080
// --release  : binaire target/release (défaut : target/debug)
// --file     : fichier TS à mesurer (défaut hors --smoke : uhd_big du corpus)
// --mode     : préférence de décodage (auto = VA-API si possible, sw = logiciel)
// Les autres options (ex. --spec e2e/transport.e2e.ts) sont passées à `wdio run`.
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const rest = [];
const env = { ...process.env };
let native = false;
let release = false;
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  if (a === "--smoke") env.MA_BENCH_SMOKE = "1";
  else if (a === "--native") native = true;
  else if (a === "--release") release = true;
  else if (a === "--file") env.MA_BENCH_FILE = path.resolve(args[++i]);
  else if (a === "--mode") env.MA_BENCH_MODE = args[++i];
  else rest.push(a);
}
const corpus = env.MA_CORPUS_DIR ?? path.join(os.homedir(), "media-analyzer-corpus/generated");
env.MA_BENCH_FILE ??= path.join(corpus, env.MA_BENCH_SMOKE ? "hevc_720p_main10.ts" : "uhd_big.ts");
env.MA_BENCH_APP ??= path.resolve(here, `../src-tauri/target/${release ? "release" : "debug"}/transport-bench`);
env.MA_BENCH_DISPLAY = native ? (env.WAYLAND_DISPLAY ? "wayland" : "x11") : "xvfb";
if (!native) {
  // Sinon GTK préfère la session Wayland à l'écran Xvfb désigné par DISPLAY.
  delete env.WAYLAND_DISPLAY;
  env.GDK_BACKEND = "x11";
}
// En mode e2e, le fichier est ouvert par le test, pas au démarrage.
const wdio = ["wdio", "run", path.join(here, "wdio.conf.ts"), ...rest];
const [cmd, cmdArgs] = native
  ? ["npx", wdio]
  : ["xvfb-run", ["-a", "-s", "-screen 0 1920x1080x24", "npx", ...wdio]];
console.log(`banc : ${env.MA_BENCH_FILE} sur ${env.MA_BENCH_DISPLAY}, application ${env.MA_BENCH_APP}`);
const r = spawnSync(cmd, cmdArgs, { stdio: "inherit", env });
process.exit(r.status ?? 1);
