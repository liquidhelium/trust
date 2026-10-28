//! Pretty-printing of generated code.
//!
//! prettyplease formats what syn parses — but the bulk of the generated
//! code carries our runtime DSL inside `rt::file_module!` / `call!` /
//! `mcall!` / `args!` invocations, which syn holds as opaque token
//! streams. prettyplease has no plugin interface (its expression-level
//! macro support is a private, hardcoded standard-library list), so
//! those token streams would be wrapped at arbitrary token boundaries.
//!
//! This module is a DSL-aware layer over prettyplease instead:
//!
//! - pure-Rust stretches (top-level items, statements, expressions) are
//!   formatted by prettyplease itself;
//! - our macro invocations are laid out by our own rules — segments one
//!   per line, value blocks broken when long — borrowing prettyplease
//!   for every real-Rust sub-piece (the shell trick: wrap in a `fn`,
//!   parse, `unparse`, strip the shell; spacing comes out canonical
//!   because it is re-printed from the AST);
//! - **token streams are preserved exactly**; only whitespace changes.
//!   `rt::span_table!` tables stay verbatim (machine data; one pair per
//!   line would add megabytes without reading benefit).
//!
//! Two invariants (see also the CLI's `--no-fmt`):
//! - **Never fail.** Any parse error, shell-extraction mismatch, or
//!   placeholder anomaly falls back to the original text. Formatting is
//!   a post-process and must never be the reason output cannot be
//!   produced.
//! - **Never change meaning.** Every formatted chunk is verified by
//!   token-stream comparison against its original; any mismatch falls
//!   back to the original bytes. (Comments are not tokens: `/* never */`
//!   and `/* trust-degraded: */` markers inside reformatted regions are
//!   dropped by the round-trip; the degraded messages survive in the
//!   `rt::degraded(...)` call arguments.)

use std::ops::Range;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::parse::{Parse as _, Parser as _};

/// Aim width for the DSL layer (prettyplease keeps its own margins).
const WIDTH: usize = 96;
/// Inline threshold for segment value blocks.
const BLOCK_INLINE: usize = 60;

/// Shell identifiers for the wrap-and-strip trick. The emitter never
/// produces them; if they occur anyway (host reuse), formatting of the
/// chunk falls back to verbatim — no silent collision.
const SHELL_FN: &str = "__trust_fmt_shell";
const SHELL_LET: &str = "__trust_fmt_v";
const PLACEHOLDER: &str = "__trust_fmt_site_";

/// Entry point: pretty-print a generated Rust source file.
pub fn format_generated(src: &str) -> String {
    // Split off the leading `//` block (banner + degradation list):
    // comments do not survive the token round-trip, so it is re-attached.
    let mut header_end = 0;
    for line in src.split_inclusive('\n') {
        if line.starts_with("//") {
            header_end += line.len();
        } else {
            break;
        }
    }
    let (header, body) = src.split_at(header_end);
    let mut out = String::with_capacity(src.len() + src.len() / 8);
    out.push_str(header);
    if !header.is_empty() {
        out.push('\n');
    }
    out.push_str(&format_items(body));
    out
}

// ------------------------------------------------------------------ items

