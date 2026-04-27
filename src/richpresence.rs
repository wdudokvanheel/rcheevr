//! Rich presence parser and renderer.
//!
//! Parses `Format:` / `Lookup:` / `Display:` sections following rcheevos
//! grammar exactly. Each display line has an optional trigger condition
//! (full rcheevos achievement syntax) and a body of literal text plus
//! `@Name(value_expr)` macros where `value_expr` is a legacy value chain
//! (`_`-joined AddSource clauses, last is Measured, `*` multiplier, `*-N`
//! SubSource) or an explicit-flag condset (`A:.../M:...`).

use crate::condition::{Achievement, CompareOp, ConditionFlag, ConditionParseError, Condset};
use crate::evaluator::{
    base_read_size, bcd_decode, eval_achievement, is_float_size, size_mask, transform_memref_value,
    AchievementState, ConditionState, CondsetState,
};
use crate::memaddr::{MemSize, Operand, OperandKind};

// ---------------------------------------------------------------------------
// Format kind — matches rcheevos `RC_FORMAT_*`.
// ---------------------------------------------------------------------------

/// Output format applied to a numeric macro value.
///
/// Mirrors rcheevos `RC_FORMAT_*` constants. Numeric formats decide signed
/// vs unsigned interpretation of the underlying u32 accumulator at output time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
    /// Signed decimal with comma-separated thousands.
    Value,
    /// Frame count (60 fps) → `M:SS.cs` via centiseconds.
    Frames,
    /// `M:SS.cs` from centiseconds.
    Centisecs,
    /// `M:SS` (or `HhMM:SS`) from seconds.
    Seconds,
    /// `HhMM` from a seconds count divided by 60.
    SecondsAsMinutes,
    /// `HhMM` from a minutes count.
    Minutes,
    /// Signed decimal zero-padded to 6 digits.
    Score,
    Float1,
    Float2,
    Float3,
    Float4,
    Float5,
    Float6,
    Fixed1,
    Fixed2,
    Fixed3,
    /// Signed `Nx10` (commas).
    Tens,
    /// Signed `Nx100` (commas).
    Hundreds,
    /// Signed `Nx1000` (commas).
    Thousands,
    /// Unsigned decimal with commas.
    Unsigned,
    /// Unsigned decimal, no commas.
    Unformatted,
    /// `@ASCIIChar(...)`: emit a printable ASCII char from the value.
    AsciiChar,
    /// `@UnicodeChar(...)`: emit a UTF-8 codepoint from the value.
    UnicodeChar,
    /// `@Name(...)` referenced a Lookup table — handled separately by the renderer.
    Lookup,
}

/// Parse a `FormatType=...` value (case-sensitive, matches rcheevos
/// `rc_parse_format`).
fn parse_format_kind(s: &str) -> FormatKind {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return FormatKind::Value;
    }
    match bytes[0] {
        b'F' => {
            if s == "FRAMES" {
                return FormatKind::Frames;
            }
            if s.starts_with("FLOAT") && bytes.len() == 6 {
                let n = bytes[5];
                if (b'1'..=b'6').contains(&n) {
                    return match n {
                        b'1' => FormatKind::Float1,
                        b'2' => FormatKind::Float2,
                        b'3' => FormatKind::Float3,
                        b'4' => FormatKind::Float4,
                        b'5' => FormatKind::Float5,
                        b'6' => FormatKind::Float6,
                        _ => FormatKind::Value,
                    };
                }
            }
            if s.starts_with("FIXED") && bytes.len() == 6 {
                let n = bytes[5];
                if (b'1'..=b'3').contains(&n) {
                    return match n {
                        b'1' => FormatKind::Fixed1,
                        b'2' => FormatKind::Fixed2,
                        b'3' => FormatKind::Fixed3,
                        _ => FormatKind::Value,
                    };
                }
            }
        }
        b'T' => match s {
            "TIME" => return FormatKind::Frames,
            "TIMESECS" => return FormatKind::Seconds,
            "THOUSANDS" => return FormatKind::Thousands,
            "TENS" => return FormatKind::Tens,
            _ => {}
        },
        b'S' => match s {
            "SECS" => return FormatKind::Seconds,
            "SCORE" => return FormatKind::Score,
            "SECS_AS_MINS" => return FormatKind::SecondsAsMinutes,
            _ => {}
        },
        b'M' => match s {
            "MILLISECS" => return FormatKind::Centisecs,
            "MINUTES" => return FormatKind::Minutes,
            _ => {}
        },
        b'P' => {
            if s == "POINTS" {
                return FormatKind::Score;
            }
        }
        b'V' => {
            if s == "VALUE" {
                return FormatKind::Value;
            }
        }
        b'U' => {
            if s == "UNSIGNED" {
                return FormatKind::Unsigned;
            }
        }
        b'O' => {
            if s == "OTHER" {
                return FormatKind::Score;
            }
        }
        b'H' => {
            if s == "HUNDREDS" {
                return FormatKind::Hundreds;
            }
        }
        _ => {}
    }
    FormatKind::Value
}

// ---------------------------------------------------------------------------
// Built-in macros (no `Format:` block needed)
// ---------------------------------------------------------------------------

