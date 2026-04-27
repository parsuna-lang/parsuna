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
 *
 * `buf` is a Latin-1 string where each char's code unit is the
 * underlying UTF-8 byte (0–255). The runtime constructs it once at
 * Lexer construction so the hot-path matcher can read bytes via
 * `buf.charCodeAt(pos)` — V8's one-byte-string representation makes
 * that a single load with no bounds-check or typed-array dispatch.
 */
export type MatcherFunc<TK> = (buf: string, start: number) => DfaMatch<TK>;

const _TEXT_DECODER = new TextDecoder();
const _LATIN1_DECODER = new TextDecoder("latin1");
function decodeBytes(buf: Uint8Array, start: number, end: number): string {
  return _TEXT_DECODER.decode(buf.subarray(start, end));
}

/**
 * Byte-level lexer over a UTF-8 encoded string, driven by a compiled DFA
 * supplied via [`MatcherFunc`].
 *
 * `eofKind` is the sentinel kind the runtime emits at end-of-input. Lex
 * failures (no DFA pattern matched) come through as `Token { kind: null }`.
 *
 * Internally we keep two views of the same bytes:
 * - `bytes` (Uint8Array) — used to count lines/columns and to UTF-8-decode
 *   each token's text payload. The byte view is what the public Span
 *   offsets refer to.
 * - `buf` (Latin-1 string) — fed to the generated matcher so byte reads
 *   compile to `String.charCodeAt`, which V8 specialises hard for one-byte
 *   strings.
 */
export class Lexer<TK extends number> {
  private bytes: Uint8Array;
  private buf: string;
  private i = 0;
  private line = 1;
  private col = 1;

  constructor(
    src: string,
    private readonly matcher: MatcherFunc<TK>,
    private readonly eofKind: TK,
  ) {
    this.bytes = new TextEncoder().encode(src);
    this.buf = _LATIN1_DECODER.decode(this.bytes);
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

    const { bestLen, bestKind } = this.matcher(this.buf, this.i);

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
  /**
   * Hard cap on events the parser's fixed-size queue can hold. Equal to
   * the longest emit burst across every state body in the grammar — the
   * runtime sizes its ring at construction so a single drive call's
   * burst always fits without growth, and the per-yield bound on
   * pump/recovery keeps the queue from ever exceeding this.
   */
  queueCap: number;
  /** EOF kind sentinel (matches the one passed to the `Lexer`). */
  eofKind: TK;
  /** Whether a given token kind is a `[skip]`-annotated skip. */
  isSkip: (kind: TK) => boolean;
  /** Grammar-specific state-machine step. Called when the queue is empty. */
  drive: (p: Parser<TK, RK>) => void;
}

/**
 * Fixed-capacity FIFO ring used as the parser's event queue.
 *
 * Sized at construction by the value codegen passes via
 * [`ParserConfig.queueCap`] — computed by lowering from the longest
 * state body's emit burst, so the queue is exactly large enough for the
 * worst case the grammar can produce. No growth, no resize.
 *
 * Invariant: `len <= cap`. The runtime layer above only pushes inside
 * `drive()`/pump/recovery and each path is bounded by lowering, so an
 * overflow is a codegen bug.
 */
class EventRing<T> {
  private buf: (T | undefined)[];
  private head = 0;
  private len = 0;

  constructor(private readonly cap: number) {
    // `cap` is always >= 1 in practice (lowering rounds up so even a
    // grammar with no emits has a viable ring), so we can fix the
    // backing array length up front.
    this.buf = new Array<T | undefined>(cap);
  }

  isEmpty(): boolean {
    return this.len === 0;
  }

  pushBack(t: T): void {
    const idx = (this.head + this.len) % this.cap;
    this.buf[idx] = t;
    this.len++;
  }

