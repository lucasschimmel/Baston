//! CitizenFX's Lua is not stock Lua 5.4.
//!
//! A manifest saying `lua54 'yes'` gets CfxLua, which adds compound assignment
//! — `x += 1`, `s ..= "!"` — to the grammar. mlua embeds the reference Lua,
//! which rejects them as syntax errors. Two of `cfx-server-data`'s own
//! resources use them, so "runs FiveM Lua resources unmodified" was false for
//! the FiveM resource pack.
//!
//! The alternatives were to patch the embedded Lua, or to translate. This
//! translates, at the source level, before the chunk is loaded:
//!
//! ```lua
//! money += amount        -->  money = money + (amount)
//! t.list[i] ..= "x"      -->  t.list[i] = t.list[i] .. ("x")
//! ```
//!
//! # What it deliberately will not touch
//!
//! Rewriting duplicates the target, so the target has to be safe to evaluate
//! twice. `a[next(t)] += 1` would call `next` twice and mean something else, so
//! a target containing a call is left alone and Lua reports the syntax error it
//! always did. Anything the scanner is not sure of is left alone for the same
//! reason: a missed translation is a clear error at load, while a wrong one is
//! a silent behaviour change.
//!
//! Line numbers survive — every rewrite stays on its own line — so a stack
//! trace still points at the line the author wrote.

/// The operators CfxLua adds. Longest first: `..=` must win over `.` before
/// `.=` is ever considered.
const OPERATORS: &[&str] = &["..=", "+=", "-=", "*=", "/=", "%=", "^="];

/// Keywords a statement may legally follow on the same line.
const STATEMENT_PRECEDERS: &[&str] = &["then", "do", "else", "repeat", "begin"];

/// Keywords that close a block, so they end the right-hand side rather than
/// belonging to it.
const BLOCK_CLOSERS: &[&str] = &["end", "else", "until"];

/// Rewrite CfxLua compound assignments into plain Lua.
///
/// Returns the source unchanged when it contains none, which is the common
/// case and costs one scan.
pub fn expand_compound_assignment(source: &str) -> String {
    let code = code_mask(source);
    let mut out = String::with_capacity(source.len());
    let mut line_start = 0;

    for line in source.split_inclusive('\n') {
        let rewritten = rewrite_line(line, &code[line_start..line_start + line.len()]);
        out.push_str(rewritten.as_deref().unwrap_or(line));
        line_start += line.len();
    }
    out
}

/// Per-byte "this is code, not a string or a comment".
///
/// Compound assignment has to be found in code only: `print("a += b")` is a
/// string, and `-- x += 1` is a comment. Both would otherwise be rewritten.
fn code_mask(source: &str) -> Vec<bool> {
    let bytes = source.as_bytes();
    let mut mask = vec![false; bytes.len()];
    let mut i = 0;

    while i < bytes.len() {
        // Long bracket, as a string `[[…]]` or after `--`.
        if let Some(level) = long_bracket_at(bytes, i) {
            let close = format!("]{}]", "=".repeat(level));
            let end = find(bytes, close.as_bytes(), i).unwrap_or(bytes.len());
            i = (end + close.len()).min(bytes.len());
            continue;
        }
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            // `--[[ … ]]` is handled by the long-bracket branch on the next
            // pass; a plain comment runs to the newline.
            if let Some(level) = long_bracket_at(bytes, i + 2) {
                let close = format!("]{}]", "=".repeat(level));
                let end = find(bytes, close.as_bytes(), i + 2).unwrap_or(bytes.len());
                i = (end + close.len()).min(bytes.len());
            } else {
                i = find(bytes, b"\n", i).unwrap_or(bytes.len());
            }
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                // A backslash escapes the next byte, including the quote.
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(bytes.len());
            continue;
        }
        mask[i] = true;
        i += 1;
    }
    mask
}