/// Format a stretch of top-level items. Separators between items (blank
/// lines, comments) are preserved verbatim; each item is formatted (or
/// passed through) individually with a token-equality guard.
fn format_items(text: &str) -> String {
    let Ok(file) = syn::parse_file(text) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut pos = 0usize;
    for item in &file.items {
        let Some(range) = node_range(item) else {
            // No located span: keep the rest verbatim (not expected
            // outside proc macros, where byte ranges are exact).
            out.push_str(&text[pos..]);
            return out;
        };
        out.push_str(&text[pos..range.start]);
        let original = &text[range.clone()];
        // Item spans are in file coordinates; the item text starts at
        // `range.start` (the Fn-body slice below must be rebased).
        let formatted = format_item(original, item, range.start);
        if tokens_equal(original, &formatted) {
            out.push_str(&formatted);
        } else {
            if std::env::var_os("TRUST_FMT_DEBUG").is_some() {
                eprintln!("== original ==\n{original}\n== formatted ==\n{formatted}");
            }
            out.push_str(original);
        }
        if !out.ends_with('\n') && !text[range.end..].starts_with('\n') {
            out.push('\n');
        }
        pos = range.end;
    }
    out.push_str(&text[pos..]);
    // Collapse blank-line runs (item printouts end with a newline; the
    // original separators add their own). Generated strings escape
    // newlines, so this cannot touch literal contents.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

/// Located byte range of a syn node (joined span).
fn node_range<T: syn::spanned::Spanned>(node: &T) -> Option<Range<usize>> {
    let span = node.span();
    let start = span.start();
    (start.line != 0)
        .then(|| span.byte_range())
        .filter(|r| r.start < r.end)
}

fn format_item(original: &str, item: &syn::Item, offset: usize) -> String {
    match item {
        syn::Item::Macro(mac) => {
            let name = mac.mac.path.segments.last().map(|s| s.ident.to_string());
            match name.as_deref() {
                Some("file_module") => fmt_file_module(&mac.mac, original, offset)
                    .unwrap_or_else(|| original.to_string()),
                // span_table! (data table) and anything unknown: verbatim.
                _ => original.to_string(),
            }
        }
        syn::Item::Fn(fn_item) => {
            if find_rt_macros(original).is_empty() {
                pp_item(item)
            } else {
                // Head (attributes + signature) via prettyplease on an
                // empty-block clone; body via the macro-aware statement
                // formatter, sliced from the original item text.
                let Some(range) = node_range(&fn_item.block) else {
                    return original.to_string();
                };
                let mut head_item = fn_item.clone();
                head_item.block = Box::new(syn::Block {
                    brace_token: Default::default(),
                    stmts: Vec::new(),
                });
                let printed = pp_item(&syn::Item::Fn(head_item));
                let Some(head) = printed
                    .strip_suffix(" {}\n")
                    .or_else(|| printed.strip_suffix(" {\n}\n"))
                else {
                    return original.to_string();
                };
                let start = range.start - offset;
                let end = range.end - offset;
                let interior = &original[start + 1..end - 1];
                format!(
                    "{head} {{\n{}\n}}",
                    protect_format(interior, Mode::Stmts, 4)
                )
            }
        }
        _ => {
            if find_rt_macros(original).is_empty() {
                pp_item(item)
            } else {
                // Unexpected shape: do not guess.
                original.to_string()
            }
        }
    }
}

/// Format one pure-Rust item with prettyplease.
fn pp_item(item: &syn::Item) -> String {
    let file = syn::File {
        attrs: Vec::new(),
        shebang: None,
        items: vec![item.clone()],
    };
    prettyplease::unparse(&file)
}

// ------------------------------------------------ statements / expressions

/// What a stretch of code is, for the shell trick.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// A statement list (block interior).
    Stmts,
    /// A single expression.
    Expr,
}

/// Format a stretch of Rust that may contain `rt::call!` / `mcall!` /
/// `args!` sites: each site is laid out by our DSL rules first, a
/// placeholder stands in while prettyplease formats the surrounding
/// Rust, then the formatted site text is spliced back at the
/// placeholder's column (continuation lines keep their relative
/// indentation).
fn protect_format(text: &str, mode: Mode, indent: usize) -> String {
    let sites = find_rt_macros(text);
    if sites.is_empty() {
        return pp_shell(text, mode, indent);
    }
    if text.contains(PLACEHOLDER) || text.contains(SHELL_FN) {
        return text.to_string(); // never expected; refuse silently
    }
    // Format each site relative to column 0 (recursion: value blocks
    // inside segments go through `protect_format` again).
    let formatted: Vec<String> = sites
        .iter()
        .map(|r| fmt_macro_site(&text[r.clone()]).unwrap_or_else(|| text[r.clone()].to_string()))
        .collect();
    // Placeholder text: unique call expressions at the sites.
    let mut ph = String::with_capacity(text.len());
    let mut prev = 0usize;
    for (i, r) in sites.iter().enumerate() {
        ph.push_str(&text[prev..r.start]);
        ph.push_str(&format!("{PLACEHOLDER}{i}()"));
        prev = r.end;
    }
    ph.push_str(&text[prev..]);
    let mut pretty = pp_shell(&ph, mode, indent);
    // Splice each formatted site back at the placeholder's column.
    for (i, site) in formatted.iter().enumerate() {
        let needle = format!("{PLACEHOLDER}{i}()");
        let Some(at) = find_unique(&pretty, &needle) else {
            return text.to_string();
        };
        let line_start = pretty[..at].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let column = at - line_start;
        let shifted = shift_tail_lines(site, column);
        pretty.replace_range(at..at + needle.len(), &shifted);
    }
    pretty
}

