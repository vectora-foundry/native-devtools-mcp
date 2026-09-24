//! Decide whether a JavaScript source string is a single function
//! expression.
//!
//! `cdp_evaluate_script` calls such sources as an IIFE so that
//! `() => document.title` returns the title instead of the function object.
//! Any other expression (`[1, 2].map(x => x * 2)`, `a && (() => 1)`) must be
//! evaluated as is.
//!
//! This is a small hand-written scanner, not a JS parser. It skips string,
//! template and regex literals and comments, and matches brackets, which is
//! enough to see where a function's parameter list and body end.

/// If the whole `source` is one function expression, return the part to
/// call: `source` without surrounding whitespace and without one trailing
/// `;`. Leading and trailing comments are allowed; the caller must put a
/// newline before any closing bracket it adds after the returned text.
///
/// Function expressions are `function ...`, `async function ...`, arrow
/// functions (`x => ...`, `(a, b) => ...`, `async (...) => ...`,
/// `async x => ...`), and any of these wrapped in parentheses.
pub(crate) fn function_expression_source(source: &str) -> Option<&str> {
    let source = strip_trailing_semicolon(source.trim())?;
    is_function_expression(source).then_some(source)
}

fn is_function_expression(source: &str) -> bool {
    let source = skip_leading_comments(source.trim());
    if let Some(inner) = strip_enclosing_parens(source) {
        return is_function_expression(inner);
    }
    if let Some(rest) = strip_keyword(source, "async") {
        if is_function_keyword_expression(rest) || is_arrow_function(rest) {
            return true;
        }
    }
    // Also covers an arrow whose single parameter is named `async`.
    is_function_keyword_expression(source) || is_arrow_function(source)
}

/// `source` up to (not including) its last code token, if that token is a
/// `;`. Comments after it are dropped too. `None` if `source` has an
/// unclosed literal or comment.
fn strip_trailing_semicolon(source: &str) -> Option<&str> {
    let tokens = code_chars(source)?;
    match tokens.last() {
        Some(&(pos, ';')) => Some(source[..pos].trim_end()),
        _ => Some(source),
    }
}

/// `source` without leading `//` and `/* */` comments and whitespace.
fn skip_leading_comments(mut source: &str) -> &str {
    loop {
        source = source.trim_start();
        if let Some(rest) = source.strip_prefix("//") {
            source = rest.find('\n').map_or("", |i| &rest[i..]);
        } else if let Some(rest) = source.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => source = &rest[i + 2..],
                None => return source,
            }
        } else {
            return source;
        }
    }
}

/// Whether `source` holds only whitespace and comments.
fn is_blank(source: &str) -> bool {
    code_chars(source).is_some_and(|tokens| tokens.is_empty())
}

/// `function [*] [name] (params) { body }` with nothing after the body.
fn is_function_keyword_expression(source: &str) -> bool {
    let Some(rest) = strip_keyword(source, "function") else {
        return false;
    };
    let rest = rest.strip_prefix('*').unwrap_or(rest).trim_start();
    let rest = rest[identifier_len(rest)..].trim_start();
    let Some(after_params) = skip_bracketed(rest, '(') else {
        return false;
    };
    match skip_bracketed(skip_leading_comments(after_params), '{') {
        Some(after_body) => is_blank(after_body),
        None => false,
    }
}

/// `x => body` or `(params) => body`, where the body runs to the end.
fn is_arrow_function(source: &str) -> bool {
    let after_params = if source.starts_with('(') {
        match skip_bracketed(source, '(') {
            Some(rest) => rest,
            None => return false,
        }
    } else {
        let len = identifier_len(source);
        if len == 0 {
            return false;
        }
        &source[len..]
    };
    let Some(body) = after_params.trim_start().strip_prefix("=>") else {
        return false;
    };
    let body = skip_leading_comments(body);
    if body.starts_with('{') {
        return match skip_bracketed(body, '{') {
            Some(after_body) => is_blank(after_body),
            None => false,
        };
    }
    is_single_expression(body)
}

/// A concise arrow body must be one expression: non-empty and without a
/// top-level `;` or `,` that would end it early.
fn is_single_expression(source: &str) -> bool {
    let Some(tokens) = code_chars(source) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    let mut depth = 0usize;
    for &(_, c) in &tokens {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ';' | ',' if depth == 0 => return false,
            _ => {}
        }
    }
    true
}

/// If `source` is `( ... )` with the outer parentheses matching each other,
/// return what is inside them.
fn strip_enclosing_parens(source: &str) -> Option<&str> {
    let rest = skip_bracketed(source, '(')?;
    if is_blank(rest) {
        Some(&source[1..source.len() - rest.len() - 1])
    } else {
        None
    }
}

