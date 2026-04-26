// A generated grammar module supplies token/rule-kind enums, a DFA config,
// a set of skip kinds, a drive callback that runs the state machine, and
// `parseXxx` entry points. The types and classes here handle everything
// else: lexing, lookahead buffering, skip routing, event queueing, and
// error recovery.

/** Byte offset plus 1-based line/column for a source position. */
export interface Pos {
  /** 0-based byte offset into the source. */
  offset: number;
  /** 1-based line number (each `\n` increments this). */
  line: number;
  /** 1-based column, counted in Unicode codepoints within the current line. */
  column: number;
}

/** Half-open span `[start, end)` over the source. */
export interface Span {
  start: Pos;
  end: Pos;
}

/** Build a zero-width span at a single point. */
export function pointSpan(p: Pos): Span {
  return { start: p, end: p };
}

/**
 * A lexed token: kind, source span, and the matched text.
 *
 * `kind` is `null` only when the lexer could not match any pattern at the
 * current position; the scanner still advances by one codepoint so parsing
 * can recover. EOF is its own kind value.
 */
export interface Token<TK = number> {
  kind: TK | null;
  span: Span;
  text: string;
}

/** A recoverable parse or lex error. */
export interface ParseError {
  message: string;
  span: Span;
}

/** Format a [`ParseError`] as `error[line:col]: message`. */
export function errorToString(d: ParseError): string {
  const s = d.span;
  const loc =
    s.start.line === s.end.line && s.start.column === s.end.column
      ? `${s.start.line}:${s.start.column}`
      : `${s.start.line}:${s.start.column}-${s.end.line}:${s.end.column}`;
  return `error[${loc}]: ${d.message}`;
}

/**
 * A single event in the pull-based parse stream.
 *
 * Walking the events in order reconstructs the parse tree without ever
 * materialising one. The parser keeps emitting after an error so downstream
 * tools still see a usable stream.
 */
export type Event<TK = number, RK = number> =
  | { tag: "enter"; rule: RK; pos: Pos }
  | { tag: "exit"; rule: RK; pos: Pos }
  | { tag: "token"; token: Token<TK> }
  | { tag: "error"; error: ParseError };

/**
 * Result of running the grammar-specific compiled DFA over a byte slice.
 *
 * `bestLen`/`bestKind` are the longest match found; `bestKind` is `null`
 * when the scan never reached an accept state. `scanned` is how many
 * bytes the scan actually walked past `start` before it died — equals
 * `buf.length - start` when the scan stopped at end of input rather than
 * at a dead transition.
 */
export interface DfaMatch<TK> {
  bestLen: number;
  bestKind: TK | null;
  scanned: number;
}

/**
 * Grammar-specific compiled DFA. Generated code supplies one of these:
 * a per-state `switch` that avoids the data-dependent table load a
 * table-driven DFA pays on every byte.
 */
export type MatcherFunc<TK> = (buf: Uint8Array, start: number) => DfaMatch<TK>;

const _TEXT_DECODER = new TextDecoder();
function decodeBytes(buf: Uint8Array, start: number, end: number): string {
  return _TEXT_DECODER.decode(buf.subarray(start, end));
}

/**
 * Byte-level lexer over a UTF-8 encoded string, driven by a compiled DFA
 * supplied via [`MatcherFunc`].
 *
 * `eofKind` is the sentinel kind the runtime emits at end-of-input. Lex
 * failures (no DFA pattern matched) come through as `Token { kind: null }`.
 */
export class Lexer<TK extends number> {
  private bytes: Uint8Array;
  private i = 0;
  private line = 1;
  private col = 1;

  constructor(
    src: string,
    private readonly matcher: MatcherFunc<TK>,
    private readonly eofKind: TK,
  ) {
    this.bytes = new TextEncoder().encode(src);
  }

  private pos(): Pos {
    return { offset: this.i, line: this.line, column: this.col };
  }

