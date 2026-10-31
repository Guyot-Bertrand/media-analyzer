// Rendu WebGL2 d'une image YUV 4:2:0 reçue par le protocole frame:// (4 layouts, BT.709, plage
// limitée), agrandie au plus proche voisin dans un rectangle du canvas, et relecture du numéro
// incrusté sur les pixels du canvas.

/** 0 Planar8, 1 Planar10 (u16, bits de poids faible), 2 NV12, 3 P010 (u16, bits de poids fort). */
export type Layout = 0 | 1 | 2 | 3;

export interface Frame {
  layout: Layout;
  width: number;
  height: number;
  /** Strides en octets, un par plan. */
  strides: number[];
  planes: Uint8Array[];
}

/** Rectangle en pixels (origine en haut à gauche). */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Size {
  w: number;
  h: number;
}

/** Erreur transmise par le moteur dans l'enveloppe `MAE1`. */
export class EngineError extends Error {}

function magic(v: DataView): string {
  return String.fromCharCode(v.getUint8(0), v.getUint8(1), v.getUint8(2), v.getUint8(3));
}

/**
 * Décode la réponse : en-tête de 32 octets petit-boutiste (`MAF1`, layout, largeur, hauteur,
 * nombre de plans, 3 strides) puis les plans ; ou enveloppe d'erreur `MAE1` + JSON.
 */
export function parseFrame(buf: ArrayBuffer): Frame {
  const v = new DataView(buf);
  if (buf.byteLength < 4) throw new Error(`réponse trop courte (${buf.byteLength} octets)`);
  const m = magic(v);
  if (m === "MAE1") {
    const json = new TextDecoder().decode(new Uint8Array(buf, 4));
    throw new EngineError((JSON.parse(json) as { error: string }).error);
  }
  if (m !== "MAF1" || buf.byteLength < 32) throw new Error(`en-tête invalide : ${m}`);
  const layout = v.getUint32(4, true);
  if (layout > 3) throw new Error(`layout inconnu : ${layout}`);
  const width = v.getUint32(8, true);
  const height = v.getUint32(12, true);
  const n = v.getUint32(16, true);
  const strides = [0, 1, 2].map((i) => v.getUint32(20 + 4 * i, true));
  const semi = layout >= 2;
  if (n !== (semi ? 2 : 3)) throw new Error(`${n} plans pour le layout ${layout}`);
  const ch = Math.ceil(height / 2);
  const rows = [height, ch, ch];
  const planes: Uint8Array[] = [];
  let off = 32;
  for (let i = 0; i < n; i++) {
    const len = strides[i] * rows[i];
    planes.push(new Uint8Array(buf, off, len));
    off += len;
  }
  if (off !== buf.byteLength) throw new Error(`taille incohérente : ${off} attendus, ${buf.byteLength} reçus`);
  return { layout: layout as Layout, width, height, strides, planes };
}

const VS = `#version 300 es
in vec2 pos;
void main() { gl_Position = vec4(pos, 0.0, 1.0); }`;