/// If `source` starts with `open`, return the text after its matching
/// closing bracket.
fn skip_bracketed(source: &str, open: char) -> Option<&str> {
    if !source.starts_with(open) {
        return None;
    }
    let tokens = code_chars(source)?;
    let mut stack: Vec<char> = Vec::new();
    for &(pos, c) in &tokens {
        match c {
            '(' | '[' | '{' => stack.push(c),
            ')' | ']' | '}' => {
                let expected_open = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if stack.pop() != Some(expected_open) {
                    return None;
                }
                if stack.is_empty() {
                    return Some(&source[pos + c.len_utf8()..]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `source` without a leading `keyword`, if the keyword is a whole word.
fn strip_keyword<'a>(source: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = source.strip_prefix(keyword)?;
    match rest.chars().next() {
        Some(c) if is_identifier_char(c) => None,
        _ => Some(rest.trim_start()),
    }
}

/// Byte length of the identifier at the start of `source` (0 if none).
fn identifier_len(source: &str) -> usize {
    match source.chars().next() {
        Some(c) if is_identifier_char(c) && !c.is_ascii_digit() => source
            .char_indices()
            .find(|&(_, c)| !is_identifier_char(c))
            .map_or(source.len(), |(i, _)| i),
        _ => 0,
    }
}

fn is_identifier_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

/// Keywords after which a `/` starts a regex literal, not a division.
const REGEX_PRECEDING_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

/// Code-level characters of `source` with their byte offsets, whitespace
/// dropped. Each string, template or regex literal becomes one `'"'` at its
/// start, and comments are dropped, so brackets and separators inside them
/// are ignored. Returns `None` if a literal or comment is not closed.
fn code_chars(source: &str) -> Option<Vec<(usize, char)>> {
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut out: Vec<(usize, char)> = Vec::new();
    // Brace depth at which each open template substitution `${` started.
    let mut template_stack: Vec<usize> = Vec::new();
    let mut brace_depth = 0usize;
    let mut word = String::new();
    let mut i = 0;

    let emit = |out: &mut Vec<(usize, char)>, template_stack: &[usize], item: (usize, char)| {
        // Code inside `${ ... }` belongs to the template literal.
        if template_stack.is_empty() {
            out.push(item);
        }
    };

    while i < chars.len() {
        let (pos, c) = chars[i];
        let next = chars.get(i + 1).map(|&(_, n)| n);

        if is_identifier_char(c) {
            let continues_word = i > 0 && is_identifier_char(chars[i - 1].1);
            if !continues_word {
                word.clear();
            }
            word.push(c);
            emit(&mut out, &template_stack, (pos, c));
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // The identifier or keyword right before this character, if any.
        let previous_word = std::mem::take(&mut word);

        match c {
            '\'' | '"' => {
                emit(&mut out, &template_stack, (pos, '"'));
                i = skip_string(&chars, i + 1, c)?;
            }
            '`' => {
                emit(&mut out, &template_stack, (pos, '"'));
                let (after, opened_substitution) = skip_template(&chars, i + 1)?;
                if opened_substitution {
                    template_stack.push(brace_depth);
                }
                i = after;
            }
            '/' if next == Some('/') => {
                i = chars[i..]
                    .iter()
                    .position(|&(_, ch)| ch == '\n')
                    .map_or(chars.len(), |offset| i + offset);
            }
            '/' if next == Some('*') => {
                let close = chars[i + 2..]
                    .windows(2)
                    .position(|w| w[0].1 == '*' && w[1].1 == '/')?;
                i = i + 2 + close + 2;
            }
            '/' if starts_regex(&out, &previous_word) => {
                emit(&mut out, &template_stack, (pos, '"'));
                i = skip_regex(&chars, i + 1)?;
            }
            '{' => {
                brace_depth += 1;
                emit(&mut out, &template_stack, (pos, c));
                i += 1;
            }
            '}' if template_stack.last() == Some(&brace_depth) => {
                // End of a `${ ... }` substitution: back inside the template.
                template_stack.pop();
                let (after, opened_substitution) = skip_template(&chars, i + 1)?;
                if opened_substitution {
                    template_stack.push(brace_depth);
                }
                i = after;
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                emit(&mut out, &template_stack, (pos, c));
                i += 1;
            }
            _ => {
                emit(&mut out, &template_stack, (pos, c));
                i += 1;
            }
        }
    }

    if template_stack.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// Whether a `/` after the code chars `before` starts a regex literal
/// rather than a division. `previous_word` is the identifier or keyword
/// right before the `/`, if any.
fn starts_regex(before: &[(usize, char)], previous_word: &str) -> bool {
    let mut recent = before.iter().rev().map(|&(_, c)| c);
    let Some(previous) = recent.next() else {
        return true;
    };
    match previous {
        // After an identifier or number: division, unless it is a keyword
        // such as `return` that expects an expression.
        c if is_identifier_char(c) => REGEX_PRECEDING_KEYWORDS.contains(&previous_word),
        // Postfix `i++ /` and `i-- /` end an operand: division.
        '+' | '-' => recent.next() != Some(previous),
        c => "(,=:[!&|?{};*%<>~^".contains(c),
    }
}

/// Index just past the closing `quote` of a string literal whose body
/// starts at `start`.
fn skip_string(chars: &[(usize, char)], start: usize, quote: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        match chars[i].1 {
            '\\' => i += 2,
            c if c == quote => return Some(i + 1),
            '\n' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Scan template literal text from `start`. Returns the index after the
/// closing backtick (`false`) or after a `${` that opens a substitution
/// (`true`).
fn skip_template(chars: &[(usize, char)], start: usize) -> Option<(usize, bool)> {
    let mut i = start;
    while i < chars.len() {
        match chars[i].1 {
            '\\' => i += 2,
            '`' => return Some((i + 1, false)),
            '$' if chars.get(i + 1).map(|&(_, c)| c) == Some('{') => return Some((i + 2, true)),
            _ => i += 1,
        }
    }
    None
}

/// Index just past a regex literal (including flags) whose body starts at
/// `start`.
fn skip_regex(chars: &[(usize, char)], start: usize) -> Option<usize> {
    let mut i = start;
    let mut in_class = false;
    while i < chars.len() {
        match chars[i].1 {
            '\\' => i += 2,
            '\n' => return None,
            '[' => {
                in_class = true;
                i += 1;
            }
            ']' => {
                in_class = false;
                i += 1;
            }
            '/' if !in_class => {
                i += 1;
                while i < chars.len() && is_identifier_char(chars[i].1) {
                    i += 1;
                }
                return Some(i);
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::function_expression_source;

    fn is_function_expression(source: &str) -> bool {
        function_expression_source(source).is_some()
    }

    // --- Function expressions: called as an IIFE ---

    #[test]
    fn arrow_with_empty_params_is_function() {
        assert!(is_function_expression("() => document.title"));
    }

    #[test]
    fn async_arrow_with_block_body_is_function() {
        assert!(is_function_expression(
            "async () => { await new Promise(r => setTimeout(r, 10)); return 1; }"
        ));
    }

    #[test]
    fn arrow_with_bare_param_is_function() {
        assert!(is_function_expression("x => x * 2"));
    }

    #[test]
    fn arrow_with_multiple_params_is_function() {
        assert!(is_function_expression("(a, b) => a + b"));
    }

    #[test]
    fn async_arrow_with_bare_param_is_function() {
        assert!(is_function_expression("async x => await x"));
    }

    #[test]
    fn arrow_whose_param_is_named_async_is_function() {
        assert!(is_function_expression("async => async + 1"));
    }

    #[test]
    fn arrow_returning_object_literal_is_function() {
        assert!(is_function_expression("() => ({ title: document.title })"));
    }

    #[test]
    fn arrow_with_default_param_containing_arrow_is_function() {
        assert!(is_function_expression("(cb = () => 1) => cb()"));
    }

    #[test]
    fn function_keyword_expression_is_function() {
        assert!(is_function_expression(
            "function() { return document.title; }"
        ));
    }

    #[test]
    fn named_async_function_is_function() {
        assert!(is_function_expression(
            "async function load() { return 1; }"
        ));
    }

    #[test]
    fn function_with_surrounding_whitespace_is_function() {
        assert!(is_function_expression("\n  () => 1  \n"));
    }

    #[test]
    fn parenthesized_arrow_is_function() {
        assert!(is_function_expression("(() => 1)"));
    }

    #[test]
    fn block_body_with_closing_brace_in_string_is_function() {
        assert!(is_function_expression("() => { return '}'; }"));
    }

    #[test]
    fn block_body_with_bracket_in_regex_is_function() {
        assert!(is_function_expression(
            "() => { return document.title.replace(/[)}]/g, ''); }"
        ));
    }

    #[test]
    fn block_body_with_template_substitution_is_function() {
        assert!(is_function_expression(
            "() => { return `${document.title} }`; }"
        ));
    }

    #[test]
    fn block_body_with_brace_in_comment_is_function() {
        assert!(is_function_expression(
            "() => {\n  // closes: }\n  return 1;\n}"
        ));
    }

    #[test]
    fn block_body_with_regex_after_return_is_function() {
        assert!(is_function_expression("() => { return /}/.test('x'); }"));
    }

    #[test]
    fn division_in_concise_body_is_function() {
        assert!(is_function_expression("el => el.clientWidth / 2"));
    }

    #[test]
    fn leading_line_comment_before_arrow_is_function() {
        assert!(is_function_expression("// title\n() => document.title"));
    }

    #[test]
    fn leading_block_comment_before_arrow_is_function() {
        assert!(is_function_expression("/* title */ () => document.title"));
    }

    #[test]
    fn trailing_line_comment_after_block_body_is_function() {
        assert!(is_function_expression("() => { return 1; } // done"));
    }

    #[test]
    fn trailing_line_comment_after_concise_body_is_function() {
        assert!(is_function_expression("() => document.title // title"));
    }

    #[test]
    fn division_after_postfix_increment_is_function() {
        assert!(is_function_expression("() => i++ / 2"));
    }

    #[test]
    fn division_after_postfix_decrement_is_function() {
        assert!(is_function_expression("() => i-- / 2"));
    }

    #[test]
    fn division_after_closing_paren_is_function() {
        assert!(is_function_expression("() => (a + b) / 2"));
    }

    #[test]
    fn division_after_closing_bracket_is_function() {
        assert!(is_function_expression("() => values[0] / 2"));
    }

    #[test]
    fn division_after_number_is_function() {
        assert!(is_function_expression("() => 10 / 2"));
    }

    #[test]
    fn trailing_semicolon_after_concise_body_is_function() {
        assert!(is_function_expression("() => document.title;"));
    }

    #[test]
    fn trailing_semicolon_after_function_body_is_function() {
        assert!(is_function_expression("function() { return 1; };"));
    }

    #[test]
    fn trailing_semicolon_is_removed_from_callable_source() {
        assert_eq!(
            function_expression_source("  () => document.title;  "),
            Some("() => document.title")
        );
    }

    #[test]
    fn trailing_semicolon_and_comment_are_removed_from_callable_source() {
        assert_eq!(
            function_expression_source("() => 1; // one"),
            Some("() => 1")
        );
    }

    #[test]
    fn semicolon_inside_block_body_is_kept_in_callable_source() {
        assert_eq!(
            function_expression_source("() => { return 1; }"),
            Some("() => { return 1; }")
        );
    }

    #[test]
    fn two_trailing_semicolons_are_not_function() {
        assert!(!is_function_expression("() => 1;;"));
    }

    #[test]
    fn statement_list_ending_in_semicolon_is_not_function() {
        assert!(!is_function_expression("const f = () => 1; f();"));
    }

    // --- Other expressions: evaluated as is ---

    #[test]
    fn array_map_with_arrow_callback_is_not_function() {
        assert!(!is_function_expression("[1,2].map(x => x*2)"));
    }

    #[test]
    fn logical_and_with_parenthesized_arrow_is_not_function() {
        assert!(!is_function_expression("a && (() => 1)"));
    }

    #[test]
    fn parenthesized_arithmetic_is_not_function() {
        assert!(!is_function_expression("(1 + 2)"));
    }

    #[test]
    fn parenthesized_object_literal_is_not_function() {
        assert!(!is_function_expression("({a: 1})"));
    }

    #[test]
    fn string_containing_arrow_is_not_function() {
        assert!(!is_function_expression("\"a => b\""));
    }

    #[test]
    fn method_call_on_string_containing_arrow_is_not_function() {
        assert!(!is_function_expression("'x => y'.length"));
    }

    #[test]
    fn immediately_invoked_arrow_is_not_function() {
        assert!(!is_function_expression("(() => 1)()"));
    }

    #[test]
    fn function_call_with_arrow_argument_is_not_function() {
        assert!(!is_function_expression("setTimeout(() => 1, 0)"));
    }

    #[test]
    fn plain_property_access_is_not_function() {
        assert!(!is_function_expression("document.title"));
    }

    #[test]
    fn identifier_starting_with_function_is_not_function() {
        assert!(!is_function_expression("functionResult"));
    }

    #[test]
    fn arrow_followed_by_statement_is_not_function() {
        assert!(!is_function_expression("() => 1; document.title"));
    }

    #[test]
    fn function_followed_by_call_is_not_function() {
        assert!(!is_function_expression(
            "function() { return 1; }.call(null)"
        ));
    }

    #[test]
    fn arrow_with_unbalanced_block_is_not_function() {
        assert!(!is_function_expression("() => { return 1;"));
    }
}