  popFront(): T | undefined {
    if (this.len === 0) return undefined;
    const v = this.buf[this.head] as T;
    // Drop our reference so the GC can reclaim the event.
    this.buf[this.head] = undefined;
    this.head = (this.head + 1) % this.cap;
    this.len--;
    return v;
  }
}

/**
 * In-flight error recovery. Set by [`Parser.tryConsume`]'s slow path and
 * [`Parser.recoverTo`]; cleared once the lookahead lands on a sync
 * token (or EOF). Each call to [`Parser.nextEvent`] drains exactly one
 * garbage token before yielding, so a long run of unexpected input
 * doesn't pile up in the queue — the consumer sees recovery tokens
 * interleaved with their lex order.
 */
interface Recovery<TK> {
  /** Token kinds to recover *to*. */
  sync: readonly TK[];
  /**
   * Set when the recovery was triggered by an `expect` for `kind`: if
   * the sync set lands on `kind`, the recovery finalisation also
   * consumes the token (so the surrounding rule proceeds as if it had
   * matched). `null` for the dispatch error path, where there's no
   * expected kind to swallow on exit.
   */
  expected: TK | null;
}

/**
 * Pull-based parser. Iterate to walk the parse as a flat [`Event`] stream,
 * or call [`nextEvent`] directly for manual control.
 *
 * Each call to [`nextEvent`] makes one of four kinds of progress: drain
 * a queued event, pump one lex token (filling lookahead, or queueing a
 * skip), advance recovery by one step, or run one drive call. Each of
 * pump/recovery contributes at most one event before yielding, so the
 * event queue never grows past [`ParserConfig.queueCap`] (the longest
 * structural burst from a single drive body).
 *
 * **Skip handling** is driven by pump-mode rather than a side queue:
 * when the lexer hands the runtime a skip token it lands directly in
 * the event queue, ahead of whatever structural event the next
 * `drive()` will produce. The lookahead refills one lex token at a
 * time (yielding between each), so a long comment run can't grow the
 * queue past `queueCap`.
 */
export class Parser<
  TK extends number,
  RK extends number,
> implements IterableIterator<Event<TK, RK>> {
  /**
   * Lookahead ring. `null` slots are awaiting refill — pump-mode in
   * [`nextEvent`] pulls lex tokens one-at-a-time until every slot
   * holds a structural token. Generated `drive()` only ever runs when
   * all slots are filled, so [`look`] can read unconditionally.
   */
  private lookBuf: (Token<TK> | null)[];
  private prevEnd: Pos;
  private state: number;
  private retStack: number[] = [];
  private queue: EventRing<Event<TK, RK>>;
  private recovery: Recovery<TK> | null = null;
  private eofChecked = false;

  constructor(
    private readonly lex: Lexer<TK>,
    entry: number,
    private readonly cfg: ParserConfig<TK, RK>,
  ) {
    this.state = entry;
    this.lookBuf = new Array<Token<TK> | null>(cfg.k).fill(null);
    this.queue = new EventRing<Event<TK, RK>>(cfg.queueCap);
    // `prevEnd` is overwritten on the first `enter()`/`consume()`. Until
    // then it just needs to be a valid Pos; pin it at the source origin.
    this.prevEnd = { offset: 0, line: 1, column: 1 };
  }

  /**
   * Peek at the `i`-th lookahead token (`0 <= i < k`). Generated
   * `drive()` only runs after pump-mode has filled every slot, so this
   * never sees a `null`.
   */
  look(i: number): Token<TK> {
    const t = this.lookBuf[i];
    if (t === null) {
      throw new Error("look slot empty inside drive() — pump did not refill before dispatch");
    }
    return t;
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
    this.retStack.push(s);
  }

  /** Pop the top return address, or [`TERMINATED`] if the stack is empty. */
  popRet(): number {
    return this.retStack.length ? this.retStack.pop()! : TERMINATED;
  }

  /** True if there is nothing queued to emit. */
  queueIsEmpty(): boolean {
    return this.queue.isEmpty();
  }

  /**
   * True iff the current lookahead matches any of the given prefixes.
   * Used by generated `?`/`*`/`+` sites and by dispatch trees with k > 1.
   */
  matchesFirst(set: readonly (readonly TK[])[]): boolean {
    outer: for (const seq of set) {
      for (let i = 0; i < seq.length; i++) {
        if (this.look(i).kind !== seq[i]) continue outer;
      }
      return true;
    }
    return false;
  }

  /** Emit an `Enter` event for `rule`, anchored at the start of lookahead. */
  enter(rule: RK): void {
    const pos = this.look(0).span.start;
    this.prevEnd = pos;
    this.queue.pushBack({ tag: "enter", rule, pos });
  }

  /** Emit an `Exit` event for `rule`, anchored at the previous token's end. */
  exit(rule: RK): void {
    this.queue.pushBack({ tag: "exit", rule, pos: this.prevEnd });
  }

  /**
   * Consume the current lookahead token, emit it, and shift the buffer
   * up by one. Slot `k-1` is left null so pump-mode can refill it
   * (yielding any skip it pulls along the way) before the next
   * `drive()` reads lookahead.
   */
  consume(): void {
    const t = this.lookBuf[0];
    if (t === null) {
      throw new Error("consume called with empty lookahead");
    }
    this.prevEnd = t.span.end;
    const k = this.cfg.k;
    for (let i = 0; i < k - 1; i++) {
      this.lookBuf[i] = this.lookBuf[i + 1];
    }
    this.lookBuf[k - 1] = null;
    this.queue.pushBack({ tag: "token", token: t });
  }

  /**
   * Try to consume a token of `kind`; on mismatch, emit an error and
   * hand recovery off to the runtime's recovery-mode.
   *
   * `sync` is typically the caller rule's FOLLOW set: recovery skips
   * unexpected tokens until the lookahead lands on one of these,
   * which gives the surrounding rule a reasonable place to resume.
   * On the slow path we return immediately after staging the error and
   * recovery — drive's loop sees the queued error and yields, then
   * [`nextEvent`]'s recovery-mode advances one garbage token per call
   * until the sync set is hit.
   */
  tryConsume(kind: TK, sync: readonly TK[], expectedMsg: string): void {
    if (this.lookBuf[0] !== null && this.lookBuf[0].kind === kind) {
      this.consume();
      return;
    }
    this.errorHere(expectedMsg);
    this.recovery = { sync, expected: kind };
  }

  /**
   * Arm recovery-mode without an expected kind. Called from a dispatch
   * tree's error leaf: the surrounding `cur` was already set to the
   * post-recovery state by codegen, and the queued `Error` event makes
   * drive() yield immediately so recovery-mode can take over.
   */
  recoverTo(sync: readonly TK[]): void {
    this.recovery = { sync, expected: null };
  }

  /** Raise a recoverable error at the current lookahead. */
  errorHere(msg: string): void {
    this.queue.pushBack({
      tag: "error",
      error: { message: msg, span: this.look(0).span },
    });
  }

  /** Produce the next event, or `undefined` once the input is fully consumed. */
  nextEvent(): Event<TK, RK> | undefined {
    for (;;) {
      const queued = this.queue.popFront();
      if (queued !== undefined) return queued;
      if (this.pumpPending()) {
        this.pumpOne();
        continue;
      }
      if (this.recovery !== null) {
        this.recoverOne();
        continue;
      }
      if (this.state === TERMINATED) {
        if (!this.eofChecked) {
          this.eofChecked = true;
          if (this.lookBuf[0]?.kind !== this.cfg.eofKind) {
            // Trailing input past the entry rule. Synthesize an error
            // and use recovery-mode (with an empty sync set) to drain
            // the rest as Token events one yield at a time.
            this.errorHere("expected end of input");
            this.recovery = { sync: [], expected: null };
            continue;
          }
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

  /**
   * True iff some lookahead slot still needs to be filled. Pump-mode
   * runs whenever this holds, so generated `drive()` code can read any
   * `look(i)` unconditionally.
   */
  private pumpPending(): boolean {
    for (let i = 0; i < this.lookBuf.length; i++) {
      if (this.lookBuf[i] === null) return true;
    }
    return false;
  }

  /**
   * Lex one token. If it's a skip, push it directly onto the event
   * queue and leave pump-mode armed for another call. If it's a
   * structural token, fill the leftmost empty lookahead slot.
   *
   * Yielding per skip is what makes the queue cap honest — a long
   * comment run can't grow the queue past 1 (nextEvent drains the
   * just-pushed skip on the next loop iteration before pumping again).
   */
  private pumpOne(): void {
    const t = this.lex.nextToken();
    if (t.kind !== null && this.cfg.isSkip(t.kind)) {
      this.queue.pushBack({ tag: "token", token: t });
      return;
    }
    for (let i = 0; i < this.lookBuf.length; i++) {
      if (this.lookBuf[i] === null) {
        this.lookBuf[i] = t;
        return;
      }
    }
    throw new Error("pumpOne called with all slots filled");
  }

  /**
   * Advance recovery by one step. Either consume one garbage token
   * (one Token push, drive yield) or — if the lookahead is in the
   * sync set / EOF — finalise by clearing recovery and (when a
   * matching `expected` was set) swallowing the synced-to token.
   */
  private recoverOne(): void {
    const rec = this.recovery!;
    const look0 = this.lookBuf[0];
    const k = look0 === null ? null : look0.kind;
    if (k === this.cfg.eofKind) {
      this.recovery = null;
      return;
    }
    if (k !== null && rec.sync.indexOf(k) >= 0) {
      const wasExpected = rec.expected !== null && rec.expected === k;
      this.recovery = null;
      if (wasExpected) this.consume();
      return;
    }
    this.consume();
  }
}
