// Lightweight syntax highlighting for the Text Compare view: a table-driven
// tokenizer per language (keywords, comments, strings, numbers, markup tags),
// keyed by file extension. Only state that crosses lines (block comments,
// multi-line strings) is tracked for the whole file; lines themselves are
// tokenized lazily, as they scroll into view, and memoized.

export type TokenKind = "kw" | "type" | "lit" | "str" | "num" | "com" | "meta" | "tag" | "attr";

/** A highlighted range in one line (half-open, UTF-16 indices). */
export interface Token {
  start: number;
  end: number;
  kind: TokenKind;
}

export interface Language {
  name: string;
  keywords: Set<string>;
  types: Set<string>;
  literals: Set<string>;
  /** Keywords match regardless of case (SQL, PowerShell, batch). */
  caseInsensitive: boolean;
  lineComments: string[];
  blockComments: [string, string][];
  /** Quote characters of single-line strings. */
  strings: string[];
  /** Delimiters of strings that may span lines (open = close). */
  multilineStrings: string[];
  /** A whole line that matches is a preprocessor/directive line. */
  metaLine?: RegExp;
  /** Identifiers starting with this character are meta (decorators, attributes, at-rules). */
  metaPrefix?: string;
  /** Highlight `<tag attr="…">` markup. */
  markup: boolean;
}

const words = (s: string) => new Set(s.split(/\s+/).filter(Boolean));

function lang(name: string, def: Partial<Omit<Language, "name">>): Language {
  return {
    name,
    keywords: new Set(),
    types: new Set(),
    literals: new Set(),
    caseInsensitive: false,
    lineComments: [],
    blockComments: [],
    strings: ['"', "'"],
    multilineStrings: [],
    markup: false,
    ...def,
  };
}

const C_COMMENTS = { lineComments: ["//"], blockComments: [["/*", "*/"]] as [string, string][] };

const JS = lang("javascript", {
  ...C_COMMENTS,
  keywords: words(`break case catch class const continue debugger default delete do else export extends finally for
    function if import in instanceof let new return super switch this throw try typeof var void while with yield async
    await of static get set from as type interface enum implements private protected public readonly declare namespace
    abstract keyof infer is satisfies override`),
  types: words("string number boolean any unknown never object symbol bigint Array Promise Record Map Set"),
  literals: words("true false null undefined NaN Infinity"),
  multilineStrings: ["`"],
  metaPrefix: "@",
});

const RUST = lang("rust", {
  ...C_COMMENTS,
  keywords: words(`as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod
    move mut pub ref return self Self static struct super trait type unsafe use where while macro_rules`),
  types: words("i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize f32 f64 bool char str String Vec Option Result Box"),
  literals: words("true false None Some Ok Err"),
  // Rust string literals may span lines; `'` is mostly lifetimes, so it is not a string here.
  strings: [],
  multilineStrings: ['"'],
});

const C_KEYWORDS = `auto break case char const continue default do double else enum extern float for goto if inline int
  long register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while`;

const CPP = lang("cpp", {
  ...C_COMMENTS,
  keywords: words(`${C_KEYWORDS} alignas alignof and asm bool catch class concept consteval constexpr constinit
    const_cast co_await co_return co_yield decltype delete dynamic_cast explicit export friend mutable namespace new
    noexcept not operator or private protected public reinterpret_cast requires static_assert static_cast template
    this thread_local throw try typeid typename using virtual`),
  types: words("size_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t wchar_t std string vector"),
  literals: words("true false nullptr NULL"),
  metaLine: /^\s*#/,
});

const CSHARP = lang("csharp", {
  ...C_COMMENTS,
  keywords: words(`abstract as async await base break case catch checked class const continue default delegate do else
    enum event explicit extern finally fixed for foreach get goto if implicit in init interface internal is lock
    namespace new operator out override params partial private protected public readonly record ref required return
    sealed set sizeof stackalloc static struct switch this throw try typeof unchecked unsafe using value var virtual
    void volatile when where while yield`),
  types: words("bool byte char decimal double dynamic float int long object sbyte short string uint ulong ushort"),
  literals: words("true false null"),
  metaLine: /^\s*#/,
});

