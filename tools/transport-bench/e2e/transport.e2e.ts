// Banc interface du transport d'image : reprend les protocoles de performance.yaml côté
// interface. Ce test MESURE ; il n'est pas lié par `covers` (la couverture e2e des exigences PERF
// se fera en M7 sur l'application réelle).
//
// - PERF.SEEK.01 : 100 sauts aléatoires (graine fixe, mêmes positions que engine_bench), Ajusté.
// - PERF.STEP.01 : 200 pas avant, en Ajusté puis en 1:1 (région visible seulement).
// - PERF.STEP.02 : 200 pas arrière, en Ajusté.
// Chaque série est passée avec un intervalle de 0 ms puis de 200 ms entre la réception d'une image
// et la commande suivante ; l'intervalle s'applique aussi après le placement. Le premier pas après
// le placement (démarrage à froid) est publié à part (`first_ms`, R18) ; p50/p95/max portent sur
// les pas suivants. Un pas est compté de la commande (`bench.step`) jusqu'au dessin terminé
// (`gl.finish`). Chaque image est vérifiée par son numéro incrusté relu sur le canvas ; en 1:1 sur
// une image plus large que la zone, seuls les bits couverts par la région sont comparés.
// Une seule demande explicite en vol (R8). Résultats : target/bench/ui-*.json (racine du dépôt).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
// Déclare aussi window.bench (declare global de src/main.ts).
import type {} from "../src/main";

const here = path.dirname(fileURLToPath(import.meta.url));
const smoke = process.env.MA_BENCH_SMOKE === "1";
const file = process.env.MA_BENCH_FILE ?? "";
const mode = (process.env.MA_BENCH_MODE ?? "auto") as "auto" | "sw";
const display = process.env.MA_BENCH_DISPLAY ?? "inconnu";
const JUMPS = smoke ? 10 : 100;
const LAYOUTS = ["Planar8", "Planar10", "SemiPlanar8 (NV12)", "SemiPlanar10 (P010)"];
const STEPS = smoke ? 10 : 200;
const INTERVALS = [0, 200];
/** Graine de engine_bench (xorshift64*), pour tirer les mêmes sauts. */
const SEED = 0x4d41_5f42_454e_4348n;
const MASK64 = (1n << 64n) - 1n;

class Rng {
  constructor(private s: bigint) {}
  below(n: number): number {
    let x = this.s;
    x ^= x >> 12n;
    x = (x ^ (x << 25n)) & MASK64;
    x ^= x >> 27n;
    this.s = x;
    const r = ((x * 0x2545_f491_4f6c_dd1dn) & MASK64) >> 33n;
    return Number(r % BigInt(n));
  }
}

interface Timing {
  total: number;
  fetch: number;
  draw: number;
  backend: number;
  bytes: number;
}
interface Shown {
  pres: number;
  code: number | null;
  mask: number;
  expected: number;
  corrupt: boolean;
  layout: number;
  out: { w: number; h: number };
  timing: Timing;
}
interface OpenInfo {
  name: string;
  frames: number;
  width: number;
  height: number;
  code_modulo: number;
}
interface EngineStats {
  decode_mode: string;
  convert_path: string;
  fell_back: boolean;
  mode: string;
  vaapi_available: boolean;
  vpp_available: boolean;
  full_res_transfers: number;
}

/** Percentile en millièmes, par rang le plus proche vers le haut (comme engine_bench). */
function percentile(v: number[], permille: number): number | null {
  if (v.length === 0) return null;
  const s = [...v].sort((a, b) => a - b);
  const rank = Math.max(1, Math.ceil((permille * s.length) / 1000));
  return s[Math.min(rank, s.length) - 1];
}

const round = (v: number | null) => (v === null ? null : Math.round(v * 1000) / 1000);
const pause = (ms: number) => (ms > 0 ? new Promise((r) => setTimeout(r, ms)) : Promise.resolve());

const goto = async (n: number) => (await browser.execute(async (p: number) => window.bench.goto(p), n)) as unknown as Shown;
const step = async (d: 1 | -1) => (await browser.execute(async (s: 1 | -1) => window.bench.step(s), d)) as unknown as Shown;
const setView = (v: "fit" | "1to1") => browser.execute(async (x: "fit" | "1to1") => window.bench.setView(x), v);
const stats = async () => (await browser.execute(async () => window.bench.stats())) as EngineStats;

