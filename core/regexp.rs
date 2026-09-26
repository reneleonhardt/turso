use crate::ext::register_scalar_function;
use turso_ext::{scalar, ExtensionApi, Value, ValueType};

pub fn register_extension(ext_api: &mut ExtensionApi) {
    unsafe {
        register_scalar_function(ext_api.ctx, c"regexp".as_ptr(), regexp);
        register_scalar_function(ext_api.ctx, c"regexp_like".as_ptr(), regexp_like);
        register_scalar_function(
            ext_api.ctx,
            c"pg_similar_to_regex".as_ptr(),
            pg_similar_to_regex,
        );
    }
}

#[scalar(name = "regexp")]
fn regexp(args: &[Value]) -> Value {
    // Registered as varargs, so this is the only arity check. Indexing out of
    // bounds below would panic through the `extern "C"` shim and abort.
    if args.len() != 2 {
        return Value::error_with_message("wrong number of arguments to function regexp()".into());
    }
    let Some(pattern) = args[0].to_text_coerced() else {
        return Value::null();
    };
    let Some(haystack) = args[1].to_text_coerced() else {
        return Value::null();
    };

    let re = match cached_regex(&pattern, None, RegexMode::Sqlite) {
        Ok(re) => re,
        Err(_) => return Value::null(),
    };

    Value::from_integer(re.is_match(&haystack) as i64)
}

#[scalar(name = "regexp_like")]
fn regexp_like(args: &[Value]) -> Value {
    if !(2..=3).contains(&args.len()) {
        return Value::error_with_message("regexp_like() expects two or three arguments".into());
    }
    if args.iter().any(|arg| arg.value_type() == ValueType::Null) {
        return Value::null();
    }
    let (Some(source), Some(pattern)) = (args[0].to_text_coerced(), args[1].to_text_coerced())
    else {
        return Value::null();
    };
    let flags = match args.get(2).map(Value::to_text_coerced) {
        Some(Some(flags)) => Some(flags),
        Some(None) => return Value::null(),
        None => None,
    };
    match cached_regex(&pattern, flags.as_deref(), RegexMode::Postgres) {
        Ok(regex) => Value::from_integer(regex.is_match(&source) as i64),
        Err(error) => Value::error_with_message(error.into()),
    }
}

#[scalar(name = "pg_similar_to_regex")]
fn pg_similar_to_regex(args: &[Value]) -> Value {
    if !(1..=2).contains(&args.len()) {
        return Value::error_with_message(
            "pg_similar_to_regex() expects one or two arguments".into(),
        );
    }
    if args.iter().any(|arg| arg.value_type() == ValueType::Null) {
        return Value::null();
    }
    let Some(pattern) = args[0].to_text_coerced() else {
        return Value::null();
    };
    let escape = match args.get(1).and_then(Value::to_text_coerced) {
        Some(escape) if escape.is_empty() => None,
        Some(escape) => {
            let mut chars = escape.chars();
            let Some(escape) = chars.next() else {
                return Value::null();
            };
            if chars.next().is_some() {
                return Value::error_with_message(
                    "invalid escape string; escape must be empty or one character".into(),
                );
            }
            Some(escape)
        }
        None if args.len() == 1 => Some('\\'),
        None => return Value::null(),
    };
    Value::from_text(convert_similar_pattern(&pattern, escape))
}