const JAVA = lang("java", {
  ...C_COMMENTS,
  keywords: words(`abstract assert break case catch class const continue default do else enum extends final finally
    for goto if implements import instanceof interface native new package private protected public record return
    sealed static strictfp super switch synchronized this throw throws transient try var void volatile while yield
    fun val when object companion data open override internal lateinit by init is in out suspend inline`),
  types: words("boolean byte char double float int long short String Object Integer Long Boolean List Map Unit Any Int"),
  literals: words("true false null"),
  multilineStrings: ['"""'],
  metaPrefix: "@",
});

const GO = lang("go", {
  ...C_COMMENTS,
  keywords: words(`break case chan const continue default defer else fallthrough for func go goto if import interface
    map package range return select struct switch type var`),
  types: words(`bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8
    uint16 uint32 uint64 uintptr any`),
  literals: words("true false nil iota"),
  multilineStrings: ["`"],
});

const SWIFT = lang("swift", {
  ...C_COMMENTS,
  keywords: words(`associatedtype class deinit enum extension fileprivate func import init inout internal let open
    operator private protocol public rethrows static struct subscript typealias var break case continue default defer
    do else fallthrough for guard if in repeat return switch where while as catch is throw throws try async await some
    any self Self super mutating override final lazy weak`),
  types: words("Int Double Float Bool String Character Array Dictionary Set Optional Void"),
  literals: words("true false nil"),
  multilineStrings: ['"""'],
  metaPrefix: "@",
});

const PYTHON = lang("python", {
  lineComments: ["#"],
  keywords: words(`and as assert async await break class continue def del elif else except finally for from global if
    import in is lambda nonlocal not or pass raise return try while with yield match case self`),
  types: words("int float str bool list dict set tuple bytes object type"),
  literals: words("True False None"),
  multilineStrings: ['"""', "'''"],
  metaPrefix: "@",
});

const RUBY = lang("ruby", {
  lineComments: ["#"],
  keywords: words(`alias and begin break case class def defined? do else elsif end ensure for if in module next not or
    redo rescue retry return self super then undef unless until when while yield require attr_accessor`),
  literals: words("true false nil"),
});

const SHELL = lang("shell", {
  lineComments: ["#"],
  keywords: words(`if then else elif fi for while until do done case esac function in return local export readonly
    declare set unset shift source alias echo exit break continue`),
  literals: words("true false"),
});

const POWERSHELL = lang("powershell", {
  lineComments: ["#"],
  blockComments: [["<#", "#>"]],
  caseInsensitive: true,
  keywords: words(`begin break catch class continue data do dynamicparam else elseif end exit filter finally for foreach
    from function if in param process return switch throw trap try until using var while workflow`),
  literals: words("$true $false $null"),
});

const BATCH = lang("batch", {
  lineComments: ["::", "rem ", "REM ", "@rem ", "@REM "],
  caseInsensitive: true,
  strings: ['"'],
  keywords: words("if else for in do goto call exit set setlocal endlocal echo not exist defined errorlevel shift pushd popd"),
});

const SQL = lang("sql", {
  lineComments: ["--"],
  blockComments: [["/*", "*/"]],
  caseInsensitive: true,
  strings: ["'"],
  keywords: words(`select from where and or not insert into values update set delete create table view index drop alter
    add column primary key foreign references join inner left right outer full cross on as group by order having
    limit offset union all distinct case when then else end is in exists between like begin commit rollback
    transaction with returning default constraint unique check grant revoke procedure function trigger declare if`),
  types: words(`int integer bigint smallint tinyint decimal numeric float real double char varchar nvarchar text date
    time timestamp datetime boolean bool blob uuid json`),
  literals: words("null true false"),
});