/// `[[`, `[=[`, `[==[`… at `i`, and its level.
fn long_bracket_at(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    let mut level = 0;
    while bytes.get(i + 1 + level) == Some(&b'=') {
        level += 1;
    }
    (bytes.get(i + 1 + level) == Some(&b'[')).then_some(level)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Rewrite one line, or `None` when there is nothing to do.
fn rewrite_line(line: &str, code: &[bool]) -> Option<String> {
    let bytes = line.as_bytes();
    let (at, op) = OPERATORS.iter().find_map(|op| {
        let mut from = 0;
        while let Some(at) = find(bytes, op.as_bytes(), from) {
            // `>=`, `<=`, `==` and `~=` never start with one of our operators,
            // so only the code test matters here.
            if code[at] {
                return Some((at, *op));
            }
            from = at + 1;
        }
        None
    })?;

    let (start, target) = assignment_target(line, at)?;
    let before = &line[..start];
    if !is_statement_boundary(before) {
        return None;
    }
    // The rest of the line is the right-hand side, minus anything that closes
    // a block: `if x then a += 1 end` must not read `end` as part of the sum.
    let mut rhs = line[at + op.len()..].trim_end();
    let mut tail = String::new();
    loop {
        let trimmed = rhs.trim_end();
        let Some((head, keyword)) = BLOCK_CLOSERS.iter().find_map(|kw| {
            let head = trimmed.strip_suffix(kw)?;
            let standalone = head
                .as_bytes()
                .last()
                .is_none_or(|byte| !is_name_byte(*byte));
            standalone.then_some((head, *kw))
        }) else {
            break;
        };
        tail = format!(" {keyword}{tail}");
        rhs = head;
    }
    if let Some(head) = rhs.trim_end().strip_suffix(';') {
        tail = format!(";{tail}");
        rhs = head;
    }
    if rhs.trim().is_empty() {
        return None;
    }

    let arithmetic = op.trim_end_matches('=');
    let newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    Some(format!(
        "{before}{target} = {target} {arithmetic} ({}){tail}{newline}",
        rhs.trim()
    ))
}

/// The assignable expression immediately before `at`, as `(start, text)`, if
/// it is one this can safely duplicate.
///
/// The start index matters as much as the text: whitespace may sit between the
/// target and the operator, so the text's length does not locate it.
fn assignment_target(line: &str, at: usize) -> Option<(usize, String)> {
    let bytes = line.as_bytes();
    let mut end = at;
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    let mut start = end;

    loop {
        // An index `[ … ]` is always preceded by the thing being indexed, so
        // consuming one never ends the target — go round again for the name.
        if start > 0 && bytes[start - 1] == b']' {
            let mut depth = 0;
            loop {
                if start == 0 {
                    return None;
                }
                start -= 1;
                match bytes[start] {
                    b']' => depth += 1,
                    b'[' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    // A call inside the index would be evaluated twice.
                    b'(' | b')' => return None,
                    _ => {}
                }
            }
            continue;
        }

        let name_end = start;
        while start > 0 && is_name_byte(bytes[start - 1]) {
            start -= 1;
        }
        if start == name_end {
            return None;
        }
        // A `.` or `:` means the name was a field, so the chain continues.
        if start > 0 && (bytes[start - 1] == b'.' || bytes[start - 1] == b':') {
            start -= 1;
            continue;
        }
        break;
    }

    // A target must start with a name, never `[1] += x` or `.b += x`.
    if !bytes
        .get(start)
        .is_some_and(|b| is_name_byte(*b) && !b.is_ascii_digit())
    {
        return None;
    }
    Some((start, line[start..end].to_owned()))
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether a statement may begin after this text.
fn is_statement_boundary(before: &str) -> bool {
    let trimmed = before.trim_end();
    trimmed.is_empty()
        || trimmed.ends_with(';')
        || STATEMENT_PRECEDERS.iter().any(|kw| {
            trimmed.strip_suffix(kw).is_some_and(|head| {
                head.is_empty() || !head.as_bytes().last().is_some_and(|b| is_name_byte(*b))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(src: &str) -> String {
        expand_compound_assignment(src)
    }

    /// The line that took the server down on the first cfx-server-data boot.
    #[test]
    fn the_line_from_cfx_server_datas_money_resource() {
        assert_eq!(
            expand("    curMoney += amount"),
            "    curMoney = curMoney + (amount)"
        );
    }

    #[test]
    fn every_operator_cfx_adds() {
        assert_eq!(expand("a += 1"), "a = a + (1)");
        assert_eq!(expand("a -= 1"), "a = a - (1)");
        assert_eq!(expand("a *= 2"), "a = a * (2)");
        assert_eq!(expand("a /= 2"), "a = a / (2)");
        assert_eq!(expand("a %= 2"), "a = a % (2)");
        assert_eq!(expand("a ^= 2"), "a = a ^ (2)");
        assert_eq!(expand("s ..= \"x\""), "s = s .. (\"x\")");
    }

    /// The right-hand side is parenthesised, or precedence would change it:
    /// `a = a * 1 + 2` is not `a = a * (1 + 2)`.
    #[test]
    fn the_right_hand_side_keeps_its_meaning() {
        assert_eq!(expand("a *= 1 + 2"), "a = a * (1 + 2)");
    }

    #[test]
    fn targets_with_fields_and_indices() {
        assert_eq!(expand("t.n += 1"), "t.n = t.n + (1)");
        assert_eq!(expand("t[i] += 1"), "t[i] = t[i] + (1)");
        assert_eq!(expand("a.b[2].c += 1"), "a.b[2].c = a.b[2].c + (1)");
        assert_eq!(expand("t[a[i]] += 1"), "t[a[i]] = t[a[i]] + (1)");
    }

    /// Duplicating the target means evaluating it twice. A call in there would
    /// run twice, so it is left for Lua to reject rather than quietly changed.
    #[test]
    fn a_target_containing_a_call_is_left_alone() {
        assert_eq!(expand("t[next(x)] += 1"), "t[next(x)] += 1");
        assert_eq!(expand("f().n += 1"), "f().n += 1");
    }

    #[test]
    fn strings_and_comments_are_not_code() {
        assert_eq!(expand("print('a += b')"), "print('a += b')");
        assert_eq!(expand("-- a += b"), "-- a += b");
        assert_eq!(expand("x = [[ a += b ]]"), "x = [[ a += b ]]");
        assert_eq!(expand("--[[ a += b ]]"), "--[[ a += b ]]");
        assert_eq!(expand("x = [==[ a += b ]==]"), "x = [==[ a += b ]==]");
    }

    #[test]
    fn a_quote_escaped_inside_a_string_does_not_end_it() {
        assert_eq!(expand("x = 'it\\'s a += b'"), "x = 'it\\'s a += b'");
    }

    /// Comparison operators end in `=` too and must survive untouched.
    #[test]
    fn comparisons_are_not_assignments() {
        for src in [
            "if a >= b then",
            "if a <= b then",
            "if a == b then",
            "if a ~= b then",
        ] {
            assert_eq!(expand(src), src);
        }
    }

    #[test]
    fn a_statement_after_then_or_a_semicolon() {
        assert_eq!(expand("if x then a += 1 end"), "if x then a = a + (1) end");
        assert_eq!(expand("b = 1; a += 2"), "b = 1; a = a + (2)");
    }

    /// `then` must be the keyword, not the tail of an identifier.
    #[test]
    fn a_name_ending_in_a_keyword_is_not_a_boundary() {
        assert_eq!(expand("blacken a += 1"), "blacken a += 1");
    }

    #[test]
    fn line_numbers_are_preserved() {
        let src = "local a = 1\na += 1\nprint(a)\n";
        let out = expand(src);
        assert_eq!(out.lines().count(), 3);
        assert_eq!(out.lines().nth(1).unwrap(), "a = a + (1)");
    }

    #[test]
    fn windows_line_endings_survive() {
        assert_eq!(expand("a += 1\r\nb = 2\r\n"), "a = a + (1)\r\nb = 2\r\n");
    }

    #[test]
    fn plain_lua_is_returned_unchanged() {
        let src = "local function f(x)\n  return x + 1\nend\n";
        assert_eq!(expand(src), src);
    }

    #[test]
    fn a_trailing_semicolon_stays_at_the_end() {
        assert_eq!(expand("a += 1;"), "a = a + (1);");
    }
}
