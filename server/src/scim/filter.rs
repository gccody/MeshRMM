//! SCIM filters (RFC 7644 section 3.4.2.2), evaluated against a resource's
//! JSON. A list runs the conditions identity providers look resources up by
//! (`userName eq`, `displayName eq`, `members[value eq]`...) in SQL: each
//! resource type takes them from a filter's top-level `and` with [`split`],
//! and [`matches`] evaluates the rest on the resources SQL found.
//!
//! Supported: `eq`, `ne`, `co`, `sw`, `ew`, `pr`, `and`, `or`, `not(...)`,
//! parentheses, and value paths such as `emails[type eq "work"]` or
//! `members[value eq "id"]`. Strings compare without case, except IDs.
use serde_json::Value;

use super::attribute_key;

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Compare {
        path: Vec<String>,
        op: Op,
        value: Value,
    },
    Present(Vec<String>),
    /// `attr[filter]`, optionally followed by `.subattr` in a PATCH path.
    ValuePath {
        attribute: String,
        filter: Box<Filter>,
    },
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
    Not(Box<Filter>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Co,
    Sw,
    Ew,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Text(String),
    Open,
    Close,
    OpenBracket,
    CloseBracket,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '[' => {
                chars.next();
                tokens.push(Token::OpenBracket);
            }
            ']' => {
                chars.next();
                tokens.push(Token::CloseBracket);
            }
            '"' => {
                chars.next();
                let mut text = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(escaped) => text.push(escaped),
                            None => return Err("unterminated string".into()),
                        },
                        Some(c) => text.push(c),
                        None => return Err("unterminated string".into()),
                    }
                }
                tokens.push(Token::Text(text));
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"') {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

/// Deeper nesting than any identity provider sends; the parser recurses,
/// so a bound keeps a hostile filter from overflowing the stack.
const MAX_DEPTH: usize = 32;
/// Far longer than any real filter.
const MAX_LENGTH: usize = 4096;

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position += 1;
        token
    }

    fn keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case(keyword))
    }

    fn or(&mut self) -> Result<Filter, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the filter is nested too deeply".into());
        }
        let filter = self.or_inner();
        self.depth -= 1;
        filter
    }

    fn or_inner(&mut self) -> Result<Filter, String> {
        let mut left = self.and()?;
        while self.keyword("or") {
            self.next();
            left = Filter::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Filter, String> {
        let mut left = self.term()?;
        while self.keyword("and") {
            self.next();
            left = Filter::And(Box::new(left), Box::new(self.term()?));
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Filter, String> {
        if self.keyword("not") {
            self.next();
            if self.next() != Some(Token::Open) {
                return Err("expected ( after not".into());
            }
            let inner = self.or()?;
            if self.next() != Some(Token::Close) {
                return Err("expected )".into());
            }
            return Ok(Filter::Not(Box::new(inner)));
        }
        match self.next() {
            Some(Token::Open) => {
                let inner = self.or()?;
                if self.next() != Some(Token::Close) {
                    return Err("expected )".into());
                }
                Ok(inner)
            }
            Some(Token::Word(attribute)) => {
                if self.peek() == Some(&Token::OpenBracket) {
                    self.next();
                    let filter = self.or()?;
                    if self.next() != Some(Token::CloseBracket) {
                        return Err("expected ]".into());
                    }
                    return Ok(Filter::ValuePath {
                        attribute: attribute_key(&attribute),
                        filter: Box::new(filter),
                    });
                }
                let path = split_path(&attribute);
                let Some(Token::Word(op)) = self.next() else {
                    return Err(format!("expected an operator after {attribute}"));
                };
                let op = match op.to_ascii_lowercase().as_str() {
                    "pr" => return Ok(Filter::Present(path)),
                    "eq" => Op::Eq,
                    "ne" => Op::Ne,
                    "co" => Op::Co,
                    "sw" => Op::Sw,
                    "ew" => Op::Ew,
                    other => return Err(format!("the {other} operator isn't supported")),
                };
                let value = match self.next() {
                    Some(Token::Text(text)) => Value::String(text),
                    Some(Token::Word(word)) => match word.to_ascii_lowercase().as_str() {
                        "true" => Value::Bool(true),
                        "false" => Value::Bool(false),
                        "null" => Value::Null,
                        _ => word
                            .parse::<serde_json::Number>()
                            .map(Value::Number)
                            .map_err(|_| format!("invalid value {word}"))?,
                    },
                    _ => return Err("expected a value".into()),
                };
                Ok(Filter::Compare { path, op, value })
            }
            _ => Err("expected an attribute".into()),
        }
    }
}

/// `name.givenName` as `["name", "givenname"]`, without a schema URN.
pub fn split_path(attribute: &str) -> Vec<String> {
    attribute_key(attribute)
        .split('.')
        .map(str::to_owned)
        .collect()
}

pub fn parse(input: &str) -> Result<Filter, String> {
    if input.len() > MAX_LENGTH {
        return Err("the filter is too long".into());
    }
    let mut parser = Parser {
        tokens: tokenize(input)?,
        position: 0,
        depth: 0,
    };
    let filter = parser.or()?;
    if parser.position != parser.tokens.len() {
        return Err("unexpected text after the filter".into());
    }
    Ok(filter)
}

/// A JSON object's member, ignoring case.
pub fn member<'a>(object: &'a Value, key: &str) -> Option<&'a Value> {
    object
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value)
}

