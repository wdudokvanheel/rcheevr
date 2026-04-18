//! Rich presence parser and formatter.
//!
//! Parses `Format:` / `Lookup:` / `Display:` sections. Re-evaluated per frame
//! against the current memory snapshot.

use crate::memaddr::{parse_operand, Operand};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How a numeric value is formatted in a Format macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatType {
    Value,
    Score,
    Frames,
    TimeSecs, // a.k.a. Time / Secs
    Minutes,
    Millisecs,
    Fixed1,
    Fixed2,
    Fixed3,
    Float1,
    Float2,
    Float3,
    Float4,
    Float5,
    Float6,
    Tens,
    Hundreds,
    Thousands,
    Millions,
    Unsigned,
    Unformatted,
    Other,
}

/// A named formatter definition (`Format:<name>` / `FormatType=<ty>`).
#[derive(Debug, Clone)]
pub struct Format {
    pub name: String,
    pub ty: FormatType,
}

/// A key in a Lookup table.
#[derive(Debug, Clone)]
pub enum LookupKey {
    Single(u32),
    Range(u32, u32),
}

/// A named lookup table (`Lookup:<name>`).
#[derive(Debug, Clone)]
pub struct Lookup {
    pub name: String,
    pub entries: Vec<(LookupKey, String)>,
    pub fallback: Option<String>,
}

/// One segment of a display line body.
#[derive(Debug, Clone)]
pub enum DisplaySegment {
    Literal(String),
    /// `@Name(operand)` — apply a named Format.
    Format {
        name: String,
        operand: Operand,
    },
    /// `{Name(operand)}` — look up a value in a named Lookup.
    Lookup {
        name: String,
        operand: Operand,
    },
}

/// One display line, optionally guarded by a condition.
#[derive(Debug, Clone)]
pub struct DisplayLine {
    /// When `Some`, the line is shown only when the operand evaluates nonzero.
    pub condition: Option<Operand>,
    pub segments: Vec<DisplaySegment>,
}

/// Parsed rich presence script.
#[derive(Debug, Clone, Default)]
pub struct RichPresence {
    pub formats: Vec<Format>,
    pub lookups: Vec<Lookup>,
    pub display: Vec<DisplayLine>,
}

/// Parse error with 1-based line number and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichPresenceParseError {
    pub line: usize,
    pub msg: &'static str,
}

// ---------------------------------------------------------------------------
// FormatType parsing
// ---------------------------------------------------------------------------

fn parse_format_type(s: &str) -> FormatType {
    match s.trim() {
        "VALUE" => FormatType::Value,
        "SCORE" => FormatType::Score,
        "FRAMES" => FormatType::Frames,
        // TIME, TIMESECS, and SECS all map to TimeSecs
        "TIME" | "TIMESECS" | "SECS" => FormatType::TimeSecs,
        "MINUTES" => FormatType::Minutes,
        "MILLISECS" => FormatType::Millisecs,
        "FIXED1" => FormatType::Fixed1,
        "FIXED2" => FormatType::Fixed2,
        "FIXED3" => FormatType::Fixed3,
        "FLOAT1" => FormatType::Float1,
        "FLOAT2" => FormatType::Float2,
        "FLOAT3" => FormatType::Float3,
        "FLOAT4" => FormatType::Float4,
        "FLOAT5" => FormatType::Float5,
        "FLOAT6" => FormatType::Float6,
        "TENS" => FormatType::Tens,
        "HUNDREDS" => FormatType::Hundreds,
        "THOUSANDS" => FormatType::Thousands,
        "MILLIONS" => FormatType::Millions,
        "UNSIGNED" => FormatType::Unsigned,
        "UNFORMATTED" => FormatType::Unformatted,
        _ => FormatType::Other,
    }
}

// ---------------------------------------------------------------------------
// format_value helper
// ---------------------------------------------------------------------------