/// Byte offset of the only occurrence of `needle`, if unique.
fn find_unique(hay: &str, needle: &str) -> Option<usize> {
    let at = hay.find(needle)?;
    hay[at + needle.len()..]
        .find(needle)
        .is_none()
        .then_some(at)
}

/// Indent every line except the first by `n` spaces (the first line
/// takes the placeholder's own position).
fn shift_tail_lines(text: &str, n: usize) -> String {
    if n == 0 || !text.contains('\n') {
        return text.to_string();
    }
    let pad = " ".repeat(n);
    let mut out = String::with_capacity(text.len() + n * text.matches('\n').count());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(&pad);
            }
        }
        out.push_str(line);
    }
    out
}

/// Indent every non-empty line by `n` spaces.
fn shift_all_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return text.to_string();
    }
    let pad = " ".repeat(n);
    let mut out = String::with_capacity(text.len() + n * text.lines().count());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push_str(&pad);
        }
        out.push_str(line);
    }
    out
}

/// Remove up to `n` leading spaces from every line.
fn dedent_lines(text: &str, n: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let strip = line.bytes().take_while(|b| *b == b' ').count().min(n);
        out.push_str(&line[strip..]);
    }
    out
}

/// The shell trick: format `text` (of the given `mode`) by wrapping it
/// in a synthetic `fn`, letting prettyplease format that fn, and
/// stripping the shell. The result is indented to `indent`. Any
/// surprise returns the input unchanged.
fn pp_shell(text: &str, mode: Mode, indent: usize) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    let shell = match mode {
        Mode::Stmts => format!("fn {SHELL_FN}() {{ {text} }}"),
        // The static form's prefix is 10 columns shorter than a fn's
        // `let`, so medium-length expressions are not pushed over
        // prettyplease's margin (syn parses `_` here; nothing is type
        // checked).
        Mode::Expr => format!("static {SHELL_LET}: _ = {text};"),
    };
    let Ok(file) = syn::parse_file(&shell) else {
        return text.to_string();
    };
    let out = prettyplease::unparse(&file);
    let body = match mode {
        Mode::Stmts => out
            .strip_prefix(&format!("fn {SHELL_FN}() {{\n"))
            .and_then(|b| b.strip_suffix("}\n"))
            .map(str::to_string),
        Mode::Expr => out
            .strip_prefix(&format!("static {SHELL_LET}: _ = "))
            .and_then(|b| b.strip_suffix(";\n"))
            .map(str::to_string),
    };
    let Some(body) = body else {
        return text.to_string();
    };
    let extracted = match mode {
        Mode::Stmts => {
            // Statement lines sit at shell indent 4.
            let e = body.strip_suffix('\n').unwrap_or(&body);
            dedent_lines(e, 4)
        }
        // The static prefix/suffix strip leaves the bare expression;
        // continuation lines sit at shell indent 4.
        Mode::Expr => dedent_lines(&body, 4),
    };
    if !tokens_equal(text, &extracted) {
        return text.to_string();
    }
    shift_all_lines(&extracted, indent)
}

/// Single-line text for head expressions (receiver lists, segment
/// heads): a small spacing-aware token printer. Heads are one-liners by
/// policy (prettyplease's own margin would break medium-length ones);
/// the head vocabulary is closed — identifiers, calls, field access,
/// suffix `?`, unary `&`/`*` — so these rules cover everything the
/// emitter produces there. Spacing can never change tokens, so a
/// surprise is cosmetic at worst.
fn head_text(tts: &[TokenTree]) -> String {
    let mut out = String::new();
    for i in 0..tts.len() {
        if i > 0 && spaced(&tts[i - 1], &tts[i], i.checked_sub(2).map(|j| &tts[j])) {
            out.push(' ');
        }
        match &tts[i] {
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                out.push_str(open);
                out.push_str(&head_text(&inner));
                out.push_str(close);
            }
            other => out.push_str(&other.to_string()),
        }
    }
    out
}