fn convert_similar_pattern(pattern: &str, escape: Option<char>) -> String {
    let mut regex = String::with_capacity(pattern.len() + 8);
    regex.push_str("^(?:");
    let mut escaped = false;
    let mut bracket_depth = 0usize;
    let mut class_position = 0usize;
    for character in pattern.chars() {
        if escaped {
            if character.is_ascii_alphanumeric() {
                regex.push('\\');
                regex.push(character);
            } else {
                regex.push_str(&regex::escape(&character.to_string()));
            }
            escaped = false;
            if bracket_depth > 0 {
                class_position = 3;
            }
        } else if escape == Some(character) {
            escaped = true;
        } else if bracket_depth > 0 {
            if character == '\\' {
                regex.push('\\');
            }
            regex.push(character);
            if character == ']' && class_position > 2 {
                bracket_depth -= 1;
            } else if character == '[' {
                bracket_depth += 1;
                class_position = 3;
            } else if character == '^' {
                class_position += 1;
            } else {
                class_position = 3;
            }
        } else {
            match character {
                '[' => {
                    regex.push(character);
                    bracket_depth = 1;
                    class_position = 1;
                }
                '%' => regex.push_str(".*"),
                '_' => regex.push('.'),
                '(' => regex.push_str("(?:"),
                '.' | '^' | '$' | '\\' => {
                    regex.push('\\');
                    regex.push(character);
                }
                _ => regex.push(character),
            }
        }
    }
    regex.push_str(")$");
    regex
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegexMode {
    Sqlite,
    Postgres,
}

struct RegexCacheEntry {
    pattern: String,
    flags: Option<String>,
    mode: RegexMode,
    compiled: Result<std::rc::Rc<regex::Regex>, String>,
}

std::thread_local! {
    static REGEX_CACHE: std::cell::RefCell<Vec<RegexCacheEntry>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn cached_regex(
    pattern: &str,
    flags: Option<&str>,
    mode: RegexMode,
) -> Result<std::rc::Rc<regex::Regex>, String> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache.iter().position(|entry| {
            entry.pattern == pattern && entry.flags.as_deref() == flags && entry.mode == mode
        }) {
            let entry = cache.remove(index);
            cache.push(entry);
        } else {
            let compiled = match mode {
                RegexMode::Sqlite => regex::Regex::new(pattern).map_err(|error| error.to_string()),
                RegexMode::Postgres => compile_regex(pattern, flags),
            }
            .map(std::rc::Rc::new);
            if cache.len() == 4 {
                cache.remove(0);
            }
            cache.push(RegexCacheEntry {
                pattern: pattern.to_owned(),
                flags: flags.map(str::to_owned),
                mode,
                compiled,
            });
        }
        cache.last().unwrap().compiled.clone()
    })
}

