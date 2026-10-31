// Banc interface : une seule vue, canvas WebGL2 de 1920×1080, navigation exposée à WebDriver par
// `window.bench`. Une seule demande explicite en vol (R8) : une commande pendant une autre échoue.
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { EngineError, parseFrame, YuvRenderer, type Rect, type Size } from "./gl";

/** Taille interne du canvas (zone d'affichage du banc). */
const CANVAS: Size = { w: 1920, h: 1080 };

type View = "fit" | "1to1";

interface OpenInfo {
  name: string;
  frames: number;
  width: number;
  height: number;
  code_modulo: number;
}

interface ViewportDto {
  x: number;
  y: number;
  w: number;
  h: number;
  ow: number;
  oh: number;
}

/** Durées (ms) de la dernière commande. */
export interface Timing {
  /** Demande → `drawArrays` terminé (`gl.finish`). */
  total: number;
  /** Demande → octets de l'image reçus (`fetch` + `arrayBuffer`). */
  fetch: number;
  /** Envoi des textures et dessin, jusqu'à `gl.finish`. */
  draw: number;
  /** Attente de l'image dans le moteur (`FrameService::request`), mesurée côté Rust. */
  backend: number;
  /** Octets reçus. */
  bytes: number;
}

/** Résultat d'une commande de navigation. */
export interface Shown {
  pres: number;
  /** Numéro relu sur le canvas (bits de `mask` seulement), ou null s'il est illisible. */
  code: number | null;
  mask: number;
  /** Valeur attendue sur les bits de `mask` : `(pres % code_modulo) & mask`. */
  expected: number;
  corrupt: boolean;
  layout: number;
  out: Size;
  timing: Timing;
}

export interface Bench {
  open(path: string, mode?: "auto" | "sw"): Promise<OpenInfo>;
  goto(n: number): Promise<Shown>;
  step(d: 1 | -1): Promise<Shown>;
  setView(v: View): Promise<ViewportDto>;
  lastTiming(): Timing | null;
  current(): number | null;
  info(): OpenInfo | null;
  stats(): Promise<unknown>;
  rss(): Promise<unknown>;
}

declare global {
  interface Window {
    bench: Bench;
  }
}

const canvas = document.getElementById("view") as HTMLCanvasElement;
const status = document.getElementById("status") as HTMLDivElement;
// Toute erreur non rattrapée (WebGL2 absent, shader…) reste visible dans la page.
window.addEventListener("error", (e) => {
  status.textContent = `erreur : ${e.message}`;
});
canvas.width = CANVAS.w;
canvas.height = CANVAS.h;
const renderer = new YuvRenderer(canvas);

let info: OpenInfo | null = null;
let view: View = "fit";
let vp: ViewportDto | null = null;
let current: number | null = null;
let last: Timing | null = null;
let busy = false;

/** Rectangle du canvas où la sortie `out` est dessinée. */
function destination(out: Size): Rect {
  if (view === "1to1") return { x: 0, y: 0, w: out.w, h: out.h };
  // Ajusté : l'image entière remplit la zone, ratio conservé (agrandissement au plus proche voisin).
  const s = Math.min(CANVAS.w / out.w, CANVAS.h / out.h);
  const w = Math.min(CANVAS.w, Math.round(out.w * s));
  const h = Math.min(CANVAS.h, Math.round(out.h * s));
  return { x: Math.floor((CANVAS.w - w) / 2), y: Math.floor((CANVAS.h - h) / 2), w, h };
}

async function show(pres: number): Promise<Shown> {
  if (!info || !vp) throw new Error("aucun fichier ouvert");
  if (busy) throw new Error("une demande est déjà en cours");
  if (pres < 0 || pres >= info.frames) throw new Error(`image ${pres} hors du fichier (0..${info.frames - 1})`);
  busy = true;
  try {
    const q = `x=${vp.x}&y=${vp.y}&w=${vp.w}&h=${vp.h}&ow=${vp.ow}&oh=${vp.oh}`;
    const t0 = performance.now();
    const res = await fetch(`${convertFileSrc(String(pres), "frame")}?${q}`);
    const buf = await res.arrayBuffer();
    const t1 = performance.now();
    const f = parseFrame(buf);
    const dst = destination({ w: f.width, h: f.height });
    renderer.draw(f, dst);
    const t2 = performance.now();
    last = {
      total: t2 - t0,
      fetch: t1 - t0,
      draw: t2 - t1,
      backend: Number(res.headers.get("x-backend-ms") ?? Number.NaN),
      bytes: buf.byteLength,
    };
    current = pres;
    const full = { w: info.width, h: info.height };
    const read = renderer.readCode({ x: vp.x, y: vp.y, w: vp.w, h: vp.h }, full, dst);
    const mask = read?.mask ?? 0;
    status.textContent = `${info.name} · image ${pres} / ${info.frames - 1} · ${view} · ${f.width}×${f.height} · ${last.total.toFixed(1)} ms`;
    return {
      pres,
      code: read?.code ?? null,
      mask,
      expected: (pres % info.code_modulo) & mask,
      corrupt: res.headers.get("x-corrupt") === "1",
      layout: f.layout,
      out: { w: f.width, h: f.height },
      timing: last,
    };
  } catch (e) {
    status.textContent = `erreur : ${e instanceof EngineError ? "moteur : " : ""}${String(e)}`;
    throw e;
  } finally {
    busy = false;
  }
}

async function setView(v: View): Promise<ViewportDto> {
  vp = await invoke<ViewportDto>("viewport", { view: v, areaW: CANVAS.w, areaH: CANVAS.h });
  view = v;
  return vp;
}

window.bench = {
  async open(path, mode = "auto") {
    info = null;
    current = null;
    info = await invoke<OpenInfo>("open", { path, mode });
    await setView(view);
    status.textContent = `${info.name} : ${info.frames} images ${info.width}×${info.height}`;
    return info;
  },
  goto: (n) => show(n),
  step: (d) => {
    if (current === null) return Promise.reject(new Error("aucune image affichée"));
    return show(current + d);
  },
  setView,
  lastTiming: () => last,
  current: () => current,
  info: () => info,
  stats: () => invoke("engine_stats"),
  rss: () => invoke("rss"),
};

// Lancement à la main : fichier de MA_BENCH_OPEN, flèches gauche/droite, touche V (vue).
window.addEventListener("keydown", (e) => {
  if (current === null || busy) return;
  const run = (p: Promise<unknown>) => p.catch(() => undefined);
  if (e.key === "ArrowRight") run(window.bench.step(1));
  if (e.key === "ArrowLeft") run(window.bench.step(-1));
  if (e.key === "v") run(setView(view === "fit" ? "1to1" : "fit").then(() => show(current ?? 0)));
});

invoke<string | null>("initial_file").then(async (p) => {
  if (!p) return;
  try {
    await window.bench.open(p);
    await show(0);
  } catch (e) {
    status.textContent = `erreur : ${String(e)}`;
  }
});