/// Whether a space goes between two adjacent tokens (`prev` is the
/// token before `a`, for operator-tail and unary detection).
fn spaced(a: &TokenTree, b: &TokenTree, prev: Option<&TokenTree>) -> bool {
    use proc_macro2::Spacing;
    let is_joint =
        |t: &TokenTree| matches!(t, TokenTree::Punct(p) if p.spacing() == Spacing::Joint);
    let punct = |t: &TokenTree, c: char| matches!(t, TokenTree::Punct(p) if p.as_char() == c);
    let is_value = |t: &TokenTree| {
        matches!(
            t,
            TokenTree::Ident(_) | TokenTree::Literal(_) | TokenTree::Group(_)
        )
    };
    // Multi-punct operators (`::`, `->`, `>=`, …) stay glued. Joint only
    // means "no whitespace follows", so the tail rule is restricted to
    // real operator tails (`:` of `::`, `>` of `->`).
    if is_joint(b) || is_joint(a) {
        return false;
    }
    if let Some(p) = prev
        && is_joint(p)
        && punct(a, ':')
    {
        return false;
    }
    if let TokenTree::Punct(q) = b {
        if matches!(q.as_char(), ',' | ';' | ')' | ']' | '.' | '?') {
            return false;
        }
        // Unary operators glue to their operand after an operator or an
        // opener (after `,`/`;` a space stays).
        if matches!(q.as_char(), '&' | '*' | '-') && !matches!(a, TokenTree::Ident(_)) {
            return !matches!(a, TokenTree::Punct(p) if matches!(p.as_char(), ',' | ';'));
        }
    }
    // Openers, field access, and unary operators glue to what follows.
    if let TokenTree::Punct(p) = a {
        if matches!(p.as_char(), '(' | '[' | '.' | '!') {
            return false;
        }
        if matches!(p.as_char(), '&' | '*') && !prev.is_some_and(is_value) {
            return false;
        }
    }
    // Call/index position: `f(x)`, `a[i]`, `g()(y)`.
    if let TokenTree::Group(g) = b
        && matches!(g.delimiter(), Delimiter::Parenthesis | Delimiter::Bracket)
        && is_value(a)
    {
        return false;
    }
    true
}

// ------------------------------------------------------------- macro sites

/// Macro names whose invocations get the DSL layout: the `;`-shaped
/// argument macros and the bracket-shaped sequence macros.
const SEGMENT_MACROS: [&str; 5] = ["call", "mcall", "args", "markup", "cseq"];

/// Byte ranges of `rt::<SEGMENT_MACROS>!` invocations in `text`
/// (located spans; ranges cover `rt::…!(…)`).
fn find_rt_macros(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if let Ok(ts) = text.parse::<TokenStream>() {
        walk_tokens(ts, &mut out);
    }
    out.sort_by_key(|r| r.start);
    out
}

/// Scan a token stream for `rt::<name>!` followed by a group. Non-macro
/// groups are descended into (sites nest inside e.g. `vec![…]`
/// arguments); macro bodies themselves are not descended (their nesting
/// is handled inside `fmt_macro_site`).
fn walk_tokens(ts: TokenStream, out: &mut Vec<Range<usize>>) {
    let tts: Vec<TokenTree> = ts.into_iter().collect();
    let mut i = 0usize;
    while i < tts.len() {
        if let TokenTree::Punct(p) = &tts[i]
            && p.as_char() == '!'
            && i + 1 < tts.len()
            && let TokenTree::Group(g) = &tts[i + 1]
            && let Some(start) = rt_macro_path_start(&tts[..i])
        {
            out.push(start..g.span().byte_range().end);
            i += 2;
            continue;
        }
        if let TokenTree::Group(g) = &tts[i]
            && g.delimiter() != Delimiter::None
        {
            walk_tokens(g.stream(), out);
        }
        i += 1;
    }
}

/// For tokens ending right before a `!`: if they end with the path
/// `rt::name` where `name` is a segment macro, return the byte offset
/// where the path starts.
fn rt_macro_path_start(tts: &[TokenTree]) -> Option<usize> {
    let n = tts.len();
    if n < 4 {
        return None;
    }
    let TokenTree::Ident(name) = &tts[n - 1] else {
        return None;
    };
    let (TokenTree::Punct(c2), TokenTree::Punct(c1), TokenTree::Ident(prefix)) =
        (&tts[n - 2], &tts[n - 3], &tts[n - 4])
    else {
        return None;
    };
    if c1.as_char() == ':'
        && c2.as_char() == ':'
        && prefix == "rt"
        && SEGMENT_MACROS.contains(&name.to_string().as_str())
    {
        return Some(tts[n - 4].span().byte_range().start);
    }
    None
}