  // Fast-path: plain ASCII non-newline bytes bump column directly; anything
  // else (UTF-8 continuations, `\n`) requires a byte-by-byte walk.
  private advance(n: number): void {
    const end = this.i + n;
    let needsWalk = false;
    for (let k = this.i; k < end; k++) {
      const b = this.bytes[k];
      if (b === 0x0a || b >= 0x80) {
        needsWalk = true;
        break;
      }
    }
    if (!needsWalk) {
      this.col += n;
      this.i = end;
      return;
    }
    while (this.i < end) {
      const b = this.bytes[this.i++];
      if (b === 0x0a) {
        this.line++;
        this.col = 1;
      } else if ((b & 0xc0) !== 0x80) {
        this.col++;
      }
    }
  }

  /** Produce the next token. Returns repeated EOF once input is exhausted. */
  nextToken(): Token<TK> {
    if (this.i >= this.bytes.length) {
      const p = this.pos();
      return { kind: this.eofKind, span: pointSpan(p), text: "" };
    }

    const { bestLen, bestKind } = this.matcher(this.bytes, this.i);

    const start = this.pos();
    if (bestLen === 0) {
      // No token pattern matched — emit a single-codepoint token with
      // kind = null so the parser can flag it and keep going.
      const b = this.bytes[this.i];
      const cpLen = b < 0x80 ? 1 : b < 0xe0 ? 2 : b < 0xf0 ? 3 : 4;
      const n = Math.min(cpLen, this.bytes.length - this.i);
      const text = decodeBytes(this.bytes, this.i, this.i + n);
      this.advance(n);
      return { kind: null, span: { start, end: this.pos() }, text };
    }
    const text = decodeBytes(this.bytes, this.i, this.i + bestLen);
    this.advance(bestLen);
    return { kind: bestKind, span: { start, end: this.pos() }, text };
  }
}

/** Sentinel state id for "the parser has terminated". */
export const TERMINATED = -1;

/**
 * Configuration a generated parser injects into the runtime [`Parser`].
 *
 * The runtime calls `drive(parser)` repeatedly until the event queue has
 * something to yield or the state machine terminates.
 */
export interface ParserConfig<TK extends number, RK extends number> {
  /** Lookahead required by the grammar (LL(k)). */
  k: number;
  /** EOF kind sentinel (matches the one passed to the `Lexer`). */
  eofKind: TK;
  /** Whether a given token kind is a `[skip]`-annotated skip. */
  isSkip: (kind: TK) => boolean;
  /** Grammar-specific state-machine step. Called when the queue is empty. */
  drive: (p: Parser<TK, RK>) => void;
}

/**
 * Pull-based parser. Iterate to walk the parse as a flat [`Event`] stream,
 * or call [`nextEvent`] directly for manual control.
 */
export class Parser<
  TK extends number,
  RK extends number,