fn builtin_macro(name: &str) -> Option<FormatKind> {
    Some(match name {
        "Number" => FormatKind::Value,
        "Score" => FormatKind::Score,
        "Centiseconds" => FormatKind::Centisecs,
        "Seconds" => FormatKind::Seconds,
        "Minutes" => FormatKind::Minutes,
        "SecondsAsMinutes" => FormatKind::SecondsAsMinutes,
        "ASCIIChar" => FormatKind::AsciiChar,
        "UnicodeChar" => FormatKind::UnicodeChar,
        "Float1" => FormatKind::Float1,
        "Float2" => FormatKind::Float2,
        "Float3" => FormatKind::Float3,
        "Float4" => FormatKind::Float4,
        "Float5" => FormatKind::Float5,
        "Float6" => FormatKind::Float6,
        "Fixed1" => FormatKind::Fixed1,
        "Fixed2" => FormatKind::Fixed2,
        "Fixed3" => FormatKind::Fixed3,
        "Unsigned" => FormatKind::Unsigned,
        "Unformatted" => FormatKind::Unformatted,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Lookup tables
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LookupKey {
    Single(u32),
    Range(u32, u32),
}

#[derive(Debug, Clone, Default)]
pub struct LookupTable {
    pub entries: Vec<(LookupKey, String)>,
    pub fallback: Option<String>,
}

impl LookupTable {
    fn lookup(&self, value: u32) -> Option<&str> {
        for (key, label) in &self.entries {
            let hit = match key {
                LookupKey::Single(k) => *k == value,
                LookupKey::Range(lo, hi) => value >= *lo && value <= *hi,
            };
            if hit {
                return Some(label.as_str());
            }
        }
        self.fallback.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Macro definitions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum MacroKind {
    /// User-defined Lookup table mapped via `@Name(value)`.
    Lookup(LookupTable),
    /// User-defined `Format:`/`FormatType=` numeric formatter.
    Format(FormatKind),
}

#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    pub kind: MacroKind,
}

// ---------------------------------------------------------------------------
// Display lines
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum DisplayPart {
    Literal(String),
    Macro {
        name: String,
        /// Value expression, parsed as a single-condset achievement.
        value: Achievement,
    },
}

#[derive(Debug, Clone)]
pub struct DisplayLine {
    /// Full trigger condition; `None` means unconditional fallback.
    pub condition: Option<Achievement>,
    pub body: Vec<DisplayPart>,
}

// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RichPresence {
    pub macros: Vec<MacroDef>,
    pub display: Vec<DisplayLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichPresenceParseError {
    pub line: usize,
    pub msg: &'static str,
}

impl std::fmt::Display for RichPresenceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rich presence parse error at line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for RichPresenceParseError {}

// ---------------------------------------------------------------------------
// Per-frame state
// ---------------------------------------------------------------------------

/// Mutable per-frame state for one rich presence script.
#[derive(Debug, Clone, Default)]
pub struct RichPresenceState {
    lines: Vec<DisplayLineState>,
}

#[derive(Debug, Clone)]
struct DisplayLineState {
    /// Trigger evaluation state. `None` for unconditional fallback lines.
    condition: Option<AchievementState>,
    /// One condset state per macro in this line's body. Literals get a
    /// default placeholder.
    parts: Vec<CondsetState>,
}

impl RichPresenceState {
    /// Allocate fresh state matching the script's shape.
    pub fn for_script(rp: &RichPresence) -> Self {
        let lines = rp
            .display
            .iter()
            .map(|line| {
                let condition = line
                    .condition
                    .as_ref()
                    .map(AchievementState::for_achievement);
                let parts = line
                    .body
                    .iter()
                    .map(|p| match p {
                        DisplayPart::Literal(_) => CondsetState::default(),
                        DisplayPart::Macro { value, .. } => CondsetState {
                            cond_states: vec![ConditionState::default(); value.core.conditions.len()],
                            ..Default::default()
                        },
                    })
                    .collect();
                DisplayLineState { condition, parts }
            })
            .collect();
        RichPresenceState { lines }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

impl RichPresence {
    /// Parse a rich presence script.
    pub fn parse(src: &str) -> Result<Self, RichPresenceParseError> {
        let mut rp = RichPresence::default();
        let mut state = ParserState::Top;

        // We scan line-by-line; lookups take ownership of subsequent lines
        // until a blank line or a new section header. Comments (`//`) and
        // blank lines outside sections are ignored.
        for (idx, raw) in src.lines().enumerate() {
            let lineno = idx + 1;
            let line = strip_inline_comment(raw);

            // Section headers reset the parser state.
            if let Some(name) = line.strip_prefix("Lookup:") {
                state = ParserState::Lookup(MacroDef {
                    name: name.trim().to_string(),
                    kind: MacroKind::Lookup(LookupTable::default()),
                });
                continue;
            }
            if let Some(name) = line.strip_prefix("Format:") {
                state.flush(&mut rp);
                state = ParserState::FormatHeader(name.trim().to_string());
                continue;
            }
            if line.starts_with("Display:") {
                state.flush(&mut rp);
                state = ParserState::Display;
                continue;
            }

            match &mut state {
                ParserState::Top | ParserState::FormatDone => {
                    // Ignore stray content between sections.
                }
                ParserState::Lookup(def) => {
                    if line.is_empty() {
                        // End of lookup block.
                        rp.macros.push(std::mem::replace(
                            def,
                            MacroDef {
                                name: String::new(),
                                kind: MacroKind::Lookup(LookupTable::default()),
                            },
                        ));
                        state = ParserState::Top;
                        continue;
                    }
                    if let MacroKind::Lookup(table) = &mut def.kind {
                        parse_lookup_entry(line, lineno, table)?;
                    }
                }
                ParserState::FormatHeader(name) => {
                    if line.is_empty() {
                        return Err(RichPresenceParseError {
                            line: lineno,
                            msg: "Format block missing FormatType= line",
                        });
                    }
                    let format_value = line
                        .strip_prefix("FormatType=")
                        .or_else(|| line.strip_prefix("FormatType:"));
                    if let Some(ty_str) = format_value {
                        let kind = parse_format_kind(ty_str);
                        rp.macros.push(MacroDef {
                            name: std::mem::take(name),
                            kind: MacroKind::Format(kind),
                        });
                        state = ParserState::FormatDone;
                    }
                    // ignore other lines inside the format block (rcheevos
                    // also tolerates extra content here).
                }
                ParserState::Display => {
                    if line.is_empty() {
                        continue;
                    }
                    let display_line = parse_display_line(line, lineno)?;
                    rp.display.push(display_line);
                }
            }
        }

        state.flush(&mut rp);

        Ok(rp)
    }

    /// Convenience: build fresh state and render against a byte-only peek.
    /// Useful for tests; production code should call [`Self::render_with`]
    /// with a size-aware peek and persistent [`RichPresenceState`].
    pub fn render<F: FnMut(u32) -> u32>(&self, mut peek: F) -> String {
        let mut state = RichPresenceState::for_script(self);
        self.render_with(&mut state, |addr, _size| peek(addr))
    }

    /// Render the active display line using a size-aware peek and persistent state.
    pub fn render_with<F: FnMut(u32, MemSize) -> u32>(
        &self,
        state: &mut RichPresenceState,
        mut peek: F,
    ) -> String {
        if self.display.is_empty() {
            return String::new();
        }
        if state.lines.len() != self.display.len() {
            *state = RichPresenceState::for_script(self);
        }

        // Walk display lines in order. The last line is the unconditional
        // fallback; earlier lines have triggers that gate them.
        let last = self.display.len() - 1;
        let mut chosen: Option<usize> = None;
        for (i, line) in self.display.iter().enumerate() {
            // Always evaluate every conditional trigger so hit-count state
            // stays current even when an earlier line wins.
            if i == last && line.condition.is_none() {
                continue;
            }
            if let (Some(cond), Some(cstate)) =
                (&line.condition, state.lines[i].condition.as_mut())
            {
                let fired = eval_achievement_level(cond, cstate, &mut peek);
                if fired && chosen.is_none() {
                    chosen = Some(i);
                }
            }
        }
        let pick = chosen.unwrap_or(last);
        self.render_line(pick, state, &mut peek)
    }

    fn render_line<F: FnMut(u32, MemSize) -> u32>(
        &self,
        idx: usize,
        state: &mut RichPresenceState,
        peek: &mut F,
    ) -> String {
        let mut out = String::new();
        let line = &self.display[idx];
        let line_state = &mut state.lines[idx];

        // Group consecutive AsciiChar / UnicodeChar parts so we can stop on a NUL.
        let parts = &line.body;
        let mut i = 0;
        while i < parts.len() {
            match &parts[i] {
                DisplayPart::Literal(s) => {
                    out.push_str(s);
                    i += 1;
                }
                DisplayPart::Macro { name, value } => {
                    let kind = self.resolve_macro_kind(name);
                    match kind {
                        Some(MacroResolution::Format(FormatKind::AsciiChar))
                        | Some(MacroResolution::Format(FormatKind::UnicodeChar)) => {
                            // Consume run of same-kind char macros, stop at NUL.
                            let target = match kind {
                                Some(MacroResolution::Format(k)) => k,
                                _ => unreachable!(),
                            };
                            while i < parts.len() {
                                let DisplayPart::Macro {
                                    name: pname,
                                    value: pval,
                                } = &parts[i]
                                else {
                                    break;
                                };
                                let pkind = self.resolve_macro_kind(pname);
                                if !matches!(pkind, Some(MacroResolution::Format(k)) if k == target)
                                {
                                    break;
                                }
                                let cs = &mut line_state.parts[i];
                                let v = eval_value_expression(&pval.core, cs, peek);
                                if v == 0 {
                                    // null terminator — skip the rest of the run
                                    while i < parts.len() {
                                        if let DisplayPart::Macro { name: nname, .. } = &parts[i] {
                                            let nkind = self.resolve_macro_kind(nname);
                                            if matches!(nkind, Some(MacroResolution::Format(k)) if k == target) {
                                                i += 1;
                                                continue;
                                            }
                                        }
                                        break;
                                    }
                                    break;
                                }
                                if target == FormatKind::AsciiChar {
                                    let c = if (32..127).contains(&v) {
                                        v as u8 as char
                                    } else {
                                        '?'
                                    };
                                    out.push(c);
                                } else {
                                    let cp = if (32..=0xFFFF).contains(&v) {
                                        if (0xD800..0xE000).contains(&v) {
                                            0xFFFD
                                        } else {
                                            v
                                        }
                                    } else {
                                        0xFFFD
                                    };
                                    if let Some(c) = char::from_u32(cp) {
                                        out.push(c);
                                    }
                                }
                                i += 1;
                            }
                        }
                        Some(MacroResolution::Lookup(table)) => {
                            let cs = &mut line_state.parts[i];
                            let v = eval_value_expression(&value.core, cs, peek);
                            if let Some(label) = table.lookup(v) {
                                out.push_str(label);
                            } else if let Some(fb) = table.fallback.as_deref() {
                                out.push_str(fb);
                            }
                            i += 1;
                        }
                        Some(MacroResolution::Format(fmt)) => {
                            let cs = &mut line_state.parts[i];
                            let v = eval_value_expression(&value.core, cs, peek);
                            let s = format_value(fmt, v);
                            out.push_str(&s);
                            i += 1;
                        }
                        None => {
                            // Unknown macro: emit `[Unknown macro]Name` like rcheevos.
                            out.push_str("[Unknown macro]");
                            out.push_str(name);
                            i += 1;
                        }
                    }
                }
            }
        }
        out
    }

    fn resolve_macro_kind(&self, name: &str) -> Option<MacroResolution<'_>> {
        for m in &self.macros {
            if m.name == name {
                return Some(match &m.kind {
                    MacroKind::Lookup(t) => MacroResolution::Lookup(t),
                    MacroKind::Format(k) => MacroResolution::Format(*k),
                });
            }
        }
        builtin_macro(name).map(MacroResolution::Format)
    }
}

enum MacroResolution<'a> {
    Lookup(&'a LookupTable),
    Format(FormatKind),
}

// ---------------------------------------------------------------------------
// Parser internals
// ---------------------------------------------------------------------------

enum ParserState {
    Top,
    Lookup(MacroDef),
    FormatHeader(String),
    FormatDone,
    Display,
}

impl ParserState {
    fn flush(&mut self, rp: &mut RichPresence) {
        if let ParserState::Lookup(def) = std::mem::replace(self, ParserState::Top) {
            rp.macros.push(def);
        }
    }
}

/// Strip a trailing `// ...` comment from a line and trim trailing whitespace.
fn strip_inline_comment(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut end = bytes.len();
    let mut i = 0;
    while i + 1 < end {
        if bytes[i] == b'/'
            && bytes[i + 1] == b'/'
            && (i == 0 || bytes[i - 1] != b'\\')
        {
            end = i;
            break;
        }
        i += 1;
    }
    let mut s = &raw[..end];
    while let Some(stripped) = s.strip_suffix(' ').or_else(|| s.strip_suffix('\t')).or_else(|| s.strip_suffix('\r')) {
        s = stripped;
    }
    s
}

fn parse_lookup_entry(
    line: &str,
    lineno: usize,
    table: &mut LookupTable,
) -> Result<(), RichPresenceParseError> {
    let eq = line.find('=').ok_or(RichPresenceParseError {
        line: lineno,
        msg: "Lookup entry missing '='",
    })?;
    let key_str = &line[..eq];
    let label = line[eq + 1..].to_string();

    if key_str == "*" {
        table.fallback = Some(label);
        return Ok(());
    }

    // Comma-separated key list: each item is either `N`, `0xN`, or `N-M`.
    for part in key_str.split(',') {
        let key = parse_lookup_key(part).ok_or(RichPresenceParseError {
            line: lineno,
            msg: "invalid Lookup key",
        })?;
        table.entries.push((key, label.clone()));
    }
    Ok(())
}

fn parse_lookup_key(s: &str) -> Option<LookupKey> {
    let s = s.trim();
    if let Some((lo_str, hi_str)) = split_range(s) {
        let lo = parse_u32_key(lo_str)?;
        let hi = parse_u32_key(hi_str)?;
        return Some(LookupKey::Range(lo, hi));
    }
    parse_u32_key(s).map(LookupKey::Single)
}

fn split_range(s: &str) -> Option<(&str, &str)> {
    // Find `-` not at position 0; otherwise the whole token is just a value.
    let mut iter = s.char_indices();
    iter.next()?; // skip first char so a leading `-` (negative key) isn't a range
    for (i, c) in iter {
        if c == '-' {
            return Some((&s[..i], &s[i + 1..]));
        }
    }
    None
}

fn parse_u32_key(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u32>().ok()
    }
}

fn parse_display_line(line: &str, lineno: usize) -> Result<DisplayLine, RichPresenceParseError> {
    let (condition, body_str) = if let Some(rest) = line.strip_prefix('?') {
        let end = rest.find('?').ok_or(RichPresenceParseError {
            line: lineno,
            msg: "unterminated condition in display line",
        })?;
        let cond_str = &rest[..end];
        let body = &rest[end + 1..];
        let ach = Achievement::parse(cond_str).map_err(|e| condition_error(e, lineno))?;
        (Some(ach), body)
    } else {
        (None, line)
    };
    let body = parse_display_body(body_str, lineno)?;
    Ok(DisplayLine { condition, body })
}

fn condition_error(_e: ConditionParseError, lineno: usize) -> RichPresenceParseError {
    RichPresenceParseError {
        line: lineno,
        msg: "invalid condition expression in display line",
    }
}

fn parse_display_body(body: &str, lineno: usize) -> Result<Vec<DisplayPart>, RichPresenceParseError> {
    let mut parts = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut buf = String::new();
    while i < bytes.len() {
        // Look for `@` (not escaped).
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1] != b'\\') {
            if !buf.is_empty() {
                parts.push(DisplayPart::Literal(unescape(&buf)));
                buf.clear();
            }
            // Consume `@Name(value)`.
            i += 1;
            let name_start = i;
            while i < bytes.len() && bytes[i] != b'(' {
                i += 1;
            }
            if i >= bytes.len() {
                // No `(`: treat as literal text.
                buf.push('@');
                buf.push_str(&body[name_start..]);
                break;
            }
            let name = body[name_start..i].to_string();
            i += 1; // skip `(`
            let val_start = i;
            while i < bytes.len() && bytes[i] != b')' {
                i += 1;
            }
            if i >= bytes.len() {
                // Unterminated macro — rcheevos dumps the unparsed remainder
                // as literal text.
                buf.push('@');
                buf.push_str(&body[name_start..]);
                break;
            }
            let val_str = &body[val_start..i];
            i += 1; // skip `)`
            let value = parse_value_expression(val_str)
                .map_err(|_| RichPresenceParseError {
                    line: lineno,
                    msg: "invalid value expression in macro",
                })?;
            parts.push(DisplayPart::Macro { name, value });
            continue;
        }
        buf.push(bytes[i] as char);
        i += 1;
    }
    if !buf.is_empty() {
        parts.push(DisplayPart::Literal(unescape(&buf)));
    }
    Ok(parts)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a value expression into an `Achievement` (only `core` is used).
///
/// Two forms:
/// - Explicit: starts with `<flag-letter>:` (e.g. `M:`, `A:`) → parsed via
///   the standard achievement parser.
/// - Legacy: clauses joined by `_`. Each clause is implicitly AddSource
///   except the last, which becomes Measured. `*N` is a multiplier; `*-N`
///   converts the clause to SubSource (and the `-` is stripped from the
///   multiplier). If the last clause was `*-N` it stays SubSource and a
///   trailing `M:0` is appended so the chain ends in Measured.
fn parse_value_expression(s: &str) -> Result<Achievement, ConditionParseError> {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        // Explicit form. Parse with the achievement grammar (S alts not
        // supported here, but we at least take the core).
        return Achievement::parse(s);
    }

    // Legacy form: split on `_`.
    let clauses: Vec<&str> = s.split('_').collect();
    let n = clauses.len();
    let mut combined = String::new();
    let mut last_was_sub_source = false;
    for (i, raw) in clauses.iter().enumerate() {
        let is_last = i == n - 1;
        let mut flag_char = if is_last { 'M' } else { 'A' };
        let clause = if let Some(star) = raw.find('*') {
            let after = &raw[star + 1..];
            if let Some(stripped) = after.strip_prefix('-') {
                // `*-N` flips the contribution to SubSource regardless of
                // position; the `-` sign is consumed.
                flag_char = 'B';
                format!("{}*{}", &raw[..star], stripped)
            } else if let Some(stripped) = after.strip_prefix('+') {
                format!("{}*{}", &raw[..star], stripped)
            } else {
                (*raw).to_string()
            }
        } else {
            (*raw).to_string()
        };
        if !combined.is_empty() {
            combined.push('_');
        }
        combined.push_str(&format!("{flag_char}:{clause}"));
        if is_last && flag_char == 'B' {
            last_was_sub_source = true;
        }
    }
    if last_was_sub_source {
        combined.push_str("_M:0");
    }
    Achievement::parse(&combined)
}

// ---------------------------------------------------------------------------
// Evaluation: trigger + value expressions
// ---------------------------------------------------------------------------

/// Evaluate a display-line trigger as a level-triggered condition: do not
/// latch state.triggered between calls.
fn eval_achievement_level<F: FnMut(u32, MemSize) -> u32>(
    ach: &Achievement,
    state: &mut AchievementState,
    peek: &mut F,
) -> bool {
    state.triggered = false;
    state.waiting = false;
    state.waiting_for_trigger_flag = false;
    eval_achievement(ach, state, peek)
}

/// Evaluate a value expression (a Condset interpreted as a left-to-right
/// accumulator chain) against the current memory state.
///
/// Operates with u32 modular arithmetic to match rcheevos value semantics.
/// Negative immediates (e.g. `v-1` parsed as 0xFFFFFFFF) participate via
/// wrapping addition so `read + (-1) = read - 1` mod 2^32.
fn eval_value_expression<F: FnMut(u32, MemSize) -> u32>(
    cond: &Condset,
    state: &mut CondsetState,
    peek: &mut F,
) -> u32 {
    if cond.conditions.is_empty() {
        return 0;
    }
    // Lazily extend per-condition state when the script changes layout
    // (mainly for tests that reuse a default state).
    if state.cond_states.len() < cond.conditions.len() {
        state
            .cond_states
            .resize(cond.conditions.len(), ConditionState::default());
    }

    // Phase 1: pause-if pre-scan.
    let mut paused = false;
    for (i, c) in cond.conditions.iter().enumerate() {
        if c.flag != ConditionFlag::PauseIf {
            continue;
        }
        let cs = &mut state.cond_states[i];
        let lhs = read_lhs(&c.lhs, cs, 0, 0, peek);
        let rhs = read_rhs(&c.rhs, 0, peek);
        if compare_u32(lhs, c.op, rhs) {
            paused = true;
        }
    }
    if paused {
        state.is_paused = true;
        return state.last_val_result as u32;
    }
    state.is_paused = false;

    // Phase 2: main pass.
    let mut accum: u32 = 0;
    let mut indirect: u32 = 0;
    let mut and_next = true;
    let mut or_next = false;
    let mut add_hits: i32 = 0;
    let mut measured_from_hits = false;
    let mut can_measure = true;
    let mut remember: u32 = 0;

    for (i, c) in cond.conditions.iter().enumerate() {
        let cs = &mut state.cond_states[i];
        let prev_lhs = update_delta(cs, current_value(&c.lhs, indirect, peek));
        let lhs = resolve_lhs_with_remember(&c.lhs, cs, prev_lhs, indirect, remember, peek);
        let rhs = read_rhs(&c.rhs, indirect, peek);

        match c.flag {
            ConditionFlag::AddSource => {
                accum = accum.wrapping_add(apply_modifier(lhs, c.op, rhs));
                indirect = 0;
            }
            ConditionFlag::SubSource => {
                accum = accum.wrapping_sub(apply_modifier(lhs, c.op, rhs));
                indirect = 0;
            }
            ConditionFlag::AddAddress => {
                indirect = indirect.wrapping_add(lhs);
            }
            ConditionFlag::Remember => {
                remember = accum.wrapping_add(lhs);
                accum = 0;
                indirect = 0;
            }
            ConditionFlag::AddHits => {
                let cmp = gate(compare_u32(lhs, c.op, rhs), and_next, or_next);
                if cmp {
                    cs.hit_count += 1;
                }
                add_hits += cs.hit_count as i32;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::SubHits => {
                let cmp = gate(compare_u32(lhs, c.op, rhs), and_next, or_next);
                if cmp {
                    cs.hit_count += 1;
                }
                add_hits -= cs.hit_count as i32;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::AndNext => {
                and_next = and_next && compare_u32(lhs, c.op, rhs);
            }
            ConditionFlag::OrNext => {
                or_next = or_next || compare_u32(lhs, c.op, rhs);
            }
            ConditionFlag::MeasuredIf => {
                let cmp = gate(compare_u32(lhs, c.op, rhs), and_next, or_next);
                can_measure = can_measure && cmp;
                and_next = true;
                or_next = false;
            }
            ConditionFlag::Measured => {
                if is_comparison(c.op) {
                    let cmp = gate(compare_u32(lhs, c.op, rhs), and_next, or_next);
                    if cmp {
                        cs.hit_count += 1;
                    }
                    let total = (cs.hit_count as i32 + add_hits).max(0) as u32;
                    add_hits = 0;
                    and_next = true;
                    or_next = false;
                    measured_from_hits = true;
                    accum = total;
                    indirect = 0;
                } else {
                    accum = accum.wrapping_add(apply_modifier(lhs, c.op, rhs));
                    indirect = 0;
                }
            }
            ConditionFlag::Standard => {
                accum = accum.wrapping_add(apply_modifier(lhs, c.op, rhs));
                indirect = 0;
                and_next = true;
                or_next = false;
            }
            // Triggers / resets / pause / reset-next aren't meaningful in a
            // value expression. Ignore.
            _ => {}
        }
    }

    let mut result = accum;
    if !can_measure {
        result = 0;
    }
    if measured_from_hits && !can_measure {
        result = 0;
    }
    state.last_val_result = result as i64;
    result
}

// ---------------------------------------------------------------------------
// Comparison / modifier helpers
// ---------------------------------------------------------------------------

fn gate(raw: bool, and_next: bool, or_next: bool) -> bool {
    if or_next {
        (raw || or_next) && and_next
    } else {
        raw && and_next
    }
}

fn is_comparison(op: CompareOp) -> bool {
    matches!(
        op,
        CompareOp::Equal
            | CompareOp::NotEqual
            | CompareOp::Less
            | CompareOp::LessOrEqual
            | CompareOp::Greater
            | CompareOp::GreaterOrEqual
    )
}

fn compare_u32(lhs: u32, op: CompareOp, rhs: u32) -> bool {
    match op {
        CompareOp::Equal => lhs == rhs,
        CompareOp::NotEqual => lhs != rhs,
        CompareOp::Less => lhs < rhs,
        CompareOp::LessOrEqual => lhs <= rhs,
        CompareOp::Greater => lhs > rhs,
        CompareOp::GreaterOrEqual => lhs >= rhs,
        _ => false,
    }
}

fn apply_modifier(lhs: u32, op: CompareOp, rhs: u32) -> u32 {
    match op {
        CompareOp::Multiply => lhs.wrapping_mul(rhs),
        CompareOp::Divide => {
            if rhs == 0 {
                0
            } else {
                lhs / rhs
            }
        }
        CompareOp::BitwiseAnd => lhs & rhs,
        CompareOp::BitwiseXor => lhs ^ rhs,
        CompareOp::Modulo => {
            if rhs == 0 {
                0
            } else {
                lhs % rhs
            }
        }
        _ => lhs,
    }
}

fn current_value<F: FnMut(u32, MemSize) -> u32>(op: &Operand, indirect: u32, peek: &mut F) -> u32 {
    match op {
        Operand::Memory { size, address, .. } => {
            let addr = address.wrapping_add(indirect);
            let raw = peek(addr, base_read_size(*size));
            transform_memref_value(raw, *size)
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => f.to_bits(),
    }
}

fn update_delta(cs: &mut ConditionState, current: u32) -> u32 {
    if cs.had_sample {
        let prev = cs.last_value;
        if current != cs.last_value {
            cs.prior_value = cs.last_value;
        }
        cs.last_value = current;
        prev
    } else {
        cs.last_value = current;
        cs.prior_value = current;
        cs.had_sample = true;
        0
    }
}

fn read_lhs<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    prev: u32,
    indirect: u32,
    peek: &mut F,
) -> u32 {
    resolve_lhs_with_remember(op, cs, prev, indirect, 0, peek)
}

fn resolve_lhs_with_remember<F: FnMut(u32, MemSize) -> u32>(
    op: &Operand,
    cs: &ConditionState,
    prev: u32,
    indirect: u32,
    remember: u32,
    peek: &mut F,
) -> u32 {
    match op {
        Operand::Memory { kind, size, address } => {
            let addr = address.wrapping_add(indirect);
            match kind {
                OperandKind::Address => {
                    let raw = peek(addr, base_read_size(*size));
                    transform_memref_value(raw, *size)
                }
                OperandKind::Delta => prev,
                OperandKind::Prior => cs.prior_value,
                OperandKind::Bcd => {
                    let raw = peek(addr, base_read_size(*size));
                    let v = transform_memref_value(raw, *size);
                    if is_float_size(*size) {
                        v
                    } else {
                        bcd_decode(v)
                    }
                }
                OperandKind::Inverted => {
                    let raw = peek(addr, base_read_size(*size));
                    let v = transform_memref_value(raw, *size);
                    if is_float_size(*size) {
                        v
                    } else {
                        !v & size_mask(*size)
                    }
                }
                OperandKind::Recall => remember,
            }
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => f.to_bits(),
    }
}

fn read_rhs<F: FnMut(u32, MemSize) -> u32>(op: &Operand, indirect: u32, peek: &mut F) -> u32 {
    match op {
        Operand::Memory { size, address, .. } => {
            let addr = address.wrapping_add(indirect);
            let raw = peek(addr, base_read_size(*size));
            transform_memref_value(raw, *size)
        }
        Operand::ConstInt(v) => *v,
        Operand::ConstFloat(f) => f.to_bits(),
    }
}

// ---------------------------------------------------------------------------
// Numeric formatting (matches rcheevos `rc_format_typed_value`)
// ---------------------------------------------------------------------------

/// Format a u32 accumulator value according to the given format kind.
/// Lookup formatting is handled separately by the renderer.
pub fn format_value(kind: FormatKind, v: u32) -> String {
    match kind {
        FormatKind::Lookup => v.to_string(),
        FormatKind::Value => with_commas(format!("{}", v as i32)),
        FormatKind::Frames => {
            let cs = (v as u64) * 10 / 6;
            format_centiseconds(cs as u32)
        }
        FormatKind::Centisecs => format_centiseconds(v),
        FormatKind::Seconds => format_seconds(v),
        FormatKind::SecondsAsMinutes => format_minutes(v / 60),
        FormatKind::Minutes => format_minutes(v),
        FormatKind::Score => format!("{:06}", v as i32),
        FormatKind::Float1 => format!("{:.1}", f32::from_bits(v)),
        FormatKind::Float2 => format!("{:.2}", f32::from_bits(v)),
        FormatKind::Float3 => format!("{:.3}", f32::from_bits(v)),
        FormatKind::Float4 => format!("{:.4}", f32::from_bits(v)),
        FormatKind::Float5 => format!("{:.5}", f32::from_bits(v)),
        FormatKind::Float6 => format!("{:.6}", f32::from_bits(v)),
        FormatKind::Fixed1 => format_fixed(v as i32, 10, 1),
        FormatKind::Fixed2 => format_fixed(v as i32, 100, 2),
        FormatKind::Fixed3 => format_fixed(v as i32, 1000, 3),
        FormatKind::Tens => format_padded_with_commas(v as i32, "0"),
        FormatKind::Hundreds => format_padded_with_commas(v as i32, "00"),
        FormatKind::Thousands => format_padded_with_commas(v as i32, "000"),
        FormatKind::Unsigned => with_commas(format!("{v}")),
        FormatKind::Unformatted => format!("{v}"),
        FormatKind::AsciiChar | FormatKind::UnicodeChar => String::new(),
    }
}

fn format_minutes(minutes: u32) -> String {
    let h = minutes / 60;
    let m = minutes % 60;
    format!("{h}h{m:02}")
}

fn format_seconds(seconds: u32) -> String {
    let m = seconds / 60;
    let s = seconds % 60;
    if m < 60 {
        format!("{m}:{s:02}")
    } else {
        let h = m / 60;
        let m = m % 60;
        format!("{h}h{m:02}:{s:02}")
    }
}

fn format_centiseconds(cs: u32) -> String {
    let s = cs / 100;
    let cs = cs % 100;
    let head = format_seconds(s);
    format!("{head}.{cs:02}")
}

fn format_fixed(value: i32, factor: i32, width: usize) -> String {
    if value >= 0 {
        format!("{}.{:0width$}", value / factor, value % factor, width = width)
    } else {
        let pos = (-value) as i64;
        let factor = factor as i64;
        format!(
            "{}.{:0width$}",
            value as i64 / factor,
            pos % factor,
            width = width
        )
    }
}

fn format_padded_with_commas(value: i32, suffix: &str) -> String {
    if value == 0 {
        return "0".to_string();
    }
    with_commas(format!("{value}{suffix}"))
}

fn with_commas(s: String) -> String {
    // Insert commas into the leading numeric run (after an optional `-`).
    let bytes = s.as_bytes();
    let start = if bytes.first() == Some(&b'-') { 1 } else { 0 };
    let mut digit_end = start;
    while digit_end < bytes.len() && bytes[digit_end].is_ascii_digit() {
        digit_end += 1;
    }
    if digit_end - start <= 3 {
        return s;
    }
    let mut out = String::with_capacity(s.len() + (digit_end - start) / 3);
    if start == 1 {
        out.push('-');
    }
    let digits = &s[start..digit_end];
    let len = digits.len();
    let first_group = len % 3;
    if first_group != 0 {
        out.push_str(&digits[..first_group]);
        if len > first_group {
            out.push(',');
        }
    }
    let mut i = first_group;
    while i + 3 <= len {
        out.push_str(&digits[i..i + 3]);
        i += 3;
        if i < len {
            out.push(',');
        }
    }
    out.push_str(&s[digit_end..]);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> RichPresence {
        RichPresence::parse(src).unwrap_or_else(|e| panic!("parse failed: {e:?}"))
    }

    #[test]
    fn parse_empty() {
        let rp = parse_ok("");
        assert!(rp.macros.is_empty());
        assert!(rp.display.is_empty());
    }

    #[test]
    fn parse_format_block() {
        let rp = parse_ok("Format:Score\nFormatType=SCORE\n");
        assert_eq!(rp.macros.len(), 1);
        assert_eq!(rp.macros[0].name, "Score");
        assert!(matches!(rp.macros[0].kind, MacroKind::Format(FormatKind::Score)));
    }

    #[test]
    fn parse_lookup_singles() {
        let rp = parse_ok("Lookup:Zone\n0=Tower\n1=Castle\n");
        let MacroKind::Lookup(t) = &rp.macros[0].kind else {
            panic!("expected lookup");
        };
        assert_eq!(t.entries.len(), 2);
    }

    #[test]
    fn parse_lookup_range_and_fallback() {
        let rp = parse_ok("Lookup:S\n1-3=Forest\n*=Unknown\n");
        let MacroKind::Lookup(t) = &rp.macros[0].kind else {
            panic!();
        };
        assert!(matches!(t.entries[0].0, LookupKey::Range(1, 3)));
        assert_eq!(t.fallback.as_deref(), Some("Unknown"));
    }

    #[test]
    fn parse_lookup_comma_list() {
        let rp = parse_ok("Lookup:S\n1,2,3=A\n5-10=B\n");
        let MacroKind::Lookup(t) = &rp.macros[0].kind else {
            panic!();
        };
        // Three comma-separated keys all map to "A" + one range
        assert_eq!(t.entries.len(), 4);
        assert_eq!(t.entries[0].1, "A");
        assert_eq!(t.entries[1].1, "A");
        assert_eq!(t.entries[2].1, "A");
        assert!(matches!(t.entries[3].0, LookupKey::Range(5, 10)));
    }

    #[test]
    fn parse_display_literal() {
        let rp = parse_ok("Display:\nWelcome\n");
        assert_eq!(rp.display.len(), 1);
        assert!(rp.display[0].condition.is_none());
    }

    #[test]
    fn parse_display_at_macro() {
        let rp = parse_ok("Format:Score\nFormatType=SCORE\n\nDisplay:\nScore: @Score(0xH0010)\n");
        assert_eq!(rp.display[0].body.len(), 2);
        match &rp.display[0].body[1] {
            DisplayPart::Macro { name, .. } => assert_eq!(name, "Score"),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_display_full_trigger_condition() {
        // The classic case from the report: `?0xH1234=5?` is a real
        // comparison, not a bare operand.
        let rp = parse_ok("Display:\n?0xH1234=5?In battle\nIdle\n");
        assert_eq!(rp.display.len(), 2);
        assert!(rp.display[0].condition.is_some());
        assert!(rp.display[1].condition.is_none());
    }

    #[test]
    fn parse_display_composite_condition() {
        // Multi-clause AND-condition: `_` joins. From the SML fixture.
        let rp = parse_ok("Display:\n?0xHffff=3_d0xHffff=3_0xHffb3=15?Titlescreen\nFallback\n");
        let cond = rp.display[0].condition.as_ref().unwrap();
        assert_eq!(cond.core.conditions.len(), 3);
    }

    #[test]
    fn render_literal_only() {
        let rp = parse_ok("Display:\nHello\n");
        assert_eq!(rp.render(|_| 0), "Hello");
    }

    #[test]
    fn render_score_macro() {
        let rp = parse_ok("Format:Score\nFormatType=SCORE\n\nDisplay:\nScore: @Score(0xH0000)\n");
        let out = rp.render(|addr| if addr == 0 { 42 } else { 0 });
        assert_eq!(out, "Score: 000042");
    }

    #[test]
    fn render_builtin_number_macro() {
        // `Number` is a built-in macro; no Format: block needed.
        let rp = parse_ok("Display:\nWorld @Number(0xH0000)\n");
        let out = rp.render(|addr| if addr == 0 { 5 } else { 0 });
        assert_eq!(out, "World 5");
    }

    #[test]
    fn render_lookup_via_at_syntax() {
        let rp = parse_ok("Lookup:Zone\n0=Tower\n1=Castle\n\nDisplay:\n@Zone(0xH0010)\n");
        assert_eq!(rp.render(|addr| if addr == 0x10 { 1 } else { 0 }), "Castle");
    }

    #[test]
    fn render_full_trigger_picks_first_match() {
        let rp = parse_ok(
            "Display:\n?0xH0000=1?In battle\n?0xH0001=1?In menu\nOverworld\n",
        );
        assert_eq!(rp.render(|a| if a == 0 { 1 } else { 0 }), "In battle");
        assert_eq!(rp.render(|a| if a == 1 { 1 } else { 0 }), "In menu");
        assert_eq!(rp.render(|_| 0), "Overworld");
    }

    #[test]
    fn render_composite_trigger() {
        let rp = parse_ok(
            "Display:\n?0xH0000=1_0xH0001=2?Both\nNeither\n",
        );
        assert_eq!(
            rp.render(|a| match a {
                0 => 1,
                1 => 2,
                _ => 0,
            }),
            "Both"
        );
        assert_eq!(rp.render(|a| if a == 0 { 1 } else { 0 }), "Neither");
    }

    #[test]
    fn render_macro_size_aware_word() {
        // `0x 0010` is a 16-bit LE read at 0x0010. With size-aware peek,
        // the underlying word is 0x1234. `Number` (signed VALUE format)
        // adds thousands commas → "4,660".
        let rp = parse_ok("Display:\n@Number(0x 0010)\n");
        let mut state = RichPresenceState::for_script(&rp);
        let out = rp.render_with(&mut state, |addr, size| match (addr, size) {
            (0x10, MemSize::Word) => 0x1234,
            _ => 0,
        });
        assert_eq!(out, "4,660");
    }

    #[test]
    fn render_value_expression_addsource_chain() {
        // Legacy value expression: `0xH0010_0xH0011_0xH0012` → sum of three bytes.
        let rp = parse_ok("Display:\n@Number(0xH0010_0xH0011_0xH0012)\n");
        let out = rp.render(|addr| match addr {
            0x10 => 1,
            0x11 => 2,
            0x12 => 3,
            _ => 0,
        });
        assert_eq!(out, "6");
    }

    #[test]
    fn render_value_expression_with_multiplier() {
        let rp = parse_ok("Display:\n@Number(0xH0001*10)\n");
        let out = rp.render(|addr| if addr == 1 { 3 } else { 0 });
        assert_eq!(out, "30");
    }

    #[test]
    fn render_value_expression_negative_immediate_subtract() {
        // `0xH0001_v-1` (kirby-style "lives - 1"): should evaluate to read - 1.
        let rp = parse_ok("Display:\n@Number(0xH0001_v-1)\n");
        let out = rp.render(|addr| if addr == 1 { 5 } else { 0 });
        assert_eq!(out, "4");
    }

    #[test]
    fn format_value_matches_rcheevos() {
        assert_eq!(format_value(FormatKind::Score, 42), "000042");
        assert_eq!(format_value(FormatKind::Value, 0), "0");
        // Value formatting inserts thousands commas.
        assert_eq!(format_value(FormatKind::Value, 1234567), "1,234,567");
        // Score does not.
        assert_eq!(format_value(FormatKind::Score, 1234567), "1234567");
        // Frames: 60 frames = 1 second = 100 cs → "0:01.00".
        assert_eq!(format_value(FormatKind::Frames, 60), "0:01.00");
        // Seconds: 90s → "1:30".
        assert_eq!(format_value(FormatKind::Seconds, 90), "1:30");
        // Minutes: 90m → "1h30".
        assert_eq!(format_value(FormatKind::Minutes, 90), "1h30");
        // Centisecs: 12345 → "2:03.45".
        assert_eq!(format_value(FormatKind::Centisecs, 12345), "2:03.45");
        // Fixed2: 1234 → "12.34".
        assert_eq!(format_value(FormatKind::Fixed2, 1234), "12.34");
        // Tens / Hundreds / Thousands.
        assert_eq!(format_value(FormatKind::Tens, 5), "50");
        assert_eq!(format_value(FormatKind::Tens, 0), "0");
        assert_eq!(format_value(FormatKind::Hundreds, 12), "1,200");
        assert_eq!(format_value(FormatKind::Thousands, 5), "5,000");
        // Unsigned has commas, Unformatted does not.
        assert_eq!(format_value(FormatKind::Unsigned, 12345), "12,345");
        assert_eq!(format_value(FormatKind::Unformatted, 12345), "12345");
    }

    #[test]
    fn parse_comments_ignored() {
        // rcheevos accepts full-line comments between sections but not
        // between a `Format:` header and its `FormatType=` line.
        let rp = parse_ok(
            "// header\nFormat:V\nFormatType=VALUE\n\n// floating\nDisplay:\nHi\n",
        );
        assert_eq!(rp.macros.len(), 1);
        assert_eq!(rp.display.len(), 1);
    }

    #[test]
    fn unknown_macro_emits_marker() {
        // An unknown macro should still render (with a placeholder), not
        // cause a parse failure.
        let rp = parse_ok("Display:\n@Mystery(0xH0000)\n");
        assert!(rp.render(|_| 0).contains("[Unknown macro]Mystery"));
    }

    #[test]
    fn smb_real_world_fixture_parses() {
        // Slimmed Super Mario Land RP: full-trigger conditions + `@Number`
        // built-in + Lookup.
        let src = "Lookup:Mode\n0x00=Normal\n0x01=Hard\n\nFormat:Number\nFormatType=VALUE\n\nDisplay:\n?0xHffff=3_0xHffb3=15?Title\nMario in @Number(0xUffb4)-@Number(0xLffb4) on @Mode(0xHff9a)\n";
        let rp = parse_ok(src);
        assert_eq!(rp.display.len(), 2);
        // Title screen branch.
        let out_title = rp.render(|a| match a {
            0xffff => 3,
            0xffb3 => 15,
            _ => 0,
        });
        assert_eq!(out_title, "Title");
        // Default fallback branch.
        let out = rp.render(|a| match a {
            0xffb4 => 0x12, // upper nibble = 1, lower nibble = 2
            0xff9a => 0x00, // mode 0
            _ => 0,
        });
        assert_eq!(out, "Mario in 1-2 on Normal");
    }
}