/// Split a token slice at top-level commas.
fn split_comma(tts: &[TokenTree]) -> Vec<Vec<TokenTree>> {
    let mut parts = Vec::new();
    let mut cur = Vec::new();
    for tt in tts {
        if let TokenTree::Punct(p) = tt
            && p.as_char() == ','
        {
            parts.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(tt.clone());
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Format one `rt::call!` / `mcall!` / `args!` invocation, relative to
/// column 0. Returns `None` on any structural surprise (the caller
/// falls back to the original text).
fn fmt_macro_site(text: &str) -> Option<String> {
    let ts: TokenStream = text.parse().ok()?;
    let tts: Vec<TokenTree> = ts.into_iter().collect();
    // [Ident(rt), ':', ':', Ident(name), '!', Group]
    if tts.len() != 6 {
        return None;
    }
    let TokenTree::Ident(name) = &tts[3] else {
        return None;
    };
    let TokenTree::Group(group) = &tts[5] else {
        return None;
    };
    if group.delimiter() != Delimiter::Parenthesis {
        return None;
    }
    // rt::markup! is bracket-delimited: a capacity literal, then
    // comma-separated bracket groups (a vec with run-length entries) —
    // one item per line.
    if group.delimiter() == Delimiter::Bracket {
        let inner: Vec<TokenTree> = group.stream().into_iter().collect();
        // Leading capacity literal (before the first bracket group).
        let cap = inner
            .iter()
            .find(|t| matches!(t, TokenTree::Literal(_)))
            .map(|t| t.to_string())
            .unwrap_or_default();
        let segs: Vec<String> = inner
            .iter()
            .filter_map(|t| match t {
                TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => {
                    let tts: Vec<TokenTree> = g.stream().into_iter().collect();
                    Some(head_text(&tts))
                }
                _ => None,
            })
            .collect();
        let one_line = format!("rt::{name}![{cap}; {}]", segs.join(", "));
        if one_line.chars().count() <= WIDTH {
            return Some(one_line);
        }
        let mut out = format!("rt::{name}![{cap};");
        for seg in &segs {
            out.push('\n');
            out.push_str(&shift_all_lines(seg, 4));
        }
        out.push_str("\n]");
        return Some(out);
    }
    // Interior: receiver expressions, then ';', then segments.
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let semi = inner
        .iter()
        .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ';'))?;
    let recv = split_comma(&inner[..semi])
        .iter()
        .map(|p| head_text(p))
        .collect::<Vec<_>>()
        .join(", ");
    let segments = fmt_segments(&inner[semi + 1..])?;
    let one_line = format!("rt::{name}!({recv}; {})", segments.join(" "));
    if one_line.chars().count() <= WIDTH {
        return Some(one_line);
    }
    let mut out = format!("rt::{name}!({recv};");
    for seg in &segments {
        out.push('\n');
        out.push_str(&shift_all_lines(seg, 4));
    }
    out.push_str("\n)");
    Some(out)
}

/// Format the segment sequence: `p(span, { v })` / `n(span, vspan,
/// "name", { v })` / `s(span, { v })`, space-separated. Long value
/// blocks break into a block layout.
fn fmt_segments(tts: &[TokenTree]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < tts.len() {
        let TokenTree::Ident(kind) = &tts[i] else {
            return None;
        };
        let TokenTree::Group(args) = tts.get(i + 1)? else {
            return None;
        };
        if args.delimiter() != Delimiter::Parenthesis {
            return None;
        }
        let inner: Vec<TokenTree> = args.stream().into_iter().collect();
        // The value block is the last (brace) argument.
        let block_at = inner
            .iter()
            .rposition(|t| matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace))?;
        let head = split_comma(&inner[..block_at])
            .iter()
            .map(|p| head_text(p))
            .collect::<Vec<_>>()
            .join(", ");
        let block = fmt_value_block(&inner[block_at])?;
        out.push(format!("{kind}({head}, {block})"));
        i += 2;
    }
    Some(out)
}