const JSON_LANG = lang("json", {
  ...C_COMMENTS,
  strings: ['"'],
  literals: words("true false null"),
});

const CSS = lang("css", {
  ...C_COMMENTS,
  keywords: words("important inherit initial unset auto none"),
  metaPrefix: "@",
});

const MARKUP = lang("markup", {
  lineComments: [],
  blockComments: [["<!--", "-->"]],
  strings: [],
  markup: true,
});

const YAML = lang("yaml", {
  lineComments: ["#"],
  literals: words("true false null yes no on off ~"),
});

const TOML = lang("toml", {
  lineComments: ["#", ";"],
  literals: words("true false"),
  multilineStrings: ['"""', "'''"],
  metaLine: /^\s*\[/,
});

const LUA = lang("lua", {
  lineComments: ["--"],
  blockComments: [["--[[", "]]"]],
  keywords: words("and break do else elseif end for function goto if in local not or repeat return then until while"),
  literals: words("true false nil"),
});

const PHP = lang("php", {
  lineComments: ["//", "#"],
  blockComments: [["/*", "*/"]],
  caseInsensitive: true,
  keywords: words(`abstract and array as break callable case catch class clone const continue declare default do echo
    else elseif empty enddeclare endfor endforeach endif endswitch endwhile extends final finally fn for foreach function
    global goto if implements include include_once instanceof insteadof interface isset list match namespace new or
    print private protected public readonly require require_once return static switch throw trait try unset use var
    while yield`),
  literals: words("true false null"),
});

const BY_EXTENSION: Record<string, Language> = {};
const register = (l: Language, exts: string) => exts.split(" ").forEach((e) => (BY_EXTENSION[e] = l));
register(JS, "js jsx mjs cjs ts tsx mts cts");
register(RUST, "rs");
register(CPP, "c h cc cpp cxx hpp hh hxx ino m mm");
register(CSHARP, "cs csx");
register(JAVA, "java kt kts scala groovy gradle dart");
register(GO, "go");
register(SWIFT, "swift");
register(PYTHON, "py pyw pyi");
register(RUBY, "rb rake gemspec");
register(SHELL, "sh bash zsh ksh fish");
register(POWERSHELL, "ps1 psm1 psd1");
register(BATCH, "bat cmd");
register(SQL, "sql");
register(JSON_LANG, "json jsonc json5");
register(CSS, "css scss sass less");
register(MARKUP, "html htm xhtml xml svg xsd xsl xslt vue svelte csproj vcxproj props targets resx plist config");
register(YAML, "yml yaml");
register(TOML, "toml ini cfg conf properties editorconfig");
register(LUA, "lua");
register(PHP, "php");

/** The language for a file path (by extension), or null when unknown. */
export function languageFor(path: string): Language | null {
  const name = path.split(/[\\/]/).pop() ?? "";
  const dot = name.lastIndexOf(".");
  if (dot < 0) return /^(makefile|dockerfile|\.?bashrc|\.?zshrc|\.?profile)$/i.test(name) ? SHELL : null;
  return BY_EXTENSION[name.slice(dot + 1).toLowerCase()] ?? null;
}

// ---------- tokenizer ----------

/**
 * Lexer state at a line boundary: 0 = normal code, 1 + k = inside block
 * comment k, 100 + k = inside multi-line string k.
 */
export type LineState = number;

const isDigit = (c: string) => c >= "0" && c <= "9";
const isIdentStart = (c: string) => /[A-Za-z_$\u00C0-\uFFFF]/.test(c);
const isIdent = (c: string) => /[\w$\u00C0-\uFFFF]/.test(c);
const NUMBER = /^(?:0[xX][\da-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|(?:\d[\d_]*)?\.?\d[\d_]*(?:[eE][+-]?\d+)?)[a-zA-Z%]*/;

/** Finds the end of a string opened before `from`; returns [end, closed]. */
function scanString(text: string, from: number, quote: string): [number, boolean] {
  let i = from;
  while (i < text.length) {
    if (text[i] === "\\" && quote.length === 1) i += 2;
    else if (text.startsWith(quote, i)) return [i + quote.length, true];
    else i++;
  }
  return [text.length, false];
}

/**
 * Tokenizes one line starting in `state`. Returns the highlighted ranges and
 * the state the next line starts in. `emit` = false only computes the state.
 */
export function tokenizeLine(text: string, lng: Language, state: LineState, emit = true): { tokens: Token[]; end: LineState } {
  const tokens: Token[] = [];
  const push = (start: number, end: number, kind: TokenKind) => emit && end > start && tokens.push({ start, end, kind });
  let i = 0;

  if (state >= 100) {
    const q = lng.multilineStrings[state - 100];
    const [end, closed] = scanString(text, 0, q);
    push(0, end, "str");
    if (!closed) return { tokens, end: state };
    i = end;
  } else if (state >= 1) {
    const close = lng.blockComments[state - 1][1];
    const at = text.indexOf(close);
    if (at < 0) {
      push(0, text.length, "com");
      return { tokens, end: state };
    }
    push(0, at + close.length, "com");
    i = at + close.length;
  } else if (lng.metaLine?.test(text)) {
    // Directive lines still end comments the usual way, so only highlight up to one.
    const cut = lng.lineComments.map((c) => text.indexOf(c, 1)).filter((x) => x > 0);
    const end = cut.length ? Math.min(...cut) : text.length;
    push(0, end, "meta");
    i = end;
  }

  outer: while (i < text.length) {
    const c = text[i];
    for (const lc of lng.lineComments) {
      if (text.startsWith(lc, i) || (lng.caseInsensitive && text.slice(i, i + lc.length).toLowerCase() === lc.toLowerCase())) {
        // Batch `rem` only counts at the start of a statement.
        if (/^[a-z@]/i.test(lc) && text.slice(0, i).trim()) continue;
        push(i, text.length, "com");
        return { tokens, end: 0 };
      }
    }
    for (let k = 0; k < lng.blockComments.length; k++) {
      const [open, close] = lng.blockComments[k];
      if (!text.startsWith(open, i)) continue;
      const at = text.indexOf(close, i + open.length);
      if (at < 0) {
        push(i, text.length, "com");
        return { tokens, end: 1 + k };
      }
      push(i, at + close.length, "com");
      i = at + close.length;
      continue outer;
    }
    for (let k = 0; k < lng.multilineStrings.length; k++) {
      const q = lng.multilineStrings[k];
      if (!text.startsWith(q, i)) continue;
      const [end, closed] = scanString(text, i + q.length, q);
      push(i, end, "str");
      if (!closed) return { tokens, end: 100 + k };
      i = end;
      continue outer;
    }
    if (lng.strings.includes(c)) {
      const [end] = scanString(text, i + 1, c);
      push(i, end, "str");
      i = end;
      continue;
    }
    if (lng.markup && c === "<" && /[A-Za-z/!?]/.test(text[i + 1] ?? "")) {
      i = markupTag(text, i, push);
      continue;
    }
    if (isDigit(c) || (c === "." && isDigit(text[i + 1] ?? ""))) {
      if (i > 0 && isIdent(text[i - 1])) {
        i++;
        continue;
      }
      const m = NUMBER.exec(text.slice(i, i + 64));
      const len = m ? m[0].length : 1;
      push(i, i + len, "num");
      i += len;
      continue;
    }
    if (isIdentStart(c) || (lng.metaPrefix === c && isIdentStart(text[i + 1] ?? ""))) {
      let j = i + 1;
      while (j < text.length && isIdent(text[j])) j++;
      // Ruby's `defined?` and similar keep their trailing `?`.
      if (text[j] === "?" && lng.keywords.has(text.slice(i, j + 1))) j++;
      const word = text.slice(i, j);
      const key = lng.caseInsensitive ? word.toLowerCase() : word;
      if (c === lng.metaPrefix) push(i, j, "meta");
      else if (lng.keywords.has(key)) push(i, j, "kw");
      else if (lng.types.has(key)) push(i, j, "type");
      else if (lng.literals.has(key)) push(i, j, "lit");
      i = j;
      continue;
    }
    if (c === "$" && lng.literals.size && lng.caseInsensitive) {
      // PowerShell `$true` / `$null`.
      let j = i + 1;
      while (j < text.length && isIdent(text[j])) j++;
      if (lng.literals.has(text.slice(i, j).toLowerCase())) push(i, j, "lit");
      i = Math.max(j, i + 1);
      continue;
    }
    i++;
  }
  return { tokens, end: 0 };
}

/** Highlights `<name attr="value" …>` starting at `i`; returns where scanning resumes. */
function markupTag(text: string, i: number, push: (s: number, e: number, k: TokenKind) => void): number {
  let j = i + 1;
  if ("/!?".includes(text[j])) j++;
  const nameStart = j;
  while (j < text.length && /[\w:.-]/.test(text[j])) j++;
  push(i, j, "tag");
  if (j === nameStart) return j;
  while (j < text.length) {
    const c = text[j];
    if (c === ">" || (c === "/" && text[j + 1] === ">")) {
      const end = c === ">" ? j + 1 : j + 2;
      push(j, end, "tag");
      return end;
    }
    if (c === '"' || c === "'") {
      const [end] = scanString(text, j + 1, c);
      push(j, end, "str");
      j = end;
    } else if (/[\w:@.-]/.test(c)) {
      const s = j;
      while (j < text.length && /[\w:@.-]/.test(text[j])) j++;
      push(s, j, "attr");
    } else j++;
  }
  return j;
}

const hasLineState = (l: Language) => l.blockComments.length > 0 || l.multilineStrings.length > 0;
const stateCache = new WeakMap<string[], Map<string, Uint8Array>>();

/**
 * The lexer state at the start of every line. Computed once per line array
 * (arrays are replaced, never mutated, on edit) and only for languages whose
 * constructs can span lines.
 */
export function lineStates(lines: string[], lng: Language): Uint8Array | null {
  if (!hasLineState(lng)) return null;
  let byLang = stateCache.get(lines);
  if (!byLang) stateCache.set(lines, (byLang = new Map()));
  const cached = byLang.get(lng.name);
  if (cached) return cached;
  const states = new Uint8Array(lines.length);
  let st = 0;
  for (let i = 0; i < lines.length; i++) {
    states[i] = st;
    // Most lines contain no delimiter at all; skip the scan for them.
    st = st === 0 && !mayOpen(lines[i], lng) ? 0 : tokenizeLine(lines[i], lng, st, false).end;
  }
  byLang.set(lng.name, states);
  return states;
}

function mayOpen(text: string, lng: Language): boolean {
  for (const [open] of lng.blockComments) if (text.includes(open)) return true;
  for (const q of lng.multilineStrings) if (text.includes(q)) return true;
  return false;
}

const tokenCache = new Map<string, Token[]>();
const TOKEN_CACHE_LIMIT = 20_000;

/** Memoized `tokenizeLine` tokens for one visible line. */
export function lineTokens(text: string, lng: Language, state: LineState): Token[] {
  const key = `${lng.name}\u0001${state}\u0001${text}`;
  let t = tokenCache.get(key);
  if (!t) {
    if (tokenCache.size >= TOKEN_CACHE_LIMIT) tokenCache.clear();
    t = tokenizeLine(text, lng, state).tokens;
    tokenCache.set(key, t);
  }
  return t;
}