fn compile_regex(pattern: &str, flags: Option<&str>) -> Result<regex::Regex, String> {
    let mut case_insensitive = false;
    let mut multi_line = false;
    let mut dot_matches_new_line = true;
    let mut ignore_whitespace = false;
    let mut quote = false;
    if let Some(flags) = flags {
        for flag in flags.chars() {
            match flag {
                'i' => case_insensitive = true,
                'c' => case_insensitive = false,
                'm' | 'n' => {
                    multi_line = true;
                    dot_matches_new_line = false;
                }
                'p' => {
                    multi_line = false;
                    dot_matches_new_line = false;
                }
                's' => {
                    multi_line = false;
                    dot_matches_new_line = true;
                }
                'w' => {
                    multi_line = true;
                    dot_matches_new_line = true;
                }
                'q' => quote = true,
                'x' => ignore_whitespace = true,
                't' => ignore_whitespace = false,
                _ => return Err(format!("unsupported regular expression flag: {flag}")),
            }
        }
    }
    let pattern = if quote {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .multi_line(multi_line)
        .dot_matches_new_line(dot_matches_new_line)
        .ignore_whitespace(ignore_whitespace)
        .build()
        .map_err(|error| format!("invalid regular expression: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_ext::ValueType;

    /// Go through the generated C-ABI shim, the way `SELECT regexp(..)` does.
    fn call(args: &[Value]) -> Value {
        unsafe { regexp(0, args.len() as i32, args.as_ptr(), None, None) }
    }

    fn call_like(args: &[Value]) -> Value {
        unsafe { regexp_like(0, args.len() as i32, args.as_ptr(), None, None) }
    }

    fn call_similar_to_regex(args: &[Value]) -> Value {
        unsafe { pg_similar_to_regex(0, args.len() as i32, args.as_ptr(), None, None) }
    }

    fn text(s: &str) -> Value {
        Value::from_text(s.to_string())
    }

    #[test]
    fn regexp_rejects_wrong_arity() {
        // argc == 1 used to be admitted and then index args[1], aborting.
        for argc in [0usize, 1, 3] {
            let args: Vec<Value> = (0..argc).map(|_| text("a")).collect();
            let result = call(&args);
            let details = result
                .to_error_details()
                .unwrap_or_else(|| panic!("regexp/{argc} should error, got {result:?}"));
            assert_eq!(
                details.1.as_deref(),
                Some("wrong number of arguments to function regexp()"),
                "unexpected error for regexp/{argc}"
            );
        }
    }

    #[test]
    fn regexp_matches_with_two_arguments() {
        assert_eq!(call(&[text("^a.c$"), text("abc")]).to_integer(), Some(1));
        assert_eq!(call(&[text("^a.c$"), text("abd")]).to_integer(), Some(0));
        // An invalid pattern is NULL, not an error.
        assert_eq!(
            call(&[text("("), text("abc")]).value_type(),
            ValueType::Null
        );
    }

    #[test]
    fn regex_cache_reuses_patterns_and_keeps_modes_and_flags_separate() {
        let sqlite = cached_regex("a.b", None, RegexMode::Sqlite).unwrap();
        let repeated = cached_regex("a.b", None, RegexMode::Sqlite).unwrap();
        assert!(std::rc::Rc::ptr_eq(&sqlite, &repeated));

        let postgres = cached_regex("a.b", None, RegexMode::Postgres).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&sqlite, &postgres));
        assert!(!sqlite.is_match("a\nb"));
        assert!(postgres.is_match("a\nb"));

        let no_newline = cached_regex("a.b", Some("n"), RegexMode::Postgres).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&postgres, &no_newline));
        assert!(!no_newline.is_match("a\nb"));
        assert!(cached_regex("(", None, RegexMode::Sqlite).is_err());
        assert!(cached_regex("(", None, RegexMode::Sqlite).is_err());
    }

    #[test]
    fn regexp_like_supports_case_flags() {
        assert_eq!(
            call_like(&[text("Postgres"), text("^postgres$")]).to_integer(),
            Some(0)
        );
        assert_eq!(
            call_like(&[text("Postgres"), text("^postgres$"), text("i")]).to_integer(),
            Some(1)
        );
    }

    #[test]
    fn similar_to_regex_supports_full_match_wildcards_classes_and_escape() {
        let similar_match = |source: &str, pattern: &str, escape: Option<&str>| {
            let mut args = vec![text(pattern)];
            if let Some(escape) = escape {
                args.push(text(escape));
            }
            let regex = call_similar_to_regex(&args);
            call_like(&[text(source), regex, text("s")])
        };
        assert_eq!(similar_match("abc", "a_c", None).to_integer(), Some(1));
        assert_eq!(similar_match("abc", "%(b|d)%", None).to_integer(), Some(1));
        assert_eq!(similar_match("abcd", "bc", None).to_integer(), Some(0));
        assert_eq!(similar_match("a.c", "a.c", None).to_integer(), Some(1));
        assert_eq!(similar_match("abc", "a.c", None).to_integer(), Some(0));
        assert_eq!(similar_match("a1", "a[0-9]", None).to_integer(), Some(1));
        assert_eq!(
            similar_match("a%b", "a!%b", Some("!")).to_integer(),
            Some(1)
        );
        assert_eq!(
            similar_match("a!xxb", "a!%b", Some("")).to_integer(),
            Some(1)
        );
        assert_eq!(similar_match("a%b", r"a\%b", None).to_integer(), Some(1));
    }

    #[test]
    fn similar_to_regex_rejects_invalid_escape_and_propagates_null() {
        assert!(call_similar_to_regex(&[text("a"), text("!!")])
            .to_error_details()
            .is_some());
        assert_eq!(
            call_similar_to_regex(&[Value::null()]).value_type(),
            ValueType::Null
        );
    }

    #[test]
    fn regexp_like_supports_postgres_newline_quoted_and_expanded_flags() {
        assert_eq!(
            call_like(&[text("a\nb"), text("a.b")]).to_integer(),
            Some(1)
        );
        assert_eq!(call_like(&[text("a\nb"), text("^b")]).to_integer(), Some(0));
        assert_eq!(
            call_like(&[text("a\nb"), text("a.b"), text("s")]).to_integer(),
            Some(1)
        );
        assert_eq!(
            call_like(&[text("a\nb"), text("a.b"), text("n")]).to_integer(),
            Some(0)
        );
        assert_eq!(
            call_like(&[text("a\nb"), text("^b"), text("m")]).to_integer(),
            Some(1)
        );
        assert_eq!(
            call_like(&[text("a\nb"), text("a.b"), text("p")]).to_integer(),
            Some(0)
        );
        assert_eq!(
            call_like(&[text("a\nb"), text("^b"), text("p")]).to_integer(),
            Some(0)
        );
        assert_eq!(
            call_like(&[text("a\nb"), text("a.b"), text("w")]).to_integer(),
            Some(1)
        );
        assert_eq!(
            call_like(&[text("a\nb"), text("^b"), text("w")]).to_integer(),
            Some(1)
        );
        assert_eq!(
            call_like(&[text("a+b"), text("a+b"), text("q")]).to_integer(),
            Some(1)
        );
        assert_eq!(
            call_like(&[text("ab"), text("a b"), text("x")]).to_integer(),
            Some(1)
        );
        assert_eq!(
            call_like(&[text("a b"), text("a b"), text("t")]).to_integer(),
            Some(1)
        );
    }

    #[test]
    fn regexp_like_rejects_invalid_patterns_and_flags() {
        assert!(call_like(&[text("abc"), text("(")])
            .to_error_details()
            .is_some());
        assert!(call_like(&[text("abc"), text("a"), text("g")])
            .to_error_details()
            .is_some());
        assert_eq!(
            call_like(&[Value::null(), text("a")]).value_type(),
            ValueType::Null
        );
    }
}