/// `{ value }` — inline when the formatted value is short, block layout
/// when long. The value goes through `protect_format` (nested call
/// sites recurse). A value that is not a single expression (statement
/// sequences) is formatted as statements.
fn fmt_value_block(tt: &TokenTree) -> Option<String> {
    let TokenTree::Group(g) = tt else { return None };
    if g.delimiter() != Delimiter::Brace {
        return None;
    }
    let interior = g.stream();
    if interior.is_empty() {
        return Some("{}".to_string());
    }
    let text = interior.to_string();
    let is_expr = syn::Expr::parse.parse_str(text.trim()).is_ok();
    // Value blocks containing macro sites must go through the
    // placeholder flow (prettyplease would token-wrap the macro).
    let value = if is_expr {
        protect_format(&text, Mode::Expr, 0)
    } else {
        protect_format(&text, Mode::Stmts, 0)
    };
    let inline = format!("{{ {value} }}");
    if inline.chars().count() <= BLOCK_INLINE {
        return Some(inline);
    }
    Some(format!("{{\n{}\n}}", shift_all_lines(&value, 4)))
}

/// `rt::file_module!` layout: fixed scaffold lines, bodies through
/// `protect_format`, `file_fns` through the item walker.
fn fmt_file_module(mac: &syn::Macro, original: &str, offset: usize) -> Option<String> {
    let tts: Vec<TokenTree> = mac.tokens.clone().into_iter().collect();
    let mut i = 0usize;
    let mut path = None;
    let mut state_ty = None;
    let mut eval_body = None;
    let mut module_body = None;
    let mut file_fns = None;
    while i < tts.len() {
        let Some(TokenTree::Ident(ident)) = tts.get(i) else {
            return None;
        };
        match ident.to_string().as_str() {
            "path" => {
                path = match tts.get(i + 2)? {
                    TokenTree::Literal(l) => Some(l.to_string()),
                    _ => return None,
                };
                i += 4; // ident ':' literal ','
            }
            "state_ty" => {
                state_ty = match tts.get(i + 2)? {
                    TokenTree::Ident(id) => Some(id.to_string()),
                    _ => return None,
                };
                i += 4;
            }
            "eval_body" | "module_body" => {
                let args = match tts.get(i + 1)? {
                    TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                        idents_csv(&g.stream())
                    }
                    _ => return None,
                };
                let body = match tts.get(i + 2)? {
                    TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => g,
                    _ => return None,
                };
                let br = body.span().byte_range();
                let interior = &original[br.start + 1 - offset..br.end - 1 - offset];
                let formatted = protect_format(interior, Mode::Stmts, 8);
                let slot = format!("{ident}({args}) {{\n{formatted}\n    }}");
                if ident == "eval_body" {
                    eval_body = Some(slot);
                } else {
                    module_body = Some(slot);
                }
                i += 3;
            }
            "file_fns" => {
                let fns = match tts.get(i + 1)? {
                    TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => g,
                    _ => return None,
                };
                let fr = fns.span().byte_range();
                let text = &original[fr.start + 1 - offset..fr.end - 1 - offset];
                let formatted = if text.trim().is_empty() {
                    String::new()
                } else {
                    shift_all_lines(format_items(text).trim_end_matches('\n'), 8)
                };
                file_fns = Some(formatted);
                i += 2;
            }
            _ => return None,
        }
    }
    Some(format!(
        "rt::file_module! {{\n    path: {},\n    state_ty: {},\n    {}\n    {}\n    file_fns {{\n{}\n    }}\n}}",
        path?, state_ty?, eval_body?, module_body?, file_fns?
    ))
}

