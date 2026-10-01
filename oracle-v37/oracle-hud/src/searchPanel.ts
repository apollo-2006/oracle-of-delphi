// Recall — search everything the Oracle remembers, without asking her.
//
// Ctrl+K (or the Recall button) opens a palette over the HUD. Typing searches
// memory directly: core runs the same retrieval the planner's recall uses and
// returns the rows, so results arrive in milliseconds even while the planner
// is unloaded. An empty box shows the most recent things seen on screen.
//
// Results are rendered with textContent only. Window titles and screen
// summaries are text other people wrote (a page picks its own title), so none
// of it is ever interpreted as markup.

import type { SearchHit } from "./protocol.js";

type Send = (query: string, id: number) => void;

const DEBOUNCE_MS = 160;

const KIND_LABEL: Record<string, string> = {
  observation: "seen",
  conversation: "said",
  action: "done",
};

export class SearchPanel {
  private root: HTMLElement;
  private input: HTMLInputElement;
  private list: HTMLElement;
  private status: HTMLElement;
  private send: Send;
  private seq = 0;
  private timer: number | null = null;
  private hits: SearchHit[] = [];
  private selected = 0;
  private lastFocus: Element | null = null;

  constructor(send: Send) {
    this.send = send;
    this.root = this.build();
    document.body.appendChild(this.root);
    this.input = this.root.querySelector(".recall-input") as HTMLInputElement;
    this.list = this.root.querySelector(".recall-list") as HTMLElement;
    this.status = this.root.querySelector(".recall-status") as HTMLElement;

    this.input.addEventListener("input", () => this.schedule());
    this.input.addEventListener("keydown", (e) => this.onKey(e));
    // Click on the dimmed backdrop (not the card) closes.
    this.root.addEventListener("mousedown", (e) => {
      if (e.target === this.root) this.close();
    });
    window.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        this.toggle();
      }
    });
  }

  get isOpen(): boolean {
    return this.root.classList.contains("visible");
  }

  toggle(): void {
    if (this.isOpen) this.close();
    else this.open();
  }

  open(): void {
    this.lastFocus = document.activeElement;
    this.root.classList.add("visible");
    this.input.select();
    this.input.focus();
    this.query(); // immediately: an empty box shows the recent timeline
  }

  close(): void {
    this.root.classList.remove("visible");
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    if (this.lastFocus instanceof HTMLElement) this.lastFocus.focus();
  }

  /** Feed a search_results event in. Stale answers are dropped by id. */
  receive(id: number, items: SearchHit[], note?: string | null): void {
    if (id !== this.seq) return; // another window's query, or one we superseded
    this.hits = items;
    this.selected = 0;
    this.render(note ?? null);
  }

  private schedule(): void {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = window.setTimeout(() => this.query(), DEBOUNCE_MS);
  }

  private query(): void {
    this.timer = null;
    this.seq += 1;
    this.status.textContent = "searching…";
    this.send(this.input.value, this.seq);
  }

  private onKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      this.close();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!this.hits.length) return;
      const step = e.key === "ArrowDown" ? 1 : -1;
      this.selected = (this.selected + step + this.hits.length) % this.hits.length;
      this.highlight();
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (this.timer !== null) {
        // Typed and hit Enter before the debounce fired: search now.
        clearTimeout(this.timer);
        this.query();
      } else {
        this.copy(this.selected);
      }
    }
  }

  private copy(i: number): void {
    const h = this.hits[i];
    if (!h) return;
    const text = h.title ? `${h.title}: ${h.text}` : h.text;
    navigator.clipboard?.writeText(text).then(
      () => (this.status.textContent = "copied"),
      () => (this.status.textContent = "couldn't copy (clipboard blocked)"),
    );
  }

  private render(note: string | null): void {
    this.list.replaceChildren();
    const q = this.input.value.trim();
    if (!this.hits.length) {
      const empty = document.createElement("div");
      empty.className = "recall-empty";
      empty.textContent = note ?? "Nothing found.";
      this.list.appendChild(empty);
      this.status.textContent = "";
      return;
    }
    this.hits.forEach((h, i) => {
      const row = document.createElement("div");
      row.className = "recall-row";
      row.dataset.kind = h.kind;

      const kind = document.createElement("span");
      kind.className = "recall-kind";
      kind.textContent = KIND_LABEL[h.kind] ?? h.kind;

      const body = document.createElement("div");
      body.className = "recall-body";
      if (h.title) {
        const title = document.createElement("div");
        title.className = "recall-title";
        title.textContent = h.title;
        body.appendChild(title);
      }
      const text = document.createElement("div");
      text.className = "recall-text";
      text.textContent = h.text;
      body.appendChild(text);

      const when = document.createElement("time");
      when.className = "recall-when";
      when.dateTime = new Date(h.t_unix * 1000).toISOString();
      when.title = new Date(h.t_unix * 1000).toLocaleString();
      when.textContent = ago(h.t_unix);

      row.append(kind, body, when);
      row.addEventListener("mouseenter", () => {
        this.selected = i;
        this.highlight();
      });
      row.addEventListener("click", () => this.copy(i));
      this.list.appendChild(row);
    });
    this.highlight();
    const n = this.hits.length;
    this.status.textContent = q ? `${n} match${n === 1 ? "" : "es"}` : "recently on screen";
  }

  private highlight(): void {
    const rows = this.list.querySelectorAll<HTMLElement>(".recall-row");
    rows.forEach((r, i) => r.classList.toggle("selected", i === this.selected));
    rows[this.selected]?.scrollIntoView({ block: "nearest" });
  }

  private build(): HTMLElement {
    const el = document.createElement("div");
    el.id = "recall";
    el.innerHTML = `
      <div class="recall-card" role="dialog" aria-label="Search memory">
        <div class="recall-bar">
          <span class="recall-glyph" aria-hidden="true">⌕</span>
          <input class="recall-input" type="text" autocomplete="off" spellcheck="false"
                 placeholder="Search what you've seen and said…" />
          <span class="recall-status" aria-live="polite"></span>
        </div>
        <div class="recall-list"></div>
        <div class="recall-foot">
          <span><kbd>↑</kbd><kbd>↓</kbd> move</span>
          <span><kbd>Enter</kbd> copy</span>
          <span><kbd>Esc</kbd> close</span>
        </div>
      </div>
    `;
    return el;
  }
}

/** "just now", "12 min ago", "3 h ago", "yesterday", "4 days ago", or a date. */
export function ago(tUnix: number, nowMs: number = Date.now()): string {
  const s = Math.max(0, Math.floor(nowMs / 1000 - tUnix));
  if (s < 60) return "just now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} h ago`;
  const d = Math.floor(h / 24);
  if (d === 1) return "yesterday";
  if (d < 7) return `${d} days ago`;
  return new Date(tUnix * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}
