//! Built-in language grammars: just enough lexing to find comments, string
//! literals, keywords and numbers in source files. Text Compare uses the
//! comment and string spans to classify differences inside them as
//! unimportant, and hands every span to the UI for syntax highlighting.
//!
//! The lexer is deliberately line-oriented and forgiving: state (open block
//! comment, open multi-line string, open markup tag) is carried from one
//! line to the next, and anything it does not understand is plain code.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TokenKind {
    Comment,
    String,
    Keyword,
    Number,
}

/// Where a line-comment token may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// Anywhere outside strings (`//`, `--`).
    Anywhere,
    /// At the start of a word: line start, or after whitespace or `;`
    /// (shell `#`, which is not a comment in `$#` or `a#b`).
    WordStart,
    /// Only whitespace before it (INI `;`, batch `REM`).
    LineStart,
}

#[derive(Debug)]
struct LineComment {
    token: &'static str,
    at: At,
}

#[derive(Debug)]
struct StringRule {
    open: &'static str,
    close: &'static str,
    /// Escapes the next character (`\` in C, `` ` `` in PowerShell). Quote
    /// doubling (`'it''s'` in SQL) needs no rule: it lexes as two strings.
    escape: Option<u8>,
    /// May span lines. Single-line strings end at an unescaped line end.
    multiline: bool,
}

#[derive(Debug)]
pub struct Grammar {
    pub id: &'static str,
    pub name: &'static str,
    /// Lower-case extensions without the dot.
    pub extensions: &'static [&'static str],
    /// Lower-case whole file names (`makefile`, `.bashrc`).
    pub file_names: &'static [&'static str],
    line_comments: &'static [LineComment],
    block_comments: &'static [(&'static str, &'static str)],
    nested_comments: bool,
    /// Tried in order, so longer openers (`"""`) come before shorter ones.
    strings: &'static [StringRule],
    keywords: &'static [&'static str],
    /// Keywords and comment tokens match case-insensitively (SQL, Pascal, VB, batch).
    case_insensitive: bool,
    /// Strings only count inside `<...>` tags (HTML/XML attribute values),
    /// so apostrophes in prose do not start one.
    markup: bool,
}

/// A classified byte range of one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineToken {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
    /// The part that "Ignore string contents" ignores: for strings the text
    /// between the delimiters, for everything else the whole token.
    pub inner_start: usize,
    pub inner_end: usize,
}

const fn lc(token: &'static str, at: At) -> LineComment {
    LineComment { token, at }
}

const fn st(open: &'static str, close: &'static str, escape: Option<u8>, multiline: bool) -> StringRule {
    StringRule {
        open,
        close,
        escape,
        multiline,
    }
}

const SLASH: &[LineComment] = &[lc("//", At::Anywhere)];
const HASH: &[LineComment] = &[lc("#", At::Anywhere)];
const HASH_WORD: &[LineComment] = &[lc("#", At::WordStart)];
const DASH: &[LineComment] = &[lc("--", At::Anywhere)];
const C_BLOCK: &[(&str, &str)] = &[("/*", "*/")];
const BS: Option<u8> = Some(b'\\');

const C_STRINGS: &[StringRule] = &[st("\"", "\"", BS, false), st("'", "'", BS, false)];
const DQ_STRING: &[StringRule] = &[st("\"", "\"", BS, false)];
const SQ_DQ_ML: &[StringRule] = &[st("\"", "\"", BS, true), st("'", "'", BS, true)];
const MARKUP_STRINGS: &[StringRule] = &[st("\"", "\"", None, true), st("'", "'", None, true)];

const C_KEYWORDS: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "constexpr",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "explicit",
    "extern",
    "false",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];