/** Vérifie l'image affichée : bon numéro (sur les bits lisibles), non corrompue. */
function check(s: Shown, pres: number): number {
  if (s.pres !== pres) throw new Error(`image ${s.pres} affichée, ${pres} demandée`);
  if (s.code === null) throw new Error(`image ${pres} : numéro incrusté illisible sur le canvas`);
  if (s.code !== s.expected) {
    throw new Error(`image ${pres} : numéro lu ${s.code.toString(16)}, attendu ${s.expected.toString(16)} (masque ${s.mask.toString(16)})`);
  }
  if (s.corrupt) throw new Error(`image ${pres} : marquée corrompue`);
  return s.mask;
}

interface Series {
  name: string;
  view: "fit" | "1to1";
  interval_ms: number;
  n: number;
  p50_ms: number | null;
  p95_ms: number | null;
  max_ms: number | null;
  first_ms: number | null;
  fetch_p50_ms: number | null;
  draw_p50_ms: number | null;
  backend_p50_ms: number | null;
  bytes_per_frame: number;
  out: { w: number; h: number };
  layout: number;
  layout_name: string;
  bits_checked: string;
  /** Mode avant la série : un repli pendant la série se voit par différence avec `mode`. */
  mode_before: string;
  fell_back: boolean;
  decode_mode: string;
  convert_path: string;
  mode: string;
  full_res_transfers: number;
}

