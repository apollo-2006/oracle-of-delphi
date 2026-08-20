// The Apollo Decree — the confirmation modal for irreversible actions.
//
// Aesthetically it sits inside the Oracle of Delphi world (glass-morphism, the same
// dark field) but shifts the accent from system-cyan to Apollo's gold: the god
// of light, prophecy, and the sun. A confirmation is a moment of judgment, so
// it's framed as an oracle's decree — a laurel-crowned sun over the action,
// and two verdicts: SANCTION (let it pass) or FORBID (stay its hand). A slowly
// closing ring of light marks the time before the decree lapses to denial.

export interface DecreeRequest {
  requestId: string;
  prompt: string;
  severity: string;
}

type Verdict = (requestId: string, allow: boolean) => void;

const LAPSE_SECONDS = 120;

export class ApolloModal {
  private root: HTMLElement;
  private promptEl: HTMLElement;
  private sevEl: HTMLElement;
  private ringEl: SVGCircleElement;
  private onVerdict: Verdict;
  private current: DecreeRequest | null = null;
  private timer: number | null = null;
  private ringLen = 0;

  constructor(onVerdict: Verdict) {
    this.onVerdict = onVerdict;
    this.root = this.build();
    document.body.appendChild(this.root);
    this.promptEl = this.root.querySelector(".decree-action") as HTMLElement;
    this.sevEl = this.root.querySelector(".decree-sev") as HTMLElement;
    this.ringEl = this.root.querySelector(".lapse-ring") as unknown as SVGCircleElement;
    this.ringLen = this.ringEl.getTotalLength();
    this.ringEl.style.strokeDasharray = `${this.ringLen}`;

    (this.root.querySelector(".sanction") as HTMLElement).addEventListener("click", () =>
      this.decide(true),
    );
    (this.root.querySelector(".forbid") as HTMLElement).addEventListener("click", () =>
      this.decide(false),
    );
    // Keyboard: Y sanctions, N/Esc forbids.
    window.addEventListener("keydown", (e) => {
      if (!this.current) return;
      if (e.key === "y" || e.key === "Y") this.decide(true);
      if (e.key === "n" || e.key === "N" || e.key === "Escape") this.decide(false);
    });
  }

  /** Raise the decree for an action. */
  show(req: DecreeRequest): void {
    this.current = req;
    this.promptEl.textContent = req.prompt;
    this.sevEl.textContent = req.severity.toUpperCase();
    this.root.classList.add("visible");
    this.startLapse();
  }

  private decide(allow: boolean): void {
    if (!this.current) return;
    const id = this.current.requestId;
    this.current = null;
    this.stopLapse();
    this.root.classList.remove("visible");
    this.onVerdict(id, allow);
  }

  private startLapse(): void {
    this.stopLapse();
    const start = performance.now();
    const tick = () => {
      if (!this.current) return;
      const elapsed = (performance.now() - start) / 1000;
      const frac = Math.max(0, 1 - elapsed / LAPSE_SECONDS);
      this.ringEl.style.strokeDashoffset = `${this.ringLen * (1 - frac)}`;
      if (frac <= 0) {
        this.decide(false); // decree lapses → forbid
        return;
      }
      this.timer = requestAnimationFrame(tick);
    };
    this.timer = requestAnimationFrame(tick);
  }

  private stopLapse(): void {
    if (this.timer !== null) cancelAnimationFrame(this.timer);
    this.timer = null;
  }

  private build(): HTMLElement {
    const el = document.createElement("div");
    el.id = "apolloModal";
    el.innerHTML = `
      <div class="decree-card">
        <div class="decree-emblem">
          ${sunLaurelSvg()}
        </div>
        <div class="decree-kicker">◈ APOLLO&nbsp;&middot;&nbsp;ORACLE OF DELPHI ◈</div>
        <h1 class="decree-title">A Decree Awaits Thy Seal</h1>
        <p class="decree-sub">
          Oracle of Delphi seeks to perform an <span class="decree-sev">irreversible</span> act.
          It shall not proceed without thy word.
        </p>
        <div class="decree-action"></div>
        <div class="decree-buttons">
          <button class="forbid">⊘ Forbid <span class="key">N</span></button>
          <button class="sanction">☀ Sanction <span class="key">Y</span></button>
        </div>
      </div>
    `;
    return el;
  }
}

/** A stylized sun crowned with laurel — Apollo's emblem — as inline SVG. */
function sunLaurelSvg(): string {
  const rays: string[] = [];
  const cx = 60;
  const cy = 60;
  for (let i = 0; i < 24; i++) {
    const a = (i / 24) * Math.PI * 2;
    const r1 = 26;
    const r2 = i % 2 === 0 ? 40 : 34;
    rays.push(
      `<line x1="${cx + Math.cos(a) * r1}" y1="${cy + Math.sin(a) * r1}" x2="${
        cx + Math.cos(a) * r2
      }" y2="${cy + Math.sin(a) * r2}" />`,
    );
  }
  // Two laurel arcs (left + right) built from small leaves.
  const leaves = (side: number): string => {
    const out: string[] = [];
    for (let i = 0; i < 6; i++) {
      const t = 0.15 + i * 0.12;
      const a = side * (Math.PI * 0.5 + t * Math.PI * 0.9);
      const lx = cx + Math.cos(a) * 50;
      const ly = cy + Math.sin(a) * 50;
      const rot = (a * 180) / Math.PI + (side > 0 ? 90 : -90);
      out.push(
        `<ellipse cx="${lx}" cy="${ly}" rx="7" ry="3" transform="rotate(${rot} ${lx} ${ly})" />`,
      );
    }
    return out.join("");
  };
  return `
    <svg viewBox="0 0 120 120" width="120" height="120" aria-hidden="true">
      <g class="sun-rays">${rays.join("")}</g>
      <circle class="sun-core" cx="${cx}" cy="${cy}" r="20" />
      <circle class="sun-inner" cx="${cx}" cy="${cy}" r="12" />
      <g class="laurel">${leaves(1)}${leaves(-1)}</g>
      <circle class="lapse-track" cx="${cx}" cy="${cy}" r="52" />
      <circle class="lapse-ring" cx="${cx}" cy="${cy}" r="52"
              transform="rotate(-90 ${cx} ${cy})" />
    </svg>
  `;
}