const CSHARP_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "base",
    "bool",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "decimal",
    "default",
    "delegate",
    "do",
    "double",
    "else",
    "enum",
    "event",
    "false",
    "finally",
    "float",
    "for",
    "foreach",
    "get",
    "if",
    "in",
    "int",
    "interface",
    "internal",
    "is",
    "lock",
    "long",
    "namespace",
    "new",
    "null",
    "object",
    "out",
    "override",
    "params",
    "private",
    "protected",
    "public",
    "readonly",
    "record",
    "ref",
    "return",
    "sealed",
    "set",
    "static",
    "string",
    "struct",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "using",
    "var",
    "virtual",
    "void",
    "while",
    "yield",
];
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "record",
    "return",
    "short",
    "static",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "true",
    "try",
    "var",
    "void",
    "volatile",
    "while",
];
const KOTLIN_KEYWORDS: &[&str] = &[
    "as",
    "break",
    "class",
    "continue",
    "data",
    "do",
    "else",
    "false",
    "for",
    "fun",
    "if",
    "import",
    "in",
    "interface",
    "is",
    "null",
    "object",
    "override",
    "package",
    "private",
    "public",
    "return",
    "sealed",
    "super",
    "this",
    "throw",
    "true",
    "try",
    "val",
    "var",
    "when",
    "while",
];
const SWIFT_KEYWORDS: &[&str] = &[
    "as",
    "break",
    "case",
    "class",
    "continue",
    "default",
    "defer",
    "do",
    "else",
    "enum",
    "extension",
    "false",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "init",
    "let",
    "nil",
    "private",
    "protocol",
    "public",
    "return",
    "self",
    "static",
    "struct",
    "switch",
    "throw",
    "throws",
    "true",
    "try",
    "var",
    "where",
    "while",
];
const SCALA_KEYWORDS: &[&str] = &[
    "case",
    "class",
    "def",
    "do",
    "else",
    "extends",
    "false",
    "final",
    "for",
    "if",
    "implicit",
    "import",
    "lazy",
    "match",
    "new",
    "null",
    "object",
    "override",
    "package",
    "private",
    "protected",
    "return",
    "sealed",
    "super",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "val",
    "var",
    "while",
    "with",
    "yield",
];
const JS_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
];
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
];
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
    "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal",
    "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];
const RUBY_KEYWORDS: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif", "end", "ensure",
    "false", "for", "if", "in", "module", "next", "nil", "not", "or", "redo", "rescue", "retry", "return", "self",
    "super", "then", "true", "undef", "unless", "until", "when", "while", "yield",
];
const PERL_KEYWORDS: &[&str] = &[
    "else", "elsif", "for", "foreach", "if", "last", "local", "my", "next", "our", "package", "return", "sub",
    "unless", "until", "use", "while",
];
const PHP_KEYWORDS: &[&str] = &[
    "abstract",
    "array",
    "as",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "echo",
    "else",
    "elseif",
    "extends",
    "false",
    "finally",
    "for",
    "foreach",
    "function",
    "if",
    "implements",
    "interface",
    "namespace",
    "new",
    "null",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "switch",
    "throw",
    "true",
    "try",
    "use",
    "while",
];
const SHELL_KEYWORDS: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in", "local", "return",
    "select", "then", "until", "while",
];
const POWERSHELL_KEYWORDS: &[&str] = &[
    "begin", "break", "catch", "class", "continue", "do", "else", "elseif", "end", "finally", "for", "foreach",
    "function", "if", "in", "param", "process", "return", "switch", "throw", "trap", "try", "until", "while",
];
const BATCH_KEYWORDS: &[&str] = &[
    "call", "do", "echo", "else", "endlocal", "exist", "exit", "for", "goto", "if", "in", "not", "set", "setlocal",
    "shift",
];
const SQL_KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "case",
    "check",
    "column",
    "commit",
    "constraint",
    "create",
    "cross",
    "database",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "foreign",
    "from",
    "full",
    "function",
    "grant",
    "group",
    "having",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "not",
    "null",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "procedure",
    "references",
    "return",
    "returns",
    "right",
    "rollback",
    "select",
    "set",
    "table",
    "then",
    "trigger",
    "truncate",
    "union",
    "unique",
    "update",
    "values",
    "view",
    "when",
    "where",
    "with",
];
const PASCAL_KEYWORDS: &[&str] = &[
    "and",
    "array",
    "begin",
    "case",
    "class",
    "const",
    "constructor",
    "destructor",
    "div",
    "do",
    "downto",
    "else",
    "end",
    "except",
    "finally",
    "for",
    "function",
    "if",
    "implementation",
    "in",
    "inherited",
    "interface",
    "mod",
    "nil",
    "not",
    "object",
    "of",
    "or",
    "private",
    "procedure",
    "program",
    "property",
    "protected",
    "public",
    "published",
    "record",
    "repeat",
    "then",
    "to",
    "try",
    "type",
    "unit",
    "until",
    "uses",
    "var",
    "while",
    "with",
];
const VB_KEYWORDS: &[&str] = &[
    "and", "as", "boolean", "byref", "byval", "call", "case", "class", "const", "dim", "do", "each", "else", "elseif",
    "end", "exit", "false", "for", "function", "if", "imports", "in", "integer", "is", "loop", "me", "module", "new",
    "next", "not", "nothing", "or", "private", "property", "public", "return", "select", "set", "string", "sub",
    "then", "to", "true", "until", "wend", "while", "with",
];
const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "then", "true", "until", "while",
];
const HASKELL_KEYWORDS: &[&str] = &[
    "case", "class", "data", "deriving", "do", "else", "if", "import", "in", "instance", "let", "module", "of", "then",
    "type", "where",
];
const R_KEYWORDS: &[&str] = &[
    "break", "else", "FALSE", "for", "function", "if", "in", "NA", "next", "NULL", "repeat", "return", "TRUE", "while",
];
const FORTRAN_KEYWORDS: &[&str] = &[
    "call",
    "character",
    "contains",
    "do",
    "else",
    "end",
    "function",
    "if",
    "implicit",
    "integer",
    "logical",
    "module",
    "none",
    "program",
    "real",
    "return",
    "subroutine",
    "then",
    "use",
];
const JSON_KEYWORDS: &[&str] = &["false", "null", "true"];
const YAML_KEYWORDS: &[&str] = &["false", "no", "null", "true", "yes"];