describe("banc interface du transport d'image", () => {
  let info: OpenInfo;
  let openMs = 0;
  const series: Series[] = [];

  before(async () => {
    if (!file) throw new Error("MA_BENCH_FILE non défini (lancer par npm run e2e)");
    if (!fs.existsSync(file)) throw new Error(`corpus absent : lancer cargo xtask corpus gen (${file})`);
    try {
      await browser.waitUntil(async () => browser.execute(() => typeof window.bench !== "undefined"));
    } catch {
      const page = await browser.execute(() => `${location.href} : ${document.body?.innerText ?? ""}`);
      throw new Error(`window.bench absent ; page ${page}`);
    }
    const t0 = Date.now();
    info = (await browser.execute(async (p: string, m: "auto" | "sw") => window.bench.open(p, m), file, mode)) as OpenInfo;
    openMs = Date.now() - t0;
    // Échauffement (ouverture du décodeur, graphes GPU), hors mesure.
    check(await goto(0), 0);
    // Capture de recette (une capture X d'une fenêtre WebKitGTK sous Xvfb est blanche, 04 §3.3).
    if (process.env.MA_BENCH_SHOT) await browser.saveScreenshot(process.env.MA_BENCH_SHOT);
  });

  /** Mesure les commandes `cmds` ; si `splitFirst`, la première est publiée à part. */
  async function measure(
    name: string,
    view: "fit" | "1to1",
    interval: number,
    cmds: Array<{ pres: number; run: () => Promise<Shown> }>,
    splitFirst: boolean,
  ) {
    const before = await stats();
    const total: number[] = [];
    const fetchMs: number[] = [];
    const drawMs: number[] = [];
    const backendMs: number[] = [];
    let bytes = 0;
    let mask = 0xffff;
    let last: Shown | undefined;
    for (const c of cmds) {
      last = await c.run();
      mask &= check(last, c.pres);
      total.push(last.timing.total);
      fetchMs.push(last.timing.fetch);
      drawMs.push(last.timing.draw);
      backendMs.push(last.timing.backend);
      bytes += last.timing.bytes;
      await pause(interval);
    }
    const first = splitFirst ? total.shift()! : null;
    if (splitFirst) {
      fetchMs.shift();
      drawMs.shift();
      backendMs.shift();
    }
    const after = await stats();
    const s: Series = {
      name,
      view,
      interval_ms: interval,
      n: total.length,
      p50_ms: round(percentile(total, 500)),
      p95_ms: round(percentile(total, 950)),
      max_ms: round(percentile(total, 1000)),
      first_ms: round(first),
      fetch_p50_ms: round(percentile(fetchMs, 500)),
      draw_p50_ms: round(percentile(drawMs, 500)),
      backend_p50_ms: round(percentile(backendMs, 500)),
      bytes_per_frame: Math.round(bytes / Math.max(1, cmds.length)),
      out: last!.out,
      layout: last!.layout,
      layout_name: LAYOUTS[last!.layout] ?? "inconnu",
      bits_checked: `0x${mask.toString(16)}`,
      decode_mode: after.decode_mode,
      convert_path: after.convert_path,
      mode: after.mode,
      mode_before: before.mode,
      fell_back: after.fell_back,
      full_res_transfers: after.full_res_transfers - before.full_res_transfers,
    };
    series.push(s);
    console.log(
      `${name} ${view} ${interval} ms : n ${s.n}, p50 ${s.p50_ms?.toFixed(1)} p95 ${s.p95_ms?.toFixed(1)} max ${s.max_ms?.toFixed(1)}` +
        ` 1er ${s.first_ms?.toFixed(1) ?? "-"} ms, ${s.mode}, ${s.layout_name}, bits ${s.bits_checked}`,
    );
  }

  for (const interval of INTERVALS) {
    it(`sauts aléatoires, Ajusté, intervalle ${interval} ms`, async () => {
      await setView("fit");
      const rng = new Rng(SEED);
      const jumps = Array.from({ length: JUMPS }, () => rng.below(info.frames));
      await measure("jump", "fit", interval, jumps.map((p) => ({ pres: p, run: () => goto(p) })), false);
    });

    const steps = () => Math.min(STEPS, Math.floor((info.frames - 1) / 2));
    const stepSeries = async (name: string, view: "fit" | "1to1", from: number, d: 1 | -1, interval: number) => {
      await setView(view);
      // Placement hors mesure, puis l'intervalle comme après toute image.
      check(await goto(from), from);
      await pause(interval);
      const cmds = Array.from({ length: steps() }, (_, i) => ({ pres: from + d * (i + 1), run: () => step(d) }));
      await measure(name, view, interval, cmds, true);
    };

    for (const view of ["fit", "1to1"] as const) {
      it(`pas avant, ${view === "fit" ? "Ajusté" : "1:1"}, intervalle ${interval} ms`, async () => {
        const from = Math.min(Math.floor(info.frames / 4), info.frames - 1 - steps());
        await stepSeries("step_forward", view, from, 1, interval);
      });
    }

    it(`pas arrière, Ajusté, intervalle ${interval} ms`, async () => {
      const from = Math.min(Math.max(Math.floor(info.frames / 2), steps()), info.frames - 1);
      await stepSeries("step_backward", "fit", from, -1, interval);
    });
  }

  after(async () => {
    if (!info) return;
    const rss = (await browser.execute(async () => window.bench.rss())) as {
      total_kib: number;
      pss_total_kib: number;
      processes: Array<{ pid: number; name: string; rss_kib: number; pss_kib: number; gpu_render_node: boolean }>;
    };
    const renderer = await browser.execute(() => window.bench.renderer());
    const engine = await stats();
    const result = {
      file,
      name: info.name,
      frames: info.frames,
      frame_size: [info.width, info.height],
      canvas: [1920, 1080],
      display,
      smoke,
      seed: SEED.toString(),
      open_ms: openMs,
      mode: engine.mode,
      decode_mode: engine.decode_mode,
      convert_path: engine.convert_path,
      fell_back: engine.fell_back,
      vaapi_available: engine.vaapi_available,
      vpp_available: engine.vpp_available,
      full_res_transfers: engine.full_res_transfers,
      renderer,
      // WebKitGTK masque le nom du moteur WebGL (« Apple GPU ») : indice par processus ci-dessous.
      gpu_render_node: Object.fromEntries(rss.processes.map((p) => [p.name, p.gpu_render_node])),
      // Parcours complet : toutes les séries passées (sinon le JSON porte des séries partielles).
      complete: series.length === INTERVALS.length * 4,
      expected_series: INTERVALS.length * 4,
      rss_total_kib: rss.total_kib,
      pss_total_kib: rss.pss_total_kib,
      rss_processes: rss.processes,
      series,
    };
    const dir = path.resolve(here, "../../../target/bench");
    fs.mkdirSync(dir, { recursive: true });
    const out = path.join(dir, `ui-${display}-${info.name}-${mode}${smoke ? "-smoke" : ""}-${Date.now()}.json`);
    fs.writeFileSync(out, `${JSON.stringify(result, null, 2)}\n`);
    console.log(
      `RSS (somme de ${rss.processes.length} processus) : ${Math.round(rss.total_kib / 1024)} Mo, PSS ${Math.round(rss.pss_total_kib / 1024)} Mo ; ` +
        `mode ${engine.mode} ; rendu ${renderer.unmasked ?? renderer.renderer} ; nœud DRM ` +
        rss.processes.map((p) => `${p.name}=${p.gpu_render_node ? "oui" : "non"}`).join(" "),
    );
    console.log(`résultats : ${out}`);
  });
});
