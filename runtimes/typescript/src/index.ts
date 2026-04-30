// A generated grammar module supplies token/rule-kind enums, a DFA
// matcher, a set of skip kinds, a `step` callback that runs one state
// body of the generated dispatch, and `parseXxx` entry points. The
// types and classes here handle everything else: lexing, lookahead
// buffering, skip routing, and error recovery.

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
 *
 * `garbage` events appear between an `error` and the recovery's sync
 * point — each unexpected token the runtime had to skip past comes
 * through tagged `garbage` so consumers can drop it from the AST or
 * render it as an error span without tracking recovery state.
 */
export type Event<TK = number, RK = number> =
  | { tag: "enter"; rule: RK; pos: Pos }
  | { tag: "exit"; rule: RK; pos: Pos }
  | { tag: "token"; token: Token<TK> }
  | { tag: "garbage"; token: Token<TK> }
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
 * The handle generated `step` bodies talk to the runtime through. A
 * structural interface that re-exports just the operations dispatch
 * needs — lookahead access, return stack pushes, event builders,
 * recovery arming.
 *
 * The runtime's [`Parser`] satisfies this interface, but the public
 * `Parser` type only exposes `nextEvent` and the iterator surface, so
 * external consumers can't call these methods. Generated code asks for
 * `Cursor<TK, RK>` and pays no allocation or indirection — the cursor
 * is the parser instance, just narrowed to the dispatch surface at the
 * type level.
 */
export interface Cursor<TK extends number, RK extends number> {
  /** Peek at the i-th lookahead token. */
  look(i: number): Token<TK>;
  /** Current state id. */
  getState(): number;
  /** Overwrite the current state. */
  setState(s: number): void;
  /** Push a return address onto the call stack. */
  pushRet(s: number): void;
  /** Pop the top return address, or [`TERMINATED`] if empty. */
  popRet(): number;
  /** True iff the lookahead matches any of the given prefixes. */
  matchesFirst(set: readonly (readonly TK[])[]): boolean;
  /** Build an Enter event for `rule`. */
  enter(rule: RK): Event<TK, RK>;
  /** Build an Exit event for `rule`. */
  exit(rule: RK): Event<TK, RK>;
  /** Consume the lookahead as a `token` event. */
  consume(): Event<TK, RK>;
  /** Try to consume `kind`; on miss arm recovery and return an error. */
  tryConsume(kind: TK, sync: readonly TK[], expectedMsg: string): Event<TK, RK>;
  /** Arm recovery without an expected kind. */
  recoverTo(sync: readonly TK[]): void;
  /** Build a recoverable error event at the lookahead. */
  errorHere(msg: string): Event<TK, RK>;
}

/**
 * Configuration a generated parser injects into the runtime [`Parser`].
 *
 * The runtime calls `step(cursor)` once per drive-mode iteration.
 * `step` runs exactly one match arm of the generated dispatch and
 * returns whatever event that body's path produced (or `undefined`
 * if the body was a pure transition step).
 */
export interface ParserConfig<TK extends number, RK extends number> {
  /** Lookahead required by the grammar (LL(k)). */
  k: number;
  /** EOF kind sentinel (matches the one passed to the `Lexer`). */
  eofKind: TK;
  /** Whether a given token kind is a `[skip]`-annotated skip. */
  isSkip: (kind: TK) => boolean;
  /**
   * Run one state body of the generated dispatch. Returns the event
   * that body produced, or `undefined` if it was a pure transition
   * (the runtime's `nextEvent` loop calls `step` again).
   */
  step: (c: Cursor<TK, RK>) => Event<TK, RK> | undefined;
}

/**
 * In-flight error recovery. Set by [`Cursor.tryConsume`]'s slow path
 * and [`Cursor.recoverTo`]; cleared once the lookahead lands on a sync
 * token (or EOF). Each call to [`Parser.nextEvent`] in recovery mode
 * yields one `garbage` event before re-checking the sync set.
 */
interface Recovery<TK> {
  /** Token kinds to recover *to*. */
  sync: readonly TK[];
  /**
   * Set when recovery was triggered by `tryConsume` for `kind`: if
   * the sync set lands on `kind`, the recovery finalisation also
   * consumes the token (so the surrounding rule proceeds as if it
   * had matched). `null` for the dispatch error path, where there's
   * no expected kind to swallow on exit.
   */
  expected: TK | null;
}