/// Comma-separated plain identifiers (macro parameter lists).
fn idents_csv(ts: &TokenStream) -> String {
    ts.clone()
        .into_iter()
        .filter(|tt| !matches!(tt, TokenTree::Punct(p) if p.as_char() == ','))
        .map(|tt| tt.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// -------------------------------------------------------------- utilities

/// Token-stream equality, whitespace-, comment-, and
/// trailing-comma-insensitive (prettyplease adds a trailing comma when
/// it breaks a list — insignificant in every Rust position formatted
/// here; anything else still has to match exactly).
fn tokens_equal(a: &str, b: &str) -> bool {
    match (a.parse::<TokenStream>(), b.parse::<TokenStream>()) {
        (Ok(a), Ok(b)) => ts_eq(&normalize_ts(a), &normalize_ts(b)),
        _ => false,
    }
}

/// Recursively drop insignificant trailing commas: at the end of every
/// group, and directly before a `>` (prettyplease adds both when it
/// breaks a list — call args, array elements, generic arguments). A `,`
/// before `>` cannot occur in expression position, so this cannot mask
/// a real difference.
///
/// Not normalized (fallback instead): broken match arms, which
/// prettyplease rewrites as block bodies without separator commas —
/// `module_by_path` registry tables therefore stay as emitted (one
/// line per arm, a table anyway).
fn normalize_ts(ts: TokenStream) -> TokenStream {
    let mut tts: Vec<TokenTree> = ts
        .into_iter()
        .map(|tt| match tt {
            TokenTree::Group(g) => {
                let mut g2 = proc_macro2::Group::new(g.delimiter(), normalize_ts(g.stream()));
                g2.set_span(g.span());
                TokenTree::Group(g2)
            }
            other => other,
        })
        .collect();
    if matches!(tts.last(), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
        tts.pop();
    }
    // `, >` pairs: trailing commas of broken generic argument lists.
    let mut i = 0;
    while i + 1 < tts.len() {
        if matches!(&tts[i], TokenTree::Punct(p) if p.as_char() == ',')
            && matches!(&tts[i + 1], TokenTree::Punct(q) if q.as_char() == '>')
        {
            tts.remove(i);
        } else {
            i += 1;
        }
    }
    TokenStream::from_iter(tts)
}

fn ts_eq(a: &TokenStream, b: &TokenStream) -> bool {
    let mut bi = b.clone().into_iter();
    for (k, ta) in a.clone().into_iter().enumerate() {
        let Some(tb) = bi.next() else { return false };
        if !tt_eq(ta.clone(), tb.clone()) {
            if std::env::var_os("TRUST_FMT_DEBUG").is_some() {
                let ctx = |ts: &TokenStream, k: usize| {
                    let v: Vec<String> = ts.clone().into_iter().map(|t| t.to_string()).collect();
                    let lo = k.saturating_sub(3);
                    v[lo..(k + 2).min(v.len())].join(" ")
                };
                eprintln!(
                    "ts_eq first mismatch at {k}:\n  A: {ta:?}\n  B: {tb:?}\n  A-ctx: {}\n  B-ctx: {}",
                    ctx(a, k),
                    ctx(b, k)
                );
            }
            return false;
        }
    }
    bi.next().is_none()
}

fn tt_eq(a: TokenTree, b: TokenTree) -> bool {
    match (a, b) {
        (TokenTree::Group(x), TokenTree::Group(y)) => {
            x.delimiter() == y.delimiter() && ts_eq(&x.stream(), &y.stream())
        }
        (TokenTree::Ident(x), TokenTree::Ident(y)) => x == y,
        // Spacing is not compared: it records adjacency in the text the
        // tokens were parsed from, so prettyplease's inserted trailing
        // commas flip their neighbours' spacing without any semantic
        // change. Gluing of multi-char operators (`::`, `->`) is
        // preserved by every producer here (prettyplease prints valid
        // Rust; `head_text` follows the original joints; DSL scaffolding
        // passes tokens through verbatim).
        (TokenTree::Punct(x), TokenTree::Punct(y)) => x.as_char() == y.as_char(),
        (TokenTree::Literal(x), TokenTree::Literal(y)) => x.to_string() == y.to_string(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_items_prettyprinted() {
        let src = "fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> { let mut scope = Scope::new(); Ok(Module::new(\"x\", scope)) }\n";
        let out = format_items(src);
        // Statements one per line; signature breaks only when too long.
        assert!(
            out.contains("fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {\n    let mut scope = Scope::new();\n    Ok(Module::new(\"x\", scope))\n}\n"),
            "{out}"
        );
    }

    #[test]
    fn header_preserved() {
        let src = "// banner\n// - degraded thing\n\nfn a() {}\n";
        let out = format_generated(src);
        assert!(
            out.starts_with("// banner\n// - degraded thing\n\n"),
            "{out}"
        );
        assert!(out.contains("fn a() {}"), "{out}");
    }

    #[test]
    fn parse_failure_verbatim() {
        let src = "not rust !!\n";
        assert_eq!(format_generated(src), src);
    }

    #[test]
    fn span_table_verbatim() {
        let src = "rt::span_table!(&[(\"a.typ\", 7u64), (\"b.typ\", 9u64)]);\n";
        assert_eq!(format_items(src), src);
    }

    #[test]
    fn call_site_short_stays_one_line() {
        let src = "fn f() { let v = rt::call!(engine, context, sp(1), callee; p(sp(2), { Value::Int(0) }))?; }\n";
        let out = format_items(src);
        assert!(
            out.contains(
                "let v = rt::call!(engine, context, sp(1), callee; p(sp(2), { Value::Int(0) }))?;"
            ),
            "{out}"
        );
    }

    #[test]
    fn call_site_long_breaks_segments() {
        let mut segs = String::new();
        for i in 0..6 {
            segs.push_str(&format!("p(sp({}), {{ Value::Int({}) }}) ", 10 + i, i));
        }
        let src = format!(
            "fn f() {{ rt::call!(engine, context, sp(1), rt::callee(rt::global(engine, \"the-arguments\", file_span())?, sp(3))?; {segs}); }}\n"
        );
        let out = format_items(&src);
        // One segment per line, at the site's relative indent.
        assert!(out.contains("p(sp(10), { Value::Int(0) })\n"), "{out}");
        assert!(out.matches("\n        p(sp(").count() >= 5, "{out}");
        assert!(tokens_equal(&src, &out));
    }

    #[test]
    fn nested_macro_in_value_block() {
        let src = "fn f() { rt::call!(engine, context, sp(1), callee; p(sp(2), { rt::mcall!(engine, context, sp(3), target, \"field\", \"access\", sp(4), sp(5); p(sp(6), { Value::None })) })); }\n";
        let out = format_items(src);
        assert!(out.contains("rt::mcall!("), "{out}");
        assert!(tokens_equal(src, &out), "{out}");
    }

    #[test]
    fn file_module_layout() {
        let src = "rt::file_module! {\n    path: \"lib.typ\",\n    state_ty: __State,\n    eval_body(__st, engine, context) { let _ = (&*engine, context); rt::fill(&__st.BODY_a, Value::None)?; Ok(()) }\n    module_body(__st, engine, context) { let _ = __st; let mut scope = Scope::new(); Ok(Module::new(\"lib\", scope)) }\n    file_fns { fn helper() -> i32 { 1 + 2 } }\n}\n";
        let out = format_items(src);
        assert!(
            out.contains(
                "rt::file_module! {\n    path: \"lib.typ\",\n    state_ty: __State,\n    eval_body(__st, engine, context) {\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("fn helper() -> i32 {\n            1 + 2\n        }"),
            "{out}"
        );
        assert!(tokens_equal(src, &out), "{out}");
    }

    #[test]
    fn macro_inside_vec_items() {
        let src = "fn f() { let items = vec![(rt::space()).into_value(), (rt::call!(engine, context, sp(1), callee; p(sp(2), { Value::Int(0) }))?).into_value(), (rt::call!(engine, context, sp(3), callee2; p(sp(4), { Value::Int(1) }))?).into_value(), (rt::call!(engine, context, sp(5), callee3; p(sp(6), { Value::Int(2) }))?).into_value()]; g(items); }\n";
        let out = format_items(src);
        // Long enough to force a break: one item per line, the call
        // sites spliced at their columns.
        assert!(out.contains("vec![\n"), "{out}");
        assert!(tokens_equal(src, &out), "{out}");
    }

    #[test]
    fn placeholders_never_leak() {
        assert!(!format_generated("fn a() {}\n").contains("__trust_fmt"));
    }
}

#[cfg(test)]
mod head_tests {
    use super::head_text;
    use proc_macro2::TokenStream;

    fn h(s: &str) -> String {
        let tts: Vec<_> = s.parse::<TokenStream>().unwrap().into_iter().collect();
        head_text(&tts)
    }

    #[test]
    fn spacing_rules() {
        assert_eq!(h("f(a, b)"), "f(a, b)");
        assert_eq!(
            h("rt::callee(rt::global(engine, \"x\")?, sp(81))?"),
            "rt::callee(rt::global(engine, \"x\")?, sp(81))?"
        );
        assert_eq!(h("&__st.V3_args"), "&__st.V3_args");
        assert_eq!(h("a.b . c?"), "a.b.c?");
    }
}