macro_rules! grammar {
    ($id:expr, $name:expr, ext: $ext:expr, names: $names:expr, line: $line:expr, block: $block:expr,
     nested: $nested:expr, strings: $strings:expr, keywords: $kw:expr, ci: $ci:expr, markup: $markup:expr) => {
        Grammar {
            id: $id,
            name: $name,
            extensions: $ext,
            file_names: $names,
            line_comments: $line,
            block_comments: $block,
            nested_comments: $nested,
            strings: $strings,
            keywords: $kw,
            case_insensitive: $ci,
            markup: $markup,
        }
    };
}

/// Every built-in grammar, in the order the UI lists them.
pub static GRAMMARS: &[Grammar] = &[
    grammar!("c", "C / C++", ext: &["c", "h", "cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "inl", "ino", "m", "mm"],
        names: &[], line: SLASH, block: C_BLOCK, nested: false, strings: C_STRINGS, keywords: C_KEYWORDS,
        ci: false, markup: false),
    grammar!("csharp", "C#", ext: &["cs", "csx"], names: &[], line: SLASH, block: C_BLOCK, nested: false,
        strings: &[st("\"\"\"", "\"\"\"", None, true), st("@\"", "\"", None, true), st("\"", "\"", BS, false),
            st("'", "'", BS, false)],
        keywords: CSHARP_KEYWORDS, ci: false, markup: false),
    grammar!("java", "Java", ext: &["java"], names: &[], line: SLASH, block: C_BLOCK, nested: false,
        strings: &[st("\"\"\"", "\"\"\"", BS, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: JAVA_KEYWORDS, ci: false, markup: false),
    grammar!("kotlin", "Kotlin", ext: &["kt", "kts"], names: &[], line: SLASH, block: C_BLOCK, nested: true,
        strings: &[st("\"\"\"", "\"\"\"", None, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: KOTLIN_KEYWORDS, ci: false, markup: false),
    grammar!("swift", "Swift", ext: &["swift"], names: &[], line: SLASH, block: C_BLOCK, nested: true,
        strings: &[st("\"\"\"", "\"\"\"", BS, true), st("\"", "\"", BS, false)],
        keywords: SWIFT_KEYWORDS, ci: false, markup: false),
    grammar!("scala", "Scala", ext: &["scala", "sc", "sbt"], names: &[], line: SLASH, block: C_BLOCK, nested: true,
        strings: &[st("\"\"\"", "\"\"\"", None, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: SCALA_KEYWORDS, ci: false, markup: false),
    grammar!("javascript", "JavaScript", ext: &["js", "mjs", "cjs", "jsx"], names: &[], line: SLASH,
        block: C_BLOCK, nested: false,
        strings: &[st("`", "`", BS, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: JS_KEYWORDS, ci: false, markup: false),
    grammar!("typescript", "TypeScript", ext: &["ts", "tsx", "mts", "cts"], names: &[], line: SLASH,
        block: C_BLOCK, nested: false,
        strings: &[st("`", "`", BS, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: JS_KEYWORDS, ci: false, markup: false),
    // `'` is left out: it is both a char literal and a lifetime (`<'a>`).
    grammar!("rust", "Rust", ext: &["rs"], names: &[], line: SLASH, block: C_BLOCK, nested: true,
        strings: &[st("\"", "\"", BS, true)], keywords: RUST_KEYWORDS, ci: false, markup: false),
    grammar!("go", "Go", ext: &["go"], names: &[], line: SLASH, block: C_BLOCK, nested: false,
        strings: &[st("`", "`", None, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: GO_KEYWORDS, ci: false, markup: false),
    grammar!("python", "Python", ext: &["py", "pyw", "pyi"], names: &["sconstruct", "sconscript"], line: HASH,
        block: &[], nested: false,
        strings: &[st("\"\"\"", "\"\"\"", BS, true), st("'''", "'''", BS, true), st("\"", "\"", BS, false),
            st("'", "'", BS, false)],
        keywords: PYTHON_KEYWORDS, ci: false, markup: false),
    grammar!("ruby", "Ruby", ext: &["rb", "rake", "gemspec", "ru"], names: &["rakefile", "gemfile", "podfile"],
        line: HASH, block: &[("=begin", "=end")], nested: false, strings: SQ_DQ_ML, keywords: RUBY_KEYWORDS,
        ci: false, markup: false),
    grammar!("perl", "Perl", ext: &["pl", "pm", "t"], names: &[], line: HASH_WORD, block: &[], nested: false,
        strings: SQ_DQ_ML, keywords: PERL_KEYWORDS, ci: false, markup: false),
    grammar!("php", "PHP", ext: &["php", "phtml", "php4", "php5"], names: &[],
        line: &[lc("//", At::Anywhere), lc("#", At::Anywhere)], block: C_BLOCK, nested: false, strings: SQ_DQ_ML,
        keywords: PHP_KEYWORDS, ci: false, markup: false),
    grammar!("shell", "Shell", ext: &["sh", "bash", "zsh", "ksh", "fish"],
        names: &[".bashrc", ".bash_profile", ".zshrc", ".profile", ".zprofile"], line: HASH_WORD, block: &[],
        nested: false, strings: &[st("\"", "\"", BS, true), st("'", "'", None, true)], keywords: SHELL_KEYWORDS,
        ci: false, markup: false),
    grammar!("powershell", "PowerShell", ext: &["ps1", "psm1", "psd1"], names: &[], line: HASH,
        block: &[("<#", "#>")], nested: false,
        strings: &[st("\"", "\"", Some(b'`'), true), st("'", "'", None, true)], keywords: POWERSHELL_KEYWORDS,
        ci: true, markup: false),
    grammar!("batch", "Batch", ext: &["bat", "cmd"], names: &[],
        line: &[lc("::", At::LineStart), lc("rem", At::LineStart), lc("@rem", At::LineStart)], block: &[],
        nested: false, strings: &[st("\"", "\"", None, false)], keywords: BATCH_KEYWORDS, ci: true, markup: false),
    grammar!("sql", "SQL", ext: &["sql", "ddl", "dml", "pks", "pkb", "prc", "fnc", "trg", "vw"], names: &[],
        line: DASH, block: C_BLOCK, nested: false, strings: &[st("'", "'", None, true)], keywords: SQL_KEYWORDS,
        ci: true, markup: false),
    grammar!("pascal", "Pascal / Delphi", ext: &["pas", "pp", "dpr", "dpk", "lpr", "dfm", "lfm"], names: &[],
        line: SLASH, block: &[("{", "}"), ("(*", "*)")], nested: false, strings: &[st("'", "'", None, false)],
        keywords: PASCAL_KEYWORDS, ci: true, markup: false),
    grammar!("vb", "Visual Basic", ext: &["vb", "vbs", "bas", "cls", "frm", "vba"], names: &[],
        line: &[lc("'", At::Anywhere), lc("rem", At::WordStart)], block: &[], nested: false,
        strings: &[st("\"", "\"", None, false)], keywords: VB_KEYWORDS, ci: true, markup: false),
    grammar!("html", "HTML", ext: &["html", "htm", "xhtml", "shtml", "vue", "svelte", "cshtml", "razor", "aspx", "jsp"],
        names: &[], line: &[], block: &[("<!--", "-->")], nested: false, strings: MARKUP_STRINGS, keywords: &[],
        ci: false, markup: true),
    grammar!("xml", "XML", ext: &["xml", "xsd", "xsl", "xslt", "svg", "csproj", "vbproj", "vcxproj", "fsproj",
            "props", "targets", "config", "resx", "plist", "xaml", "wsdl", "nuspec", "manifest", "pom"],
        names: &[], line: &[], block: &[("<!--", "-->")], nested: false, strings: MARKUP_STRINGS, keywords: &[],
        ci: false, markup: true),
    grammar!("css", "CSS", ext: &["css"], names: &[], line: &[], block: C_BLOCK, nested: false,
        strings: C_STRINGS, keywords: &[], ci: false, markup: false),
    grammar!("scss", "SCSS / Less", ext: &["scss", "sass", "less"], names: &[], line: SLASH, block: C_BLOCK,
        nested: false, strings: C_STRINGS, keywords: &[], ci: false, markup: false),
    grammar!("ini", "INI / properties", ext: &["ini", "cfg", "conf", "properties", "inf", "reg", "editorconfig",
            "gitconfig", "desktop", "url", "env"],
        names: &[".gitconfig", ".editorconfig", ".npmrc", ".env"],
        line: &[lc(";", At::LineStart), lc("#", At::LineStart)], block: &[], nested: false, strings: &[],
        keywords: &[], ci: false, markup: false),
    grammar!("toml", "TOML", ext: &["toml"], names: &["cargo.lock"], line: HASH, block: &[], nested: false,
        strings: &[st("\"\"\"", "\"\"\"", BS, true), st("'''", "'''", None, true), st("\"", "\"", BS, false),
            st("'", "'", None, false)],
        keywords: JSON_KEYWORDS, ci: false, markup: false),
    grammar!("yaml", "YAML", ext: &["yml", "yaml"], names: &[], line: HASH_WORD, block: &[], nested: false,
        strings: &[st("\"", "\"", BS, true), st("'", "'", None, true)], keywords: YAML_KEYWORDS, ci: false,
        markup: false),
    // Comments are not JSON, but JSONC / JSON5 files (VS Code settings, tsconfig) have them.
    grammar!("json", "JSON", ext: &["json", "jsonc", "json5", "code-workspace", "babelrc", "eslintrc"], names: &[],
        line: SLASH, block: C_BLOCK, nested: false, strings: DQ_STRING, keywords: JSON_KEYWORDS, ci: false,
        markup: false),
    grammar!("lua", "Lua", ext: &["lua"], names: &[], line: DASH, block: &[("--[[", "]]")], nested: false,
        strings: &[st("[[", "]]", None, true), st("\"", "\"", BS, false), st("'", "'", BS, false)],
        keywords: LUA_KEYWORDS, ci: false, markup: false),
    grammar!("haskell", "Haskell", ext: &["hs", "lhs", "elm"], names: &[], line: DASH, block: &[("{-", "-}")],
        nested: true, strings: DQ_STRING, keywords: HASKELL_KEYWORDS, ci: false, markup: false),
    grammar!("r", "R", ext: &["r", "rmd"], names: &[], line: HASH, block: &[], nested: false, strings: SQ_DQ_ML,
        keywords: R_KEYWORDS, ci: false, markup: false),
    grammar!("fortran", "Fortran", ext: &["f90", "f95", "f03", "f08", "for", "f"], names: &[],
        line: &[lc("!", At::Anywhere)], block: &[], nested: false,
        strings: &[st("'", "'", None, false), st("\"", "\"", None, false)], keywords: FORTRAN_KEYWORDS, ci: true,
        markup: false),
    grammar!("makefile", "Makefile", ext: &["mk", "mak", "make"], names: &["makefile", "gnumakefile"], line: HASH,
        block: &[], nested: false, strings: &[], keywords: &[], ci: false, markup: false),
    grammar!("dockerfile", "Dockerfile", ext: &["dockerfile"], names: &["dockerfile", "containerfile"],
        line: &[lc("#", At::LineStart)], block: &[], nested: false, strings: &[st("\"", "\"", BS, false)],
        keywords: &[], ci: false, markup: false),
    grammar!("markdown", "Markdown", ext: &["md", "markdown"], names: &[], line: &[], block: &[("<!--", "-->")],
        nested: false, strings: &[], keywords: &[], ci: false, markup: false),
];

/// Looks a grammar up by its id.
pub fn by_id(id: &str) -> Option<&'static Grammar> {
    GRAMMARS.iter().find(|g| g.id.eq_ignore_ascii_case(id))
}

/// Picks the grammar for a file from its name, then its extension.
/// `remote://` paths work too: only the last path segment is looked at.
pub fn for_path(path: &str) -> Option<&'static Grammar> {
    let name = Path::new(path).file_name()?.to_string_lossy().to_lowercase();
    if let Some(g) = GRAMMARS.iter().find(|g| g.file_names.contains(&name.as_str())) {
        return Some(g);
    }
    let ext = Path::new(&name).extension()?.to_string_lossy().into_owned();
    GRAMMARS.iter().find(|g| g.extensions.contains(&ext.as_str()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    None,
    Block { idx: usize, depth: usize },
    Str { idx: usize },
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `line[i..]` starts with `token` (ASCII), optionally ignoring case.
fn starts_at(line: &[u8], i: usize, token: &str, ci: bool) -> bool {
    let t = token.as_bytes();
    line.len() >= i + t.len()
        && if ci {
            line[i..i + t.len()].eq_ignore_ascii_case(t)
        } else {
            &line[i..i + t.len()] == t
        }
}

impl Grammar {
    fn is_keyword(&self, word: &str) -> bool {
        if self.case_insensitive {
            self.keywords.iter().any(|k| k.eq_ignore_ascii_case(word))
        } else {
            self.keywords.contains(&word)
        }
    }

    fn line_comment_at(&self, line: &[u8], i: usize) -> bool {
        self.line_comments.iter().any(|c| {
            if !starts_at(line, i, c.token, self.case_insensitive) {
                return false;
            }
            // Word-like tokens (`rem`) must not run into an identifier (`remark`).
            let end = i + c.token.len();
            if c.token.as_bytes().last().is_some_and(|&b| is_ident(b)) && line.get(end).is_some_and(|&b| is_ident(b)) {
                return false;
            }
            match c.at {
                At::Anywhere => true,
                At::WordStart => i == 0 || matches!(line[i - 1], b' ' | b'\t' | b';'),
                At::LineStart => line[..i].iter().all(|b| b.is_ascii_whitespace()),
            }
        })
    }

    /// Scans a block comment body from `i`; returns where it ends (after the
    /// closing token) or `None` with the updated depth if it runs past the line.
    fn scan_block(&self, line: &[u8], mut i: usize, idx: usize, mut depth: usize) -> (Option<usize>, usize) {
        let (open, close) = self.block_comments[idx];
        while i < line.len() {
            if self.nested_comments && starts_at(line, i, open, false) {
                depth += 1;
                i += open.len();
            } else if starts_at(line, i, close, false) {
                depth -= 1;
                i += close.len();
                if depth == 0 {
                    return (Some(i), 0);
                }
            } else {
                i += 1;
            }
        }
        (None, depth)
    }

    /// Scans a string body from `i`; returns `(content end, token end)` when
    /// it closes on this line, `None` when it runs to the end of the line.
    fn scan_string(&self, line: &[u8], mut i: usize, idx: usize) -> Option<(usize, usize)> {
        let rule = &self.strings[idx];
        while i < line.len() {
            if Some(line[i]) == rule.escape {
                i += 2;
            } else if starts_at(line, i, rule.close, false) {
                return Some((i, i + rule.close.len()));
            } else {
                i += 1;
            }
        }
        None
    }

    /// Classifies every line. Comment and string state carries across lines;
    /// keyword and number tokens are only produced when `with_words` is set
    /// (they are only needed for highlighting).
    pub fn tokenize(&self, lines: &[&str], with_words: bool) -> Vec<Vec<LineToken>> {
        let mut open = Open::None;
        let mut in_tag = false;
        lines
            .iter()
            .map(|line| self.tokenize_line(line.as_bytes(), &mut open, &mut in_tag, with_words))
            .collect()
    }

    fn tokenize_line(&self, line: &[u8], open: &mut Open, in_tag: &mut bool, with_words: bool) -> Vec<LineToken> {
        let mut out = Vec::new();
        let mut push = |start, end, kind, inner_start, inner_end| {
            if end > start {
                out.push(LineToken {
                    start,
                    end,
                    kind,
                    inner_start,
                    inner_end,
                });
            }
        };
        let len = line.len();
        let mut i = 0;
        // Finish whatever the previous line left open.
        match *open {
            Open::None => {}
            Open::Block { idx, depth } => match self.scan_block(line, 0, idx, depth) {
                (Some(end), _) => {
                    push(0, end, TokenKind::Comment, 0, end);
                    *open = Open::None;
                    i = end;
                }
                (None, depth) => {
                    push(0, len, TokenKind::Comment, 0, len);
                    *open = Open::Block { idx, depth };
                    return out;
                }
            },
            Open::Str { idx } => match self.scan_string(line, 0, idx) {
                Some((inner_end, end)) => {
                    push(0, end, TokenKind::String, 0, inner_end);
                    *open = Open::None;
                    i = end;
                }
                None => {
                    push(0, len, TokenKind::String, 0, len);
                    return out;
                }
            },
        }

        'scan: while i < len {
            let b = line[i];
            if let Some(idx) = self
                .block_comments
                .iter()
                .position(|(o, _)| starts_at(line, i, o, false))
            {
                let body = i + self.block_comments[idx].0.len();
                match self.scan_block(line, body, idx, 1) {
                    (Some(end), _) => {
                        push(i, end, TokenKind::Comment, i, end);
                        i = end;
                        continue;
                    }
                    (None, depth) => {
                        push(i, len, TokenKind::Comment, i, len);
                        *open = Open::Block { idx, depth };
                        break;
                    }
                }
            }
            if self.line_comment_at(line, i) {
                push(i, len, TokenKind::Comment, i, len);
                break;
            }
            if self.markup {
                match b {
                    b'<' => *in_tag = true,
                    b'>' => *in_tag = false,
                    _ => {}
                }
            }
            if !self.markup || *in_tag {
                for (idx, rule) in self.strings.iter().enumerate() {
                    if !starts_at(line, i, rule.open, false) {
                        continue;
                    }
                    let body = i + rule.open.len();
                    match self.scan_string(line, body, idx) {
                        Some((inner_end, end)) => {
                            push(i, end, TokenKind::String, body, inner_end);
                            i = end;
                        }
                        None => {
                            push(i, len, TokenKind::String, body, len);
                            // An escape right at the line end continues the string.
                            let continued = line.last() == rule.escape.as_ref() && len > body;
                            if rule.multiline || continued {
                                *open = Open::Str { idx };
                            }
                            i = len;
                        }
                    }
                    continue 'scan;
                }
            }
            let after_ident = i > 0 && is_ident(line[i - 1]);
            if with_words && !after_ident && (b.is_ascii_alphabetic() || b == b'_') {
                let mut end = i + 1;
                while end < len && is_ident(line[end]) {
                    end += 1;
                }
                // Ruby's `defined?` and similar keep their trailing `?`.
                if end < len && line[end] == b'?' && self.is_keyword(&String::from_utf8_lossy(&line[i..=end])) {
                    end += 1;
                }
                if self.is_keyword(&String::from_utf8_lossy(&line[i..end])) {
                    push(i, end, TokenKind::Keyword, i, end);
                }
                i = end;
                continue;
            }
            if with_words && !after_ident && b.is_ascii_digit() {
                let mut end = i + 1;
                while end < len && (is_ident(line[end]) || line[end] == b'.') {
                    end += 1;
                }
                push(i, end, TokenKind::Number, i, end);
                i = end;
                continue;
            }
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(g: &Grammar, text: &str) -> Vec<Vec<(String, TokenKind)>> {
        let lines: Vec<&str> = text.lines().collect();
        g.tokenize(&lines, true)
            .iter()
            .zip(&lines)
            .map(|(toks, line)| toks.iter().map(|t| (line[t.start..t.end].to_owned(), t.kind)).collect())
            .collect()
    }

    fn comments_and_strings(g: &Grammar, text: &str) -> Vec<Vec<String>> {
        kinds(g, text)
            .into_iter()
            .map(|l| {
                l.into_iter()
                    .filter(|(_, k)| matches!(k, TokenKind::Comment | TokenKind::String))
                    .map(|(t, _)| t)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn detects_grammar_from_file_name() {
        assert_eq!(for_path("/src/main.rs").unwrap().id, "rust");
        assert_eq!(for_path("C:\\x\\Unit1.PAS").unwrap().id, "pascal");
        assert_eq!(for_path("remote://s3/dir/Makefile").unwrap().id, "makefile");
        assert_eq!(for_path("query.sql").unwrap().id, "sql");
        assert_eq!(for_path("a.tsx").unwrap().id, "typescript");
        assert!(for_path("README").is_none());
        assert_eq!(by_id("PYTHON").unwrap().name, "Python");
        // Ids and extensions are unique.
        let mut ids: Vec<_> = GRAMMARS.iter().map(|g| g.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), GRAMMARS.len());
        let mut exts: Vec<_> = GRAMMARS.iter().flat_map(|g| g.extensions.iter()).collect();
        let n = exts.len();
        exts.sort_unstable();
        exts.dedup();
        assert_eq!(exts.len(), n);
    }

    #[test]
    fn c_comments_and_strings() {
        let g = by_id("c").unwrap();
        let out = comments_and_strings(
            g,
            "int a = 1; // one\nchar *s = \"a // b\\\" c\"; /* x */ int b;\n/* start\n middle\nend */ x = '\\'';",
        );
        assert_eq!(out[0], vec!["// one"]);
        assert_eq!(out[1], vec!["\"a // b\\\" c\"", "/* x */"]);
        assert_eq!(out[2], vec!["/* start"]);
        assert_eq!(out[3], vec![" middle"]);
        assert_eq!(out[4], vec!["end */", "'\\''"]);
    }

    #[test]
    fn keywords_and_numbers() {
        let g = by_id("rust").unwrap();
        let k = kinds(g, "pub fn f2(x: u8) -> u8 { return 0x1F + x; }");
        assert_eq!(
            k[0],
            vec![
                ("pub".into(), TokenKind::Keyword),
                ("fn".into(), TokenKind::Keyword),
                ("return".into(), TokenKind::Keyword),
                ("0x1F".into(), TokenKind::Number),
            ]
        );
        // Case-insensitive keywords in SQL; `--` comments.
        let k = kinds(by_id("sql").unwrap(), "Select 'it''s' -- note");
        assert_eq!(k[0][0], ("Select".into(), TokenKind::Keyword));
        assert_eq!(k[0][3], ("-- note".into(), TokenKind::Comment));
    }

    #[test]
    fn python_triple_quoted_strings_span_lines() {
        let out = comments_and_strings(
            by_id("python").unwrap(),
            "x = \"\"\"doc\n# not a comment\n\"\"\"  # real",
        );
        assert_eq!(out[0], vec!["\"\"\"doc"]);
        assert_eq!(out[1], vec!["# not a comment"]);
        assert_eq!(out[2], vec!["\"\"\"", "# real"]);
        let toks = by_id("python").unwrap().tokenize(&["s = 'abc'"], false);
        let t = toks[0][0];
        assert_eq!((t.start, t.end, t.inner_start, t.inner_end), (4, 9, 5, 8));
    }

    #[test]
    fn shell_hash_needs_a_word_start() {
        let out = comments_and_strings(by_id("shell").unwrap(), "echo $# a#b # c\necho 'x # y';# z");
        assert_eq!(out[0], vec!["# c"]);
        assert_eq!(out[1], vec!["'x # y'", "# z"]);
    }

    #[test]
    fn nested_block_comments() {
        let out = comments_and_strings(by_id("rust").unwrap(), "/* a /* b */ c */ x\n/* /* */\n*/ y");
        assert_eq!(out[0], vec!["/* a /* b */ c */"]);
        assert_eq!(out[1], vec!["/* /* */"]);
        assert_eq!(out[2], vec!["*/"]);
    }

    #[test]
    fn markup_strings_only_inside_tags() {
        let out = comments_and_strings(
            by_id("html").unwrap(),
            "<p class=\"x\">don't stop</p> <!-- note -->\n<a\n href='y'>",
        );
        assert_eq!(out[0], vec!["\"x\"", "<!-- note -->"]);
        assert_eq!(out[2], vec!["'y'"]);
    }

    #[test]
    fn line_start_and_word_comments() {
        let out = comments_and_strings(by_id("ini").unwrap(), "; top\nkey = a;b # not\n  # yes");
        assert_eq!(out, vec![vec!["; top"], vec![], vec!["# yes"]]);
        let out = comments_and_strings(by_id("batch").unwrap(), "REM hi\nremark x\n:: lbl\nrem");
        assert_eq!(out, vec![vec!["REM hi"], vec![], vec![":: lbl"], vec!["rem"]]);
        let out = comments_and_strings(by_id("vb").unwrap(), "x = \"it's\" ' note");
        assert_eq!(out[0], vec!["\"it's\"", "' note"]);
    }

    #[test]
    fn pascal_and_powershell_blocks() {
        let out = comments_and_strings(by_id("pascal").unwrap(), "{ a } x := 'q'; (* b *) // c");
        assert_eq!(out[0], vec!["{ a }", "'q'", "(* b *)", "// c"]);
        let out = comments_and_strings(by_id("powershell").unwrap(), "<# doc\n#> $a = \"x`\"y\" # c");
        assert_eq!(out[0], vec!["<# doc"]);
        assert_eq!(out[1], vec!["#>", "\"x`\"y\"", "# c"]);
    }

    #[test]
    fn unterminated_single_line_string_ends_at_eol() {
        let out = comments_and_strings(by_id("c").unwrap(), "s = \"open\nint x; // c");
        assert_eq!(out[1], vec!["// c"]);
        // ... unless the line ends in an escape (line continuation).
        let out = comments_and_strings(by_id("c").unwrap(), "s = \"a\\\nb\"; // c");
        assert_eq!(out[1], vec!["b\"", "// c"]);
    }
}