/**
 * Pull-based parser. Iterate to walk the parse as a flat [`Event`]
 * stream, or call [`nextEvent`] directly for manual control.
 *
 * Each `nextEvent` call fires one of three modes — pump (refill
 * lookahead, possibly yielding a skip), recovery (one garbage token
 * per call), or drive (one `step` call) — and yields one event per
 * mode. Drive's `step` may return `undefined` for pure-transition
 * state bodies; the runtime simply loops drive again.
 *
 * The class implements [`Cursor`] internally — generated dispatch
 * receives a `Cursor` view of the parser, so external consumers (who
 * see only the public `Parser` API: constructor, `nextEvent`, and the
 * iterator surface) never get to the dispatch hooks. The methods are
 * marked `private` for type-level enforcement; at runtime the same
 * instance plays both roles, so there's no wrapper allocation and no
 * indirection.
 */
export class Parser<
  TK extends number,
  RK extends number,
> implements IterableIterator<Event<TK, RK>>, Cursor<TK, RK> {
  /**
   * Lookahead ring. `null` slots are awaiting refill — pump-mode in
   * [`nextEvent`] pulls lex tokens one-at-a-time until every slot
   * holds a structural token. Generated `step()` only ever runs
   * when all slots are filled, so [`look`] can read unconditionally.
   *
   * Empty slots are a contiguous suffix: `consume` shifts down by
   * one and parks the new `null` at index `k-1`, and `pumpOne`
   * always fills the leftmost empty slot.
   */
  private lookBuf: (Token<TK> | null)[];
  private prevEnd: Pos;
  private state: number;
  private retStack: number[] = [];
  private recovery: Recovery<TK> | null = null;

  constructor(
    private readonly lex: Lexer<TK>,
    entry: number,
    private readonly cfg: ParserConfig<TK, RK>,
  ) {
    this.state = entry;
    this.lookBuf = new Array<Token<TK> | null>(cfg.k).fill(null);
    // `prevEnd` is overwritten on the first `enter()`/`consume()`. Until
    // then it just needs to be a valid Pos; pin it at the source origin.
    this.prevEnd = { offset: 0, line: 1, column: 1 };
  }

  // -------------------------------------------------------------------
  // Cursor implementation. These are public at the runtime/JS level
  // (TypeScript's `private` keyword is type-only) so generated code in
  // user packages can call them via the `Cursor<TK, RK>` view.
  // -------------------------------------------------------------------

  /** @internal */
  look(i: number): Token<TK> {
    const t = this.lookBuf[i];
    if (t === null) {
      throw new Error("look slot empty inside step() — pump did not refill before dispatch");
    }
    return t;
  }

  /** @internal */
  getState(): number {
    return this.state;
  }

  /** @internal */
  setState(s: number): void {
    this.state = s;
  }

  /** @internal */
  pushRet(s: number): void {
    this.retStack.push(s);
  }

  /** @internal */
  popRet(): number {
    return this.retStack.length ? this.retStack.pop()! : TERMINATED;
  }

  /** @internal */
  matchesFirst(set: readonly (readonly TK[])[]): boolean {
    outer: for (const seq of set) {
      for (let i = 0; i < seq.length; i++) {
        if (this.look(i).kind !== seq[i]) continue outer;
      }
      return true;
    }
    return false;
  }

  /** @internal */
  enter(rule: RK): Event<TK, RK> {
    const pos = this.look(0).span.start;
    this.prevEnd = pos;
    return { tag: "enter", rule, pos };
  }

  /** @internal */
  exit(rule: RK): Event<TK, RK> {
    return { tag: "exit", rule, pos: this.prevEnd };
  }

  /**
   * Pop the current lookahead token, shifting the buffer up by one.
   * Slot `k-1` is left null so pump-mode can refill it before the
   * next `step()` reads lookahead.
   */
  private takeToken(): Token<TK> {
    const t = this.lookBuf[0];
    if (t === null) {
      throw new Error("takeToken called with empty lookahead");
    }
    this.prevEnd = t.span.end;
    const k = this.cfg.k;
    // Shift left and park `null` at the end; same shape as Rust's
    // `look.rotate_left(1)` but spelled out for V8.
    for (let i = 0; i < k - 1; i++) {
      this.lookBuf[i] = this.lookBuf[i + 1];
    }
    this.lookBuf[k - 1] = null;
    return t;
  }

  /** @internal */
  consume(): Event<TK, RK> {
    return { tag: "token", token: this.takeToken() };
  }

  /** @internal */
  tryConsume(kind: TK, sync: readonly TK[], expectedMsg: string): Event<TK, RK> {
    if (this.lookBuf[0] !== null && this.lookBuf[0].kind === kind) {
      return this.consume();
    }
    const event = this.errorHere(expectedMsg);
    this.recovery = { sync, expected: kind };
    return event;
  }

  /** @internal */
  recoverTo(sync: readonly TK[]): void {
    this.recovery = { sync, expected: null };
  }

  /** @internal */
  errorHere(msg: string): Event<TK, RK> {
    return { tag: "error", error: { message: msg, span: this.look(0).span } };
  }

  // -------------------------------------------------------------------
  // Public iteration API.
  // -------------------------------------------------------------------

  /**
   * Produce the next event from the parse, or signal completion via
   * `IteratorResult`. The whole pull loop lives here — there's no
   * separate `nextEvent` indirection.
   *
   * Each call fires one of three modes — pump (refill lookahead,
   * possibly yielding a skip), recovery (one garbage token per call),
   * or drive (one `step` call) — and yields one event before looping
   * again. Drive's `step` may return `undefined` for pure-transition
   * state bodies; the runtime simply loops drive again.
   */
  next(): IteratorResult<Event<TK, RK>> {
    for (;;) {
      // Skip mode: refill any empty lookahead slot. A skip token
      // becomes the yielded event; a structural token fills the slot
      // and the loop retries. Slots fill leftmost-first and `consume`
      // parks new `null`s at the end, so checking slot `k-1` is the
      // O(1) form of "any slot is null".
      if (this.lookBuf[this.cfg.k - 1] === null) {
        const t = this.lex.nextToken();
        if (t.kind !== null && this.cfg.isSkip(t.kind)) {
          return { done: false, value: { tag: "token", token: t } };
        }
        for (let i = 0; i < this.lookBuf.length; i++) {
          if (this.lookBuf[i] === null) {
            this.lookBuf[i] = t;
            break;
          }
        }
        continue;
      }

      // Recovery mode: advance one step. Three outcomes — yield a
      // `garbage` event for an unexpected token, yield a normal
      // `token` event when the sync hit on the kind we were
      // expecting, or finalise without consuming and loop.
      if (this.recovery !== null) {
        const rec = this.recovery;
        const look0Kind = this.lookBuf[0]!.kind;
        if (look0Kind === this.cfg.eofKind) {
          this.recovery = null;
          continue;
        }
        if (look0Kind !== null && rec.sync.indexOf(look0Kind) >= 0) {
          const wasExpected = rec.expected !== null && rec.expected === look0Kind;
          this.recovery = null;
          if (wasExpected) {
            return { done: false, value: this.consume() };
          }
          continue;
        }
        return { done: false, value: { tag: "garbage", token: this.takeToken() } };
      }

      // EOF gate. On the first visit with trailing input, raise an
      // error and arm a sync-empty recovery so the rest of the input
      // drains as garbage events one per call. Once recovery has
      // eaten its way to EOF the lookahead pins at EOF (the lexer
      // keeps yielding it), so this is naturally idempotent —
      // subsequent visits just signal done.
      if (this.state === TERMINATED) {
        if (this.lookBuf[0]?.kind === this.cfg.eofKind) {
          return { done: true, value: undefined as unknown as Event<TK, RK> };
        }
        const event = this.errorHere("expected end of input");
        this.recovery = { sync: [], expected: null };
        return { done: false, value: event };
      }

      // Drive mode: run one state body. If `step` emitted, yield it.
      // Otherwise it just transitioned `cur` (and possibly the return
      // stack); loop and run the next body. The cursor is just `this`
      // narrowed to the dispatch interface — no allocation, no hop.
      const ev = this.cfg.step(this);
      if (ev !== undefined) return { done: false, value: ev };
    }
  }

  [Symbol.iterator](): IterableIterator<Event<TK, RK>> {
    return this;
  }
}