// Agrandissement (ou réduction) au plus proche voisin : chaque pixel du rectangle de destination
// lit le texel entier correspondant, sans filtrage.
const FS = `#version 300 es
precision highp float;
precision highp int;
precision highp usampler2D;
out vec4 color;
uniform usampler2D texY;
uniform usampler2D texU; // U, ou UV entrelacés (semi-planaire)
uniform usampler2D texV;
uniform int pixLayout;
uniform ivec4 dst;       // x, y, w, h dans le canvas (origine en haut à gauche)
uniform ivec2 canvasSize;
uniform ivec2 texSize;
// Valeur ramenée à l'échelle 8 bits.
float to8(uint v) {
  if (pixLayout == 0 || pixLayout == 2) return float(v);
  if (pixLayout == 1) return float(v) / 4.0;
  return float(v >> 6u) / 4.0;
}
void main() {
  ivec2 c = ivec2(gl_FragCoord.xy);
  c.y = canvasSize.y - 1 - c.y;
  ivec2 r = c - dst.xy;
  if (r.x < 0 || r.y < 0 || r.x >= dst.z || r.y >= dst.w) { color = vec4(0.0, 0.0, 0.0, 1.0); return; }
  ivec2 p = min(r * texSize / dst.zw, texSize - 1);
  float y = to8(texelFetch(texY, p, 0).r);
  // Chroma : échantillon co-situé en haut à gauche (p / 2), sans le décalage vertical d'un
  // demi-pixel de la convention MPEG-2 4:2:0 ; sans effet visible au plus proche voisin (à revoir
  // en M6 pour un rendu filtré).
  float u; float v;
  if (pixLayout >= 2) {
    uvec2 uv = texelFetch(texU, p / 2, 0).rg;
    u = to8(uv.r); v = to8(uv.g);
  } else {
    u = to8(texelFetch(texU, p / 2, 0).r);
    v = to8(texelFetch(texV, p / 2, 0).r);
  }
  // BT.709, plage limitée.
  float Y = (y - 16.0) / 219.0;
  float Cb = (u - 128.0) / 224.0;
  float Cr = (v - 128.0) / 224.0;
  color = vec4(clamp(vec3(Y + 1.5748 * Cr, Y - 0.1873 * Cb - 0.4681 * Cr, Y + 1.8556 * Cb), 0.0, 1.0), 1.0);
}`;

