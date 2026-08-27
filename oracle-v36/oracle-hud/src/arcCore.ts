// The audio-reactive arc core (architecture §6.2).
//
// A GPU-instanced particle shell on a Fibonacci sphere plus orbital wireframe
// rings. FFT bands are uploaded once per frame into a 64×1 DataTexture; the
// vertex shader samples it — so the per-frame CPU cost is one small texture
// update, never per-instance attribute uploads. Agent state modulates palette
// and drift via uniforms.

import * as THREE from "three";
import { FFT_BANDS, HudState } from "./protocol.js";

const PARTICLE_COUNT = 24000;

interface StateStyle {
  color: THREE.Color;
  drift: number; // angular drift speed
  amp: number; // displacement gain
}

const STATE_STYLES: Record<HudState, StateStyle> = {
  idle: { color: new THREE.Color(0x1fb6ff), drift: 0.05, amp: 0.2 },
  listening: { color: new THREE.Color(0x00e0c6), drift: 0.15, amp: 0.9 },
  thinking: { color: new THREE.Color(0xffb020), drift: 0.6, amp: 0.5 },
  speaking: { color: new THREE.Color(0x7c5cff), drift: 0.25, amp: 1.0 },
  tool: { color: new THREE.Color(0x38f08f), drift: 0.4, amp: 0.6 },
};

const vertexShader = /* glsl */ `
  uniform sampler2D uBands;   // 64x1 R32F, FFT magnitudes 0..1
  uniform float uTime;
  uniform float uAmp;
  uniform float uDrift;
  attribute float aBand;      // which FFT band this instance tracks (0..63)
  attribute float aSeed;      // per-instance noise seed
  varying float vDisp;        // displacement, passed to fragment for emissive

  // cheap hash noise
  float hash(vec3 p) {
    p = fract(p * 0.3183099 + 0.1);
    p *= 17.0;
    return fract(p.x * p.y * p.z * (p.x + p.y + p.z));
  }

  void main() {
    // instanceMatrix column 3 holds the base direction*radius (set on CPU)
    vec3 baseDir = normalize(vec3(instanceMatrix[3]));
    float baseRadius = length(vec3(instanceMatrix[3]));

    float band = texture2D(uBands, vec2((aBand + 0.5) / ${FFT_BANDS}.0, 0.5)).r;
    float noise = hash(baseDir * (2.0 + aSeed) + uTime * uDrift) - 0.5;

    float disp = band * uAmp + noise * 0.08;
    vDisp = disp;

    vec3 pos = baseDir * (baseRadius + disp);
    // rotate slowly around Y for life
    float a = uTime * uDrift * 0.3;
    mat2 rot = mat2(cos(a), -sin(a), sin(a), cos(a));
    pos.xz = rot * pos.xz;

    vec4 mvPosition = modelViewMatrix * vec4(pos, 1.0);
    gl_Position = projectionMatrix * mvPosition;
    gl_PointSize = 2.0 + band * 3.0;
  }
`;

const fragmentShader = /* glsl */ `
  uniform vec3 uColor;
  varying float vDisp;
  void main() {
    // soft round point
    vec2 c = gl_PointCoord - 0.5;
    float d = dot(c, c);
    if (d > 0.25) discard;
    float glow = smoothstep(0.25, 0.0, d);
    // loud instances glow brighter → picked up by the bloom pass
    float emissive = 0.4 + clamp(vDisp, 0.0, 1.0) * 1.6;
    gl_FragColor = vec4(uColor * emissive, glow);
  }
`;

export class ArcCore {
  readonly object: THREE.Group;
  private material: THREE.ShaderMaterial;
  private bandTexture: THREE.DataTexture;
  private bandData: Float32Array<ArrayBuffer>;
  private rings: THREE.LineSegments;
  private targetStyle: StateStyle = STATE_STYLES.idle;
  private currentColor = STATE_STYLES.idle.color.clone();

  constructor() {
    this.object = new THREE.Group();

    // Band texture (64×1, single float channel). Backed by an explicit
    // ArrayBuffer so its type is Float32Array<ArrayBuffer> (TS 5.7+ typed-array
    // generics), which is what three's DataTexture / WebGL upload paths expect.
    this.bandData = new Float32Array(new ArrayBuffer(FFT_BANDS * 4));
    this.bandTexture = new THREE.DataTexture(
      this.bandData,
      FFT_BANDS,
      1,
      THREE.RedFormat,
      THREE.FloatType,
    );
    this.bandTexture.needsUpdate = true;

    this.material = new THREE.ShaderMaterial({
      uniforms: {
        uBands: { value: this.bandTexture },
        uTime: { value: 0 },
        uAmp: { value: this.targetStyle.amp },
        uDrift: { value: this.targetStyle.drift },
        uColor: { value: this.currentColor },
      },
      vertexShader,
      fragmentShader,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    });

    this.object.add(this.buildParticles());
    this.rings = this.buildRings();
    this.object.add(this.rings);
  }