/// Format a u32 according to the given FormatType.
pub fn format_value(ty: FormatType, v: u32) -> String {
    match ty {
        FormatType::Score => format!("{v:06}"),
        FormatType::Value => v.to_string(),
        FormatType::Frames => {
            // Convert frames @ 60 fps to M:SS or HhMM:SS
            let total_secs = v / 60;
            let h = total_secs / 3600;
            let m = (total_secs / 60) % 60;
            let s = total_secs % 60;
            if h > 0 {
                format!("{h}h{m:02}:{s:02}")
            } else {
                format!("{m}:{s:02}")
            }
        }
        FormatType::TimeSecs => {
            let h = v / 3600;
            let m = (v / 60) % 60;
            let s = v % 60;
            if h > 0 {
                format!("{h}h{m:02}:{s:02}")
            } else {
                format!("{m}:{s:02}")
            }
        }
        FormatType::Minutes => {
            let h = v / 60;
            let m = v % 60;
            format!("{h}h{m:02}")
        }
        FormatType::Millisecs => {
            let s = v / 100;
            let cs = v % 100;
            format!("{s}.{cs:02}")
        }
        FormatType::Fixed1 => format!("{}.{}", v / 10, v % 10),
        FormatType::Fixed2 => format!("{}.{:02}", v / 100, v % 100),
        FormatType::Fixed3 => format!("{}.{:03}", v / 1000, v % 1000),
        // Float types: reinterpret the u32 bits as f32 and format.
        FormatType::Float1 => format!("{:.1}", f32::from_bits(v)),
        FormatType::Float2 => format!("{:.2}", f32::from_bits(v)),
        FormatType::Float3 => format!("{:.3}", f32::from_bits(v)),
        FormatType::Float4 => format!("{:.4}", f32::from_bits(v)),
        FormatType::Float5 => format!("{:.5}", f32::from_bits(v)),
        FormatType::Float6 => format!("{:.6}", f32::from_bits(v)),
        FormatType::Tens => format!("{v}0"),
        FormatType::Hundreds => format!("{v}00"),
        FormatType::Thousands => format!("{v}000"),
        FormatType::Millions => format!("{v}000000"),
        FormatType::Unsigned => v.to_string(),
        FormatType::Unformatted | FormatType::Other => v.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Body parser: literal text + @Name(op) / {Name(op)} macros
// ---------------------------------------------------------------------------

fn parse_body(body: &str, lineno: usize) -> Result<Vec<DisplaySegment>, RichPresenceParseError> {
    let mut segments = Vec::new();
    let mut rest = body;

    while !rest.is_empty() {
        // Find the next macro: '@' or '{'
        let at = rest.find('@');
        let brace = rest.find('{');

        let next = match (at, brace) {
            (None, None) => {
                // Only literal text remains.
                if !rest.is_empty() {
                    segments.push(DisplaySegment::Literal(rest.to_string()));
                }
                return Ok(segments);
            }
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => a.min(b),
        };

        // Flush literal text before the macro.
        if next > 0 {
            segments.push(DisplaySegment::Literal(rest[..next].to_string()));
        }

        let ch = rest.as_bytes()[next];

        if ch == b'@' {
            // `@Name(operand)`
            let after_at = &rest[next + 1..];
            let paren = after_at.find('(').ok_or(RichPresenceParseError {
                line: lineno,
                msg: "expected '(' after @Name",
            })?;
            let name = after_at[..paren].to_string();
            if name.is_empty() {
                return Err(RichPresenceParseError {
                    line: lineno,
                    msg: "empty @Name",
                });
            }
            let after_paren = &after_at[paren + 1..];
            // Find matching closing paren for operand.
            let close = after_paren.find(')').ok_or(RichPresenceParseError {
                line: lineno,
                msg: "expected ')' closing @Name operand",
            })?;
            let operand_str = &after_paren[..close];
            let (operand, _) = parse_operand(operand_str).map_err(|_| RichPresenceParseError {
                line: lineno,
                msg: "invalid operand in @Name()",
            })?;
            segments.push(DisplaySegment::Format { name, operand });
            // Advance past `@Name(operand)`
            rest = &rest[next + 1 + paren + 1 + close + 1..];
        } else {
            // ch == b'{': `{Name(operand)}`
            let after_brace = &rest[next + 1..];
            let paren = after_brace.find('(').ok_or(RichPresenceParseError {
                line: lineno,
                msg: "expected '(' after {Name",
            })?;
            let name = after_brace[..paren].to_string();
            if name.is_empty() {
                return Err(RichPresenceParseError {
                    line: lineno,
                    msg: "empty {Name}",
                });
            }
            let after_paren = &after_brace[paren + 1..];
            // Find matching ')}'
            let close_paren = after_paren.find(')').ok_or(RichPresenceParseError {
                line: lineno,
                msg: "expected ')' closing {Name operand",
            })?;
            let operand_str = &after_paren[..close_paren];
            let (operand, _) = parse_operand(operand_str).map_err(|_| RichPresenceParseError {
                line: lineno,
                msg: "invalid operand in {Name()}",
            })?;
            // Expect '}' immediately after ')'
            let after_close_paren = &after_paren[close_paren + 1..];
            let close_brace = after_close_paren.find('}').ok_or(RichPresenceParseError {
                line: lineno,
                msg: "expected '}' closing {Name()}",
            })?;
            segments.push(DisplaySegment::Lookup { name, operand });
            // Advance past `{Name(operand)}`; skip any text between ')' and '}'
            rest = &rest[next + 1 + paren + 1 + close_paren + 1 + close_brace + 1..];
        }
    }

    Ok(segments)
}

// ---------------------------------------------------------------------------
// Lookup key parser
// ---------------------------------------------------------------------------

fn parse_lookup_key(s: &str) -> Option<LookupKey> {
    if s == "*" {
        return None; // signals "fallback"
    }
    // Range: "lo-hi" (but not negative — RA keys are always non-negative)
    if let Some(dash) = s.find('-') {
        // Avoid treating a lone hex value with no range as a subtraction.
        let lo_str = &s[..dash];
        let hi_str = &s[dash + 1..];
        if !lo_str.is_empty() && !hi_str.is_empty() {
            let lo = parse_u32_key(lo_str)?;
            let hi = parse_u32_key(hi_str)?;
            return Some(LookupKey::Range(lo, hi));
        }
    }
    parse_u32_key(s).map(LookupKey::Single)
}

fn parse_u32_key(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u32>().ok()
    }
}

// ---------------------------------------------------------------------------
// Parser state machine
// ---------------------------------------------------------------------------

/// Which section the parser is currently inside.
enum Section {
    None,
    Format(String), // name; waiting for FormatType= line
    FormatDone,     // format has been fully read
    Lookup(String), // name; collecting key=value lines
    Display,
}

impl RichPresence {
    /// Parse a rich presence script from a string.
    pub fn parse(src: &str) -> Result<Self, RichPresenceParseError> {
        let mut rp = RichPresence::default();
        let mut section = Section::None;
        // Pending lookup being assembled
        let mut pending_lookup: Option<Lookup> = None;
        // Whether current format block has received a FormatType line
        let mut format_type_seen = false;

        for (idx, raw_line) in src.lines().enumerate() {
            let lineno = idx + 1;
            let line = raw_line.trim_end();

            // Comments
            if line.starts_with("//") {
                continue;
            }

            // Section headers
            if let Some(name) = line.strip_prefix("Format:") {
                // Flush pending lookup if any
                if let Some(lk) = pending_lookup.take() {
                    rp.lookups.push(lk);
                }
                // Validate previous format had a FormatType
                if let Section::Format(ref prev_name) = section {
                    if !format_type_seen {
                        return Err(RichPresenceParseError {
                            line: lineno,
                            msg: "Format block missing FormatType= line",
                        });
                    }
                    let _ = prev_name;
                }
                section = Section::Format(name.to_string());
                format_type_seen = false;
                continue;
            }

            if let Some(name) = line.strip_prefix("Lookup:") {
                // Flush pending lookup
                if let Some(lk) = pending_lookup.take() {
                    rp.lookups.push(lk);
                }
                // Validate previous format
                if let Section::Format(_) = section {
                    if !format_type_seen {
                        return Err(RichPresenceParseError {
                            line: lineno,
                            msg: "Format block missing FormatType= line",
                        });
                    }
                }
                pending_lookup = Some(Lookup {
                    name: name.to_string(),
                    entries: Vec::new(),
                    fallback: None,
                });
                section = Section::Lookup(name.to_string());
                continue;
            }

            if line == "Display:" || line.starts_with("Display:") {
                // Flush pending lookup
                if let Some(lk) = pending_lookup.take() {
                    rp.lookups.push(lk);
                }
                // Validate previous format
                if let Section::Format(_) = section {
                    if !format_type_seen {
                        return Err(RichPresenceParseError {
                            line: lineno,
                            msg: "Format block missing FormatType= line",
                        });
                    }
                }
                section = Section::Display;
                continue;
            }

            match section {
                Section::None => {
                    // Ignore unrecognised lines before any section.
                }

                Section::Format(ref name) => {
                    if line.is_empty() {
                        // Blank line ends a Format block.
                        if !format_type_seen {
                            return Err(RichPresenceParseError {
                                line: lineno,
                                msg: "Format block missing FormatType= line",
                            });
                        }
                        section = Section::FormatDone;
                        continue;
                    }
                    if let Some(ty_str) = line
                        .strip_prefix("FormatType=")
                        .or_else(|| line.strip_prefix("FormatType:"))
                    {
                        let ty = parse_format_type(ty_str);
                        rp.formats.push(Format {
                            name: name.clone(),
                            ty,
                        });
                        format_type_seen = true;
                    }
                    // Other lines inside a Format block are silently ignored.
                }

                Section::FormatDone => {
                    // Between sections — ignore blank/extra lines.
                }

                Section::Lookup(ref _name) => {
                    if line.is_empty() {
                        // Blank line ends the lookup block.
                        if let Some(lk) = pending_lookup.take() {
                            rp.lookups.push(lk);
                        }
                        section = Section::None;
                        continue;
                    }
                    // Each non-blank line is `key=value`.
                    let eq = line.find('=').ok_or(RichPresenceParseError {
                        line: lineno,
                        msg: "Lookup entry missing '='",
                    })?;
                    let key_str = &line[..eq];
                    let value_str = line[eq + 1..].to_string();
                    if key_str == "*" {
                        if let Some(ref mut lk) = pending_lookup {
                            lk.fallback = Some(value_str);
                        }
                    } else {
                        let key = parse_lookup_key(key_str).ok_or(RichPresenceParseError {
                            line: lineno,
                            msg: "invalid Lookup key",
                        })?;
                        if let Some(ref mut lk) = pending_lookup {
                            lk.entries.push((key, value_str));
                        }
                    }
                }

                Section::Display => {
                    if line.is_empty() {
                        continue;
                    }
                    // A display line may start with `?cond?` for a conditional.
                    let (condition, body) = if let Some(stripped) = line.strip_prefix('?') {
                        // Find the closing '?'
                        let end = stripped.find('?').ok_or(RichPresenceParseError {
                            line: lineno,
                            msg: "unterminated condition in display line",
                        })?;
                        let cond_str = &stripped[..end];
                        // The condition in RA rich presence is `operand=value`
                        // or just an operand.  For milestone 6 we parse only the
                        // left-hand operand and treat nonzero as true.
                        let (operand, _) =
                            parse_operand(cond_str).map_err(|_| RichPresenceParseError {
                                line: lineno,
                                msg: "invalid condition operand in display line",
                            })?;
                        let body = &stripped[end + 1..]; // after the closing '?'
                        (Some(operand), body)
                    } else {
                        (None, line)
                    };

                    let segments = parse_body(body, lineno)?;
                    rp.display.push(DisplayLine {
                        condition,
                        segments,
                    });
                }
            }
        }

        // Flush any pending lookup at EOF.
        if let Some(lk) = pending_lookup.take() {
            rp.lookups.push(lk);
        }

        // Validate pending Format block (if file ends inside one).
        if let Section::Format(_) = section {
            if !format_type_seen {
                return Err(RichPresenceParseError {
                    line: src.lines().count() + 1,
                    msg: "Format block missing FormatType= line",
                });
            }
        }

        Ok(rp)
    }

    /// Evaluate the operand against memory using the provided peek closure.
    /// Milestone 6: treats every operand as a byte-address read.
    fn eval_operand<F: FnMut(u32) -> u32>(operand: &Operand, peek: &mut F) -> u32 {
        match operand {
            Operand::Memory { address, .. } => peek(*address),
            Operand::ConstInt(v) => *v,
            Operand::ConstFloat(f) => *f as u32,
        }
    }

    /// Render the active display line using the given memory peek closure.
    ///
    /// Iterates display lines in order, evaluates any condition, picks the
    /// first matching conditional line.  If no conditional matches, the last
    /// line is used (it must be unconditional in well-formed scripts, but we
    /// don't enforce that here).
    pub fn render<F: FnMut(u32) -> u32>(&self, mut peek: F) -> String {
        if self.display.is_empty() {
            return String::new();
        }

        // Find the first conditional line whose condition is satisfied.
        let chosen = self
            .display
            .iter()
            .find(|dl| match &dl.condition {
                Some(op) => Self::eval_operand(op, &mut peek) != 0,
                None => false, // unconditional lines don't win by default
            })
            .or_else(|| {
                // Fallback: last line (whether conditional or not).
                self.display.last()
            });

        let Some(line) = chosen else {
            return String::new();
        };

        self.render_line(line, &mut peek)
    }

    fn render_line<F: FnMut(u32) -> u32>(&self, line: &DisplayLine, peek: &mut F) -> String {
        let mut out = String::new();
        for seg in &line.segments {
            match seg {
                DisplaySegment::Literal(s) => out.push_str(s),
                DisplaySegment::Format { name, operand } => {
                    let v = Self::eval_operand(operand, peek);
                    let ty = self
                        .formats
                        .iter()
                        .find(|f| f.name == *name)
                        .map(|f| f.ty)
                        .unwrap_or(FormatType::Value);
                    out.push_str(&format_value(ty, v));
                }
                DisplaySegment::Lookup { name, operand } => {
                    let v = Self::eval_operand(operand, peek);
                    if let Some(lk) = self.lookups.iter().find(|l| l.name == *name) {
                        // Try Single match first, then Range.
                        let found = lk.entries.iter().find_map(|(key, val)| match key {
                            LookupKey::Single(k) if *k == v => Some(val.as_str()),
                            LookupKey::Range(lo, hi) if v >= *lo && v <= *hi => Some(val.as_str()),
                            _ => None,
                        });
                        if let Some(s) = found {
                            out.push_str(s);
                        } else if let Some(ref fb) = lk.fallback {
                            out.push_str(fb);
                        }
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // 1. Empty input
    // -----------------------------------------------------------------------

    #[test]
    fn parse_empty_input() {
        let rp = RichPresence::parse("").unwrap();
        assert!(rp.formats.is_empty());
        assert!(rp.lookups.is_empty());
        assert!(rp.display.is_empty());
    }

    // -----------------------------------------------------------------------
    // 2. Single Format block
    // -----------------------------------------------------------------------

    #[test]
    fn parse_format_block() {
        let src = "Format:Score\nFormatType=SCORE\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.formats.len(), 1);
        assert_eq!(rp.formats[0].name, "Score");
        assert_eq!(rp.formats[0].ty, FormatType::Score);
    }

    // -----------------------------------------------------------------------
    // 3. Lookup block with single-key entries
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lookup_single_keys() {
        let src = "Lookup:Zone\n0=Tower\n1=Castle\n2=Dungeon\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.lookups.len(), 1);
        let lk = &rp.lookups[0];
        assert_eq!(lk.name, "Zone");
        assert_eq!(lk.entries.len(), 3);
        assert!(matches!(lk.entries[0].0, LookupKey::Single(0)));
        assert_eq!(lk.entries[0].1, "Tower");
        assert!(matches!(lk.entries[2].0, LookupKey::Single(2)));
        assert_eq!(lk.entries[2].1, "Dungeon");
    }

    // -----------------------------------------------------------------------
    // 4. Lookup with range entry
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lookup_range_entry() {
        let src = "Lookup:Stage\n1-3=Forest\n4-6=Cave\n";
        let rp = RichPresence::parse(src).unwrap();
        let lk = &rp.lookups[0];
        assert!(matches!(lk.entries[0].0, LookupKey::Range(1, 3)));
        assert_eq!(lk.entries[0].1, "Forest");
        assert!(matches!(lk.entries[1].0, LookupKey::Range(4, 6)));
    }

    // -----------------------------------------------------------------------
    // 5. Lookup with * default fallback
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lookup_fallback() {
        let src = "Lookup:Zone\n0=Tower\n*=Unknown\n";
        let rp = RichPresence::parse(src).unwrap();
        let lk = &rp.lookups[0];
        assert_eq!(lk.fallback.as_deref(), Some("Unknown"));
        assert_eq!(lk.entries.len(), 1);
    }

    // -----------------------------------------------------------------------
    // 6. Display block with a literal-only line
    // -----------------------------------------------------------------------

    #[test]
    fn parse_display_literal() {
        let src = "Display:\nIn the overworld\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.display.len(), 1);
        let dl = &rp.display[0];
        assert!(dl.condition.is_none());
        assert_eq!(dl.segments.len(), 1);
        assert!(matches!(&dl.segments[0], DisplaySegment::Literal(s) if s == "In the overworld"));
    }

    // -----------------------------------------------------------------------
    // 7. Display with @Format macro
    // -----------------------------------------------------------------------

    #[test]
    fn parse_display_format_macro() {
        let src = "Display:\nScore: @Score(0xH5678)\n";
        let rp = RichPresence::parse(src).unwrap();
        let dl = &rp.display[0];
        assert_eq!(dl.segments.len(), 2);
        assert!(matches!(&dl.segments[0], DisplaySegment::Literal(s) if s == "Score: "));
        match &dl.segments[1] {
            DisplaySegment::Format { name, operand } => {
                assert_eq!(name, "Score");
                assert!(matches!(
                    operand,
                    Operand::Memory {
                        address: 0x5678,
                        ..
                    }
                ));
            }
            other => panic!("expected Format segment, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // 8. Display with {Lookup} macro
    // -----------------------------------------------------------------------

    #[test]
    fn parse_display_lookup_macro() {
        let src = "Display:\n{Zone(0xH1234)}\n";
        let rp = RichPresence::parse(src).unwrap();
        let dl = &rp.display[0];
        assert_eq!(dl.segments.len(), 1);
        match &dl.segments[0] {
            DisplaySegment::Lookup { name, operand } => {
                assert_eq!(name, "Zone");
                assert!(matches!(
                    operand,
                    Operand::Memory {
                        address: 0x1234,
                        ..
                    }
                ));
            }
            other => panic!("expected Lookup segment, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // 9. Display with conditional prefix
    // -----------------------------------------------------------------------

    #[test]
    fn parse_display_conditional() {
        let src = "Display:\n?0xH0000=1?In battle\nOut of battle\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.display.len(), 2);
        let cond_line = &rp.display[0];
        assert!(cond_line.condition.is_some());
        // The operand should be the memory ref 0x0000
        assert!(matches!(
            cond_line.condition.as_ref().unwrap(),
            Operand::Memory {
                address: 0x0000,
                ..
            }
        ));
        let fallback_line = &rp.display[1];
        assert!(fallback_line.condition.is_none());
    }

    // -----------------------------------------------------------------------
    // 10. Render literal-only display line
    // -----------------------------------------------------------------------

    #[test]
    fn render_literal_only() {
        let src = "Display:\nWelcome!\n";
        let rp = RichPresence::parse(src).unwrap();
        let result = rp.render(|_| 0);
        assert_eq!(result, "Welcome!");
    }

    // -----------------------------------------------------------------------
    // 11. Render with @Score macro
    // -----------------------------------------------------------------------

    #[test]
    fn render_format_macro_score() {
        let src = "Format:Score\nFormatType=SCORE\n\nDisplay:\nScore: @Score(0xH0000)\n";
        let rp = RichPresence::parse(src).unwrap();
        // Address 0x0000 returns 42
        let result = rp.render(|addr| if addr == 0x0000 { 42 } else { 0 });
        assert_eq!(result, "Score: 000042");
    }

    // -----------------------------------------------------------------------
    // 12. Render with {Lookup} macro against a matching key
    // -----------------------------------------------------------------------

    #[test]
    fn render_lookup_macro_match() {
        let src = "Lookup:Zone\n0=Tower\n1=Castle\n2=Dungeon\n\nDisplay:\n{Zone(0xH0010)}\n";
        let rp = RichPresence::parse(src).unwrap();
        let result = rp.render(|addr| if addr == 0x0010 { 1 } else { 0 });
        assert_eq!(result, "Castle");
    }

    // -----------------------------------------------------------------------
    // 13. Render picks first matching conditional line
    // -----------------------------------------------------------------------

    #[test]
    fn render_picks_first_matching_conditional() {
        let src = "Display:\n?0xH0000?Battle mode\n?0xH0001?Menu mode\nOverworld\n";
        let rp = RichPresence::parse(src).unwrap();
        // 0x0000=1, 0x0001=0 → first conditional matches
        let result = rp.render(|addr| if addr == 0x0000 { 1 } else { 0 });
        assert_eq!(result, "Battle mode");
    }

    // -----------------------------------------------------------------------
    // 14. Render falls back to last line when no conditional matches
    // -----------------------------------------------------------------------

    #[test]
    fn render_fallback_last_line() {
        let src = "Display:\n?0xH0000?Battle mode\n?0xH0001?Menu mode\nOverworld\n";
        let rp = RichPresence::parse(src).unwrap();
        // Both addresses return 0 → no condition matches → last line used
        let result = rp.render(|_| 0);
        assert_eq!(result, "Overworld");
    }

    // -----------------------------------------------------------------------
    // 15. format_value for several types
    // -----------------------------------------------------------------------

    #[test]
    fn format_value_score() {
        assert_eq!(format_value(FormatType::Score, 42), "000042");
        assert_eq!(format_value(FormatType::Score, 1000000), "1000000");
    }

    #[test]
    fn format_value_value() {
        assert_eq!(format_value(FormatType::Value, 0), "0");
        assert_eq!(format_value(FormatType::Value, 12345), "12345");
    }

    #[test]
    fn format_value_frames() {
        // 3600 frames = 60 s = 1:00
        assert_eq!(format_value(FormatType::Frames, 3600), "1:00");
        // 216000 frames = 3600 s = 1h00:00
        assert_eq!(format_value(FormatType::Frames, 216000), "1h00:00");
    }

    #[test]
    fn format_value_timesecs() {
        assert_eq!(format_value(FormatType::TimeSecs, 90), "1:30");
        assert_eq!(format_value(FormatType::TimeSecs, 3661), "1h01:01");
    }

    #[test]
    fn format_value_fixed2() {
        assert_eq!(format_value(FormatType::Fixed2, 1234), "12.34");
        assert_eq!(format_value(FormatType::Fixed2, 100), "1.00");
    }

    #[test]
    fn format_value_tens() {
        assert_eq!(format_value(FormatType::Tens, 5), "50");
        assert_eq!(format_value(FormatType::Tens, 12), "120");
    }

    // -----------------------------------------------------------------------
    // 16. Parser errors
    // -----------------------------------------------------------------------

    #[test]
    fn error_format_without_format_type() {
        // Format block with no FormatType= before next section.
        let src = "Format:Score\nLookup:Zone\n0=Tower\n";
        let err = RichPresence::parse(src).unwrap_err();
        assert_eq!(err.msg, "Format block missing FormatType= line");
    }

    #[test]
    fn error_lookup_entry_missing_equals() {
        let src = "Lookup:Zone\nTower\n";
        let err = RichPresence::parse(src).unwrap_err();
        assert_eq!(err.msg, "Lookup entry missing '='");
    }

    // -----------------------------------------------------------------------
    // 17. Multiple sections combined
    // -----------------------------------------------------------------------

    #[test]
    fn parse_full_script() {
        let src = "Format:Score\nFormatType=SCORE\n\nLookup:Zone\n0=Tower\n1=Castle\n\nDisplay:\n@Score(0xH0010) in {Zone(0xH0020)}\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.formats.len(), 1);
        assert_eq!(rp.lookups.len(), 1);
        assert_eq!(rp.display.len(), 1);

        let result = rp.render(|addr| match addr {
            0x0010 => 99,
            0x0020 => 1,
            _ => 0,
        });
        assert_eq!(result, "000099 in Castle");
    }

    // -----------------------------------------------------------------------
    // 18. Lookup range match during render
    // -----------------------------------------------------------------------

    #[test]
    fn render_lookup_range_match() {
        let src = "Lookup:Stage\n1-5=Early\n6-10=Late\n\nDisplay:\n{Stage(0xH0000)}\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.render(|_| 3), "Early");
        assert_eq!(rp.render(|_| 8), "Late");
    }

    // -----------------------------------------------------------------------
    // 19. Lookup fallback when no key matches
    // -----------------------------------------------------------------------

    #[test]
    fn render_lookup_fallback() {
        let src = "Lookup:Zone\n0=Tower\n*=Unknown\n\nDisplay:\n{Zone(0xH0000)}\n";
        let rp = RichPresence::parse(src).unwrap();
        // Address returns 99 → no single-key match → fallback
        assert_eq!(rp.render(|_| 99), "Unknown");
        // Address returns 0 → matches Single(0)
        assert_eq!(rp.render(|_| 0), "Tower");
    }

    // -----------------------------------------------------------------------
    // 20. Comment lines are ignored
    // -----------------------------------------------------------------------

    #[test]
    fn parse_comment_lines_ignored() {
        let src = "// This is a comment\nFormat:Score\n// another comment\nFormatType=VALUE\n";
        let rp = RichPresence::parse(src).unwrap();
        assert_eq!(rp.formats.len(), 1);
        assert_eq!(rp.formats[0].ty, FormatType::Value);
    }

    // -----------------------------------------------------------------------
    // 21. Hex lookup key (0x...)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lookup_hex_key() {
        let src = "Lookup:Status\n0x01=Active\n0x02=Inactive\n";
        let rp = RichPresence::parse(src).unwrap();
        let lk = &rp.lookups[0];
        assert!(matches!(lk.entries[0].0, LookupKey::Single(1)));
        assert!(matches!(lk.entries[1].0, LookupKey::Single(2)));
    }
}