/// Every value at `path`, flattening arrays along the way.
fn values_at<'a>(resource: &'a Value, path: &[String]) -> Vec<&'a Value> {
    let mut current = vec![resource];
    for key in path {
        current = current
            .into_iter()
            .flat_map(|value| match value {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                other => vec![other],
            })
            .filter_map(|value| member(value, key))
            .collect();
    }
    current
        .into_iter()
        .flat_map(|value| match value {
            Value::Array(items) => items.iter().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

/// IDs and external IDs are compared exactly; other strings without case.
fn case_exact(path: &[String]) -> bool {
    matches!(path.last().map(String::as_str), Some("id" | "externalid"))
        || path == ["members", "value"]
}

fn compare(actual: &Value, op: Op, expected: &Value, exact: bool) -> bool {
    match (actual, expected) {
        (Value::String(actual), Value::String(expected)) => {
            let (actual, expected) = if exact {
                (actual.clone(), expected.clone())
            } else {
                (actual.to_lowercase(), expected.to_lowercase())
            };
            match op {
                Op::Eq => actual == expected,
                Op::Ne => actual != expected,
                Op::Co => actual.contains(&expected),
                Op::Sw => actual.starts_with(&expected),
                Op::Ew => actual.ends_with(&expected),
            }
        }
        (actual, expected) => match op {
            Op::Eq => actual == expected,
            Op::Ne => actual != expected,
            _ => false,
        },
    }
}

/// Whether `resource` matches `filter`. Inside a value path, `prefix` is
/// the multi-valued attribute the element came from.
pub fn matches(resource: &Value, filter: &Filter) -> bool {
    matches_in(resource, filter, &[])
}

fn matches_in(resource: &Value, filter: &Filter, prefix: &[String]) -> bool {
    match filter {
        Filter::Compare { path, op, value } => {
            let full = [prefix, path].concat();
            let found = values_at(resource, path);
            match op {
                // `ne` holds when no value equals.
                Op::Ne => !found
                    .iter()
                    .any(|actual| compare(actual, Op::Eq, value, case_exact(&full))),
                _ => found
                    .iter()
                    .any(|actual| compare(actual, *op, value, case_exact(&full))),
            }
        }
        Filter::Present(path) => values_at(resource, path)
            .iter()
            .any(|value| !value.is_null() && value.as_str() != Some("")),
        Filter::ValuePath { attribute, filter } => {
            let prefix = [prefix, std::slice::from_ref(attribute)].concat();
            values_at(resource, std::slice::from_ref(attribute))
                .into_iter()
                .any(|element| matches_in(element, filter, &prefix))
        }
        Filter::And(left, right) => {
            matches_in(resource, left, prefix) && matches_in(resource, right, prefix)
        }
        Filter::Or(left, right) => {
            matches_in(resource, left, prefix) || matches_in(resource, right, prefix)
        }
        Filter::Not(inner) => !matches_in(resource, inner, prefix),
    }
}

/// Splits `filter` into the conditions `translate` turns into SQL, taken
/// from its top-level `and`, and the rest, for [`matches`]. A resource
/// matches the filter exactly when it meets every condition and matches the
/// rest, so `translate` must only take a condition SQL tests exactly as
/// [`matches`] would.
pub fn split<T>(
    filter: Filter,
    translate: impl Fn(&Filter) -> Option<T>,
) -> (Vec<T>, Option<Filter>) {
    let mut conditions = Vec::new();
    let mut rest: Option<Filter> = None;
    // A long chain of `and` nests deeply, so it is walked without recursion.
    let mut pending = vec![filter];
    while let Some(filter) = pending.pop() {
        match filter {
            Filter::And(left, right) => {
                pending.push(*right);
                pending.push(*left);
            }
            filter => match translate(&filter) {
                Some(condition) => conditions.push(condition),
                None => {
                    rest = Some(match rest {
                        Some(left) => Filter::And(Box::new(left), Box::new(filter)),
                        None => filter,
                    });
                }
            },
        }
    }
    (conditions, rest)
}

/// The string `filter` says the attribute at `path` equals, as in
/// `userName eq "ada@example.com"`, or with a value path,
/// `emails[value eq "ada@example.com"]` for `["emails", "value"]`.
pub fn equals<'a>(filter: &'a Filter, path: &[&str]) -> Option<&'a str> {
    match filter {
        Filter::Compare {
            path: tested,
            op: Op::Eq,
            value: Value::String(value),
        } if tested.iter().map(String::as_str).eq(path.iter().copied()) => Some(value),
        Filter::ValuePath { attribute, filter } => match path {
            [first, inner @ ..] if !inner.is_empty() && first == attribute => equals(filter, inner),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `filter` tests the top-level attribute `attribute` (lowercase),
/// so evaluating it needs that attribute's values.
pub fn mentions(filter: &Filter, attribute: &str) -> bool {
    match filter {
        Filter::Compare { path, .. } | Filter::Present(path) => {
            path.first().is_some_and(|first| first == attribute)
        }
        Filter::ValuePath {
            attribute: tested, ..
        } => tested == attribute,
        Filter::And(left, right) | Filter::Or(left, right) => {
            mentions(left, attribute) || mentions(right, attribute)
        }
        Filter::Not(inner) => mentions(inner, attribute),
    }
}

/// Whether one element of a multi-valued attribute matches the filter inside
/// a value path, such as `type eq "work"` in `emails[type eq "work"]`.
pub fn element_matches(element: &Value, attribute: &str, filter: &Filter) -> bool {
    matches_in(element, filter, &[attribute.to_owned()])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user() -> Value {
        json!({
            "id": "AbC",
            "userName": "Ada@Example.com",
            "externalId": "ext-1",
            "active": true,
            "name": { "givenName": "Ada" },
            "emails": [
                { "value": "ada@example.com", "type": "work", "primary": true },
                { "value": "ada@home.example", "type": "home" }
            ],
            "members": [{ "value": "u1" }, { "value": "u2" }]
        })
    }

    fn check(filter: &str) -> bool {
        matches(&user(), &parse(filter).unwrap())
    }

    #[test]
    fn equality_ignores_case_except_for_ids() {
        assert!(check(r#"userName eq "ada@example.com""#));
        assert!(check(r#"USERNAME Eq "ADA@EXAMPLE.COM""#));
        assert!(check(
            r#"urn:ietf:params:scim:schemas:core:2.0:User:userName eq "ada@example.com""#
        ));
        assert!(check(r#"id eq "AbC""#));
        assert!(!check(r#"id eq "abc""#));
        assert!(check(r#"externalId eq "ext-1""#));
        assert!(!check(r#"externalId eq "EXT-1""#));
        assert!(check("active eq true"));
        assert!(!check("active eq false"));
    }

    #[test]
    fn operators_logic_and_value_paths() {
        assert!(check(
            r#"name.givenName sw "ad" and emails.value ew "home.example""#
        ));
        assert!(check(r#"userName co "nobody" or externalId pr"#));
        assert!(check(r#"not (userName eq "x")"#));
        assert!(check(
            r#"emails[type eq "work" and value co "example.com"]"#
        ));
        assert!(!check(r#"emails[type eq "other"]"#));
        assert!(check(r#"id eq "AbC" and members[value eq "u2"]"#));
        assert!(!check(r#"members[value eq "U2"]"#));
        assert!(check(r#"userName ne "someone@else""#));
        assert!(!check(r#"title pr"#));
    }

    #[test]
    fn split_takes_the_conditions_sql_tests_from_the_top_level_and() {
        let split_text = |text: &str| {
            split(parse(text).unwrap(), |filter| {
                equals(filter, &["username"]).map(str::to_owned)
            })
        };
        assert_eq!(
            split_text(r#"userName eq "a""#),
            (vec!["a".to_owned()], None)
        );
        let (conditions, rest) =
            split_text(r#"userName eq "a" and active eq true and (userName eq "b")"#);
        assert_eq!(conditions, ["a", "b"]);
        assert_eq!(rest, Some(parse("active eq true").unwrap()));
        let (conditions, rest) = split_text(r#"active eq true and userName eq "a" and title pr"#);
        assert_eq!(conditions, ["a"]);
        assert_eq!(rest, Some(parse("active eq true and title pr").unwrap()));
        // Under `or` and `not`, a condition stays in the rest.
        for text in [
            r#"userName eq "a" or userName eq "b""#,
            r#"not (userName eq "a")"#,
            r#"userName co "a""#,
            r#"userName eq 1"#,
            r#"userName ne "a""#,
        ] {
            assert_eq!(
                split_text(text),
                (vec![], Some(parse(text).unwrap())),
                "{text}"
            );
        }
        let chain = vec![r#"userName eq "a""#; 200].join(" and ");
        assert_eq!(split_text(&chain).0.len(), 200);
    }

    #[test]
    fn equals_reads_attribute_and_value_paths() {
        let check =
            |text: &str, path: &[&str]| equals(&parse(text).unwrap(), path).map(str::to_owned);
        let value = ["emails", "value"];
        assert_eq!(
            check(r#"emails.value eq "a""#, &value).as_deref(),
            Some("a")
        );
        assert_eq!(
            check(r#"emails[value eq "a"]"#, &value).as_deref(),
            Some("a")
        );
        assert_eq!(check(r#"emails[value eq "a"]"#, &["emails"]), None);
        assert_eq!(check(r#"emails[value co "a"]"#, &value), None);
        assert_eq!(
            check(r#"emails[value eq "a" or value eq "b"]"#, &value),
            None
        );
        assert_eq!(check(r#"name.givenName eq "a""#, &["name"]), None);
        assert_eq!(
            check(
                r#"urn:ietf:params:scim:schemas:core:2.0:Group:displayName eq "a""#,
                &["displayname"]
            )
            .as_deref(),
            Some("a")
        );
    }

    #[test]
    fn mentions_finds_top_level_attributes() {
        let check = |text: &str| mentions(&parse(text).unwrap(), "members");
        assert!(check("members pr"));
        assert!(check(r#"members.value eq "a""#));
        assert!(check(
            r#"displayName eq "a" or not (members[value co "a"])"#
        ));
        assert!(!check(r#"displayName eq "a""#));
        assert!(!check(r#"emails[members eq "a"]"#));
    }

    #[test]
    fn bad_filters_are_refused() {
        for filter in [
            r#"userName gt "a""#,
            r#"userName eq"#,
            r#"userName eq "unterminated"#,
            r#"(userName eq "a""#,
            r#"userName eq "a" extra"#,
            "",
        ] {
            assert!(parse(filter).is_err(), "{filter:?}");
        }
        let nested =
            |depth: usize| format!("{}userName pr{}", "(".repeat(depth), ")".repeat(depth));
        assert!(parse(&nested(20)).is_ok());
        assert!(parse(&nested(2000)).is_err());
        assert!(parse(&format!("{}userName pr", "not(".repeat(2000))).is_err());
        assert!(parse(&format!("userName eq \"{}\"", "a".repeat(5000))).is_err());
    }
}