export class YuvRenderer {
  readonly gl: WebGL2RenderingContext;
  readonly canvas: HTMLCanvasElement;
  private prog: WebGLProgram;
  private tex: WebGLTexture[];
  private loc: Record<string, WebGLUniformLocation | null> = {};

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const gl = canvas.getContext("webgl2", { preserveDrawingBuffer: true, antialias: false, alpha: false });
    if (!gl) throw new Error("WebGL2 indisponible");
    this.gl = gl;
    this.prog = this.link();
    this.tex = [0, 1, 2].map(() => gl.createTexture()!);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const pos = gl.getAttribLocation(this.prog, "pos");
    gl.enableVertexAttribArray(pos);
    gl.vertexAttribPointer(pos, 2, gl.FLOAT, false, 0, 0);
    gl.useProgram(this.prog);
    for (const n of ["texY", "texU", "texV", "pixLayout", "dst", "canvasSize", "texSize"]) {
      this.loc[n] = gl.getUniformLocation(this.prog, n);
    }
    gl.uniform1i(this.loc.texY, 0);
    gl.uniform1i(this.loc.texU, 1);
    gl.uniform1i(this.loc.texV, 2);
    for (const t of this.tex) {
      gl.bindTexture(gl.TEXTURE_2D, t);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    }
  }

  private link(): WebGLProgram {
    const gl = this.gl;
    const sh = (type: number, src: string) => {
      const s = gl.createShader(type)!;
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? "shader");
      return s;
    };
    const p = gl.createProgram()!;
    gl.attachShader(p, sh(gl.VERTEX_SHADER, VS));
    gl.attachShader(p, sh(gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p) ?? "link");
    return p;
  }

  private upload(unit: number, data: Uint8Array, stride: number, w: number, h: number, comps: 1 | 2, wide: boolean) {
    const gl = this.gl;
    const texel = comps * (wide ? 2 : 1);
    if (stride % texel !== 0) throw new Error(`stride ${stride} non multiple de ${texel}`);
    gl.activeTexture(gl.TEXTURE0 + unit);
    gl.bindTexture(gl.TEXTURE_2D, this.tex[unit]);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.pixelStorei(gl.UNPACK_ROW_LENGTH, stride / texel);
    const fmt = comps === 1 ? (wide ? gl.R16UI : gl.R8UI) : wide ? gl.RG16UI : gl.RG8UI;
    const ext = comps === 1 ? gl.RED_INTEGER : gl.RG_INTEGER;
    const type = wide ? gl.UNSIGNED_SHORT : gl.UNSIGNED_BYTE;
    const src = wide ? new Uint16Array(data.buffer, data.byteOffset, data.byteLength / 2) : data;
    gl.texImage2D(gl.TEXTURE_2D, 0, fmt, w, h, 0, ext, type, src);
  }

  /** Dessine `f` dans le rectangle `dst` du canvas et attend la fin du rendu (`gl.finish`). */
  draw(f: Frame, dst: Rect) {
    const gl = this.gl;
    const wide = f.layout === 1 || f.layout === 3;
    const semi = f.layout >= 2;
    const cw = Math.ceil(f.width / 2);
    const ch = Math.ceil(f.height / 2);
    this.upload(0, f.planes[0], f.strides[0], f.width, f.height, 1, wide);
    if (semi) {
      this.upload(1, f.planes[1], f.strides[1], cw, ch, 2, wide);
    } else {
      this.upload(1, f.planes[1], f.strides[1], cw, ch, 1, wide);
      this.upload(2, f.planes[2], f.strides[2], cw, ch, 1, wide);
    }
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.useProgram(this.prog);
    // texV n'est pas lue en semi-planaire : elle pointe sur l'unité de UV (texture entière valide).
    gl.uniform1i(this.loc.texV, semi ? 1 : 2);
    gl.uniform1i(this.loc.pixLayout, f.layout);
    gl.uniform4i(this.loc.dst, dst.x, dst.y, dst.w, dst.h);
    gl.uniform2i(this.loc.canvasSize, this.canvas.width, this.canvas.height);
    gl.uniform2i(this.loc.texSize, f.width, f.height);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    gl.finish();
  }

  /** Moteur de rendu WebGL (nom démasqué si l'extension existe ; WebKitGTK peut le masquer). */
  renderer(): { renderer: string; unmasked: string | null; vendor: string } {
    const gl = this.gl;
    const ext = gl.getExtension("WEBGL_debug_renderer_info");
    return {
      renderer: String(gl.getParameter(gl.RENDERER)),
      unmasked: ext ? String(gl.getParameter(ext.UNMASKED_RENDERER_WEBGL)) : null,
      vendor: String(gl.getParameter(gl.VENDOR)),
    };
  }

  /**
   * Numéro incrusté relu sur les pixels du canvas, avec l'algorithme de `read_frame_code_bits`
   * (ma-testkit) : 16 blocs de `full.w / 16` sur la largeur, bande de `full.h / 24` en haut ; le
   * centre de chaque bloc est projeté de l'image entière vers la région `region`, puis vers le
   * rectangle `dst` où elle est dessinée. Seuls les bits dont le centre tombe dans la région sont
   * lus (`mask`). La luma est recalculée en plage limitée depuis le RVB (seuil 128).
   */
  readCode(region: Rect, full: Size, dst: Rect): { code: number; mask: number } | null {
    const BITS = 16;
    const bw = Math.floor(full.w / BITS);
    const bh = Math.floor(full.h / 24);
    if (bw === 0 || bh === 0 || region.w === 0 || region.h === 0 || dst.w === 0 || dst.h === 0) return null;
    const cy = Math.floor(bh / 2);
    if (cy < region.y || cy >= region.y + region.h) return null;
    const map = (f: number, origin: number, rlen: number, size: number) =>
      Math.min(Math.floor(((f - origin) * size) / rlen), size - 1);
    const iy = map(cy, region.y, region.h, dst.h);
    const gl = this.gl;
    const row = new Uint8Array(this.canvas.width * 4);
    gl.readPixels(0, this.canvas.height - 1 - (dst.y + iy), this.canvas.width, 1, gl.RGBA, gl.UNSIGNED_BYTE, row);
    let code = 0;
    let mask = 0;
    for (let i = 0; i < BITS; i++) {
      const cx = i * bw + Math.floor(bw / 2);
      if (cx < region.x || cx >= region.x + region.w) continue;
      const x = dst.x + map(cx, region.x, region.w, dst.w);
      const [r, g, b] = [row[4 * x], row[4 * x + 1], row[4 * x + 2]];
      const luma = 16 + ((0.2126 * r + 0.7152 * g + 0.0722 * b) * 219) / 255;
      if (luma >= 128) code |= 1 << i;
      mask |= 1 << i;
    }
    return mask === 0 ? null : { code, mask };
  }
}