  private buildParticles(): THREE.Points {
    const geo = new THREE.BufferGeometry();
    // Fibonacci sphere directions baked into an instanceMatrix-like attribute.
    // For Points we store position directly and encode base dir*radius there.
    const positions = new Float32Array(PARTICLE_COUNT * 3);
    const bands = new Float32Array(PARTICLE_COUNT);
    const seeds = new Float32Array(PARTICLE_COUNT);
    const golden = Math.PI * (3 - Math.sqrt(5));
    for (let i = 0; i < PARTICLE_COUNT; i++) {
      const y = 1 - (i / (PARTICLE_COUNT - 1)) * 2;
      const r = Math.sqrt(1 - y * y);
      const theta = golden * i;
      const dir = new THREE.Vector3(Math.cos(theta) * r, y, Math.sin(theta) * r);
      const radius = 2.0;
      positions[i * 3] = dir.x * radius;
      positions[i * 3 + 1] = dir.y * radius;
      positions[i * 3 + 2] = dir.z * radius;
      // bass (low band) → poles/inner, treble → equator sparkle
      bands[i] = Math.floor(Math.abs(y) * (FFT_BANDS - 1));
      seeds[i] = Math.random() * 10;
    }
    geo.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    geo.setAttribute("aBand", new THREE.BufferAttribute(bands, 1));
    geo.setAttribute("aSeed", new THREE.BufferAttribute(seeds, 1));

    // We reuse the shader; for Points, instanceMatrix isn't present, so patch
    // the shader to read position as baseDir*radius directly.
    const pointsMat = this.material.clone();
    pointsMat.uniforms = this.material.uniforms; // share uniforms (live updates)
    pointsMat.vertexShader = pointsMat.vertexShader
      .replace(
        "vec3 baseDir = normalize(vec3(instanceMatrix[3]));",
        "vec3 baseDir = normalize(position);",
      )
      .replace("float baseRadius = length(vec3(instanceMatrix[3]));", "float baseRadius = length(position);");
    return new THREE.Points(geo, pointsMat);
  }

  private buildRings(): THREE.LineSegments {
    const ringGeo = new THREE.BufferGeometry();
    const verts: number[] = [];
    const ringCount = 3;
    const seg = 128;
    for (let r = 0; r < ringCount; r++) {
      const radius = 2.6 + r * 0.35;
      const tilt = (r / ringCount) * Math.PI;
      for (let s = 0; s < seg; s++) {
        const a0 = (s / seg) * Math.PI * 2;
        const a1 = ((s + 1) / seg) * Math.PI * 2;
        for (const a of [a0, a1]) {
          const x = Math.cos(a) * radius;
          const z = Math.sin(a) * radius;
          const y = Math.sin(a * 2 + tilt) * 0.15;
          verts.push(x, y * Math.cos(tilt) + z * Math.sin(tilt), z * Math.cos(tilt) - y * Math.sin(tilt));
        }
      }
    }
    ringGeo.setAttribute("position", new THREE.Float32BufferAttribute(verts, 3));
    const mat = new THREE.LineBasicMaterial({
      color: this.currentColor,
      transparent: true,
      opacity: 0.5,
      blending: THREE.AdditiveBlending,
    });
    return new THREE.LineSegments(ringGeo, mat);
  }

  /** Push a new FFT frame (called from the WS handler; cheap). */
  setBands(bands: Float32Array): void {
    const n = Math.min(bands.length, FFT_BANDS);
    for (let i = 0; i < n; i++) this.bandData[i] = bands[i];
    this.bandTexture.needsUpdate = true;
  }

  /** Switch visual state (idle/listening/thinking/speaking/tool). */
  setState(state: HudState): void {
    this.targetStyle = STATE_STYLES[state];
  }

  /** Advance animation; call once per rAF with elapsed seconds. */
  update(elapsed: number, dt: number): void {
    this.material.uniforms.uTime.value = elapsed;
    // ease amp/drift/color toward the target style (no hard cuts)
    const u = this.material.uniforms;
    u.uAmp.value += (this.targetStyle.amp - u.uAmp.value) * Math.min(1, dt * 4);
    u.uDrift.value += (this.targetStyle.drift - u.uDrift.value) * Math.min(1, dt * 4);
    this.currentColor.lerp(this.targetStyle.color, Math.min(1, dt * 3));
    (this.rings.material as THREE.LineBasicMaterial).color.copy(this.currentColor);
  }

  dispose(): void {
    this.material.dispose();
    this.bandTexture.dispose();
  }
}