> implements IterableIterator<Event<TK, RK>> {
  private lookBuf: Token<TK>[] = [];
  private prevEnd: Pos;
  private state: number;
  private ret: number[] = [];
  private queue: Event<TK, RK>[] = [];
  private pendingSkips: Token<TK>[] = [];
  private eofChecked = false;

  constructor(
    private readonly lex: Lexer<TK>,
    entry: number,
    private readonly cfg: ParserConfig<TK, RK>,
  ) {
    this.state = entry;
    for (let i = 0; i < cfg.k; i++) this.lookBuf.push(this.pumpToken());
    this.prevEnd = this.lookBuf[0].span.start;
  }

  /** Peek at the `i`-th lookahead token (`0 <= i < k`). */
  look(i: number): Token<TK> {
    return this.lookBuf[i];
  }

  /** Current state id. Read at the top of every driver iteration. */
  getState(): number {
    return this.state;
  }

  /** Overwrite the current state. */
  setState(s: number): void {
    this.state = s;
  }

  /** Push a return address onto the call stack. */
  pushRet(s: number): void {
    this.ret.push(s);
  }

  /** Pop the top return address, or [`TERMINATED`] if the stack is empty. */
  popRet(): number {
    return this.ret.length ? this.ret.pop()! : TERMINATED;
  }

  /** True if there is nothing queued to emit. */
  queueIsEmpty(): boolean {
    return this.queue.length === 0;
  }

  /**
   * True iff the current lookahead matches any of the given prefixes.
   * Used by generated `?`/`*`/`+` sites and by dispatch trees with k > 1.
   */
  matchesFirst(set: readonly (readonly TK[])[]): boolean {
    outer: for (const seq of set) {
      for (let i = 0; i < seq.length; i++) {
        if (this.lookBuf[i].kind !== seq[i]) continue outer;
      }
      return true;
    }
    return false;
  }

  /** Emit an `Enter` event for `rule`, anchored at the start of lookahead. */
  enter(rule: RK): void {
    const pos = this.lookBuf[0].span.start;
    this.prevEnd = pos;
    this.emit({ tag: "enter", rule, pos });
  }

  /** Emit an `Exit` event for `rule`, anchored at the previous token's end. */
  exit(rule: RK): void {
    this.emit({ tag: "exit", rule, pos: this.prevEnd });
  }

  /** Consume the current lookahead and emit it as a token event. */
  consume(): void {
    const prev = this.lookBuf[0];
    this.prevEnd = prev.span.end;
    this.emit({ tag: "token", token: prev });
    this.advanceLook();
  }

  /**
   * Consume a token of `kind`; on mismatch, raise an error, recover to
   * `sync`, and try once more. `expectedMsg` is the diagnostic shown to
   * the user — the generator passes the grammar-declared token name.
   */
  tryConsume(kind: TK, sync: readonly TK[], expectedMsg: string): void {
    if (this.lookBuf[0].kind === kind) {
      this.consume();
      return;
    }
    this.errorHere(expectedMsg);
    this.recoverTo(sync);
    if (this.lookBuf[0].kind === kind) this.consume();
  }

  /** Consume tokens until lookahead matches `sync` (or EOF). */
  recoverTo(sync: readonly TK[]): void {
    for (;;) {
      const k = this.lookBuf[0].kind;
      if (k === this.cfg.eofKind) return;
      if (k !== null && sync.indexOf(k) >= 0) return;
      this.emit({ tag: "token", token: this.lookBuf[0] });
      this.advanceLook();
    }
  }

  /** Raise a recoverable error at the current lookahead. */
  errorHere(msg: string): void {
    this.emit({
      tag: "error",
      error: { message: msg, span: this.lookBuf[0].span },
    });
  }

  /** Produce the next event, or `undefined` once the input is fully consumed. */
  nextEvent(): Event<TK, RK> | undefined {
    for (;;) {
      if (this.queue.length) return this.queue.shift();
      if (this.state === TERMINATED) {
        if (!this.eofChecked) {
          this.eofChecked = true;
          if (this.lookBuf[0].kind !== this.cfg.eofKind) {
            this.errorHere("expected end of input");
            while (this.lookBuf[0].kind !== this.cfg.eofKind) {
              this.emit({ tag: "token", token: this.lookBuf[0] });
              this.advanceLook();
            }
          }
          this.flushSkipsBefore(this.lookBuf[0].span.end);
          continue;
        }
        return undefined;
      }
      this.cfg.drive(this);
    }
  }

  next(): IteratorResult<Event<TK, RK>> {
    const v = this.nextEvent();
    return v === undefined
      ? { done: true, value: undefined as unknown as Event<TK, RK> }
      : { done: false, value: v };
  }
  [Symbol.iterator](): IterableIterator<Event<TK, RK>> {
    return this;
  }

  private pumpToken(): Token<TK> {
    for (;;) {
      const t = this.lex.nextToken();
      if (t.kind !== null && this.cfg.isSkip(t.kind)) {
        this.pendingSkips.push(t);
        continue;
      }
      return t;
    }
  }

  private advanceLook(): void {
    this.lookBuf.shift();
    this.lookBuf.push(this.pumpToken());
  }

  // Drain pending skip tokens whose end offset is <= `pos.offset`. Called
  // just before any event so whitespace/comments appear in source order.
  private flushSkipsBefore(pos: Pos): void {
    while (
      this.pendingSkips.length &&
      this.pendingSkips[0].span.end.offset <= pos.offset
    ) {
      const t = this.pendingSkips.shift()!;
      this.queue.push({ tag: "token", token: t });
    }
  }

  private emit(ev: Event<TK, RK>): void {
    const start =
      ev.tag === "enter" || ev.tag === "exit"
        ? ev.pos
        : ev.tag === "token"
          ? ev.token.span.start
          : ev.error.span.start;
    this.flushSkipsBefore(start);
    this.queue.push(ev);
  }
}
