//! Query grammar follows Everything default precedence: OR binds tighter than AND.
//! Matching is NFC-normalized and ICU full-case-folded unless case: is specified.
use crate::index_store::IndexedFile;
use chrono::{Datelike, Duration as DateDuration, Local, NaiveDate, TimeZone};
use icu_casemap::CaseMapper;
use memchr::memmem::Finder;
use pcre2::bytes::{Regex, RegexBuilder};
use serde_json::Value;
use std::{
    cell::{Cell, OnceCell},
    collections::HashMap,
    rc::Rc,
};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    CaseMapper::new()
        .fold_string(&s.nfc().collect::<String>())
        .into_owned()
}
/// Search-only folding is broader than sorting: Everything ignores diacritics
/// by default. Keep `fold` unchanged for natural ordering and explicit matching.
pub fn fold_search(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    fold(
        &s.nfd()
            .filter(|c| !is_combining_mark(*c))
            .collect::<String>(),
    )
}
fn nfc(s: &str) -> String {
    s.nfc().collect()
}

#[derive(Debug)]
pub enum Query {
    All,
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
    Term(Term),
}
#[derive(Debug)]
pub enum Term {
    Text {
        field: Field,
        needle: String,
        finder: Finder<'static>,
        sensitive: bool,
    },
    Pattern {
        field: Field,
        regex: Regex,
    },
    Related {
        recursive: bool,
        query: Box<Query>,
    },
    Resolved(crate::relations::ResolvedPredicate),
    Extension(Vec<String>),
    IsDir(bool),
    IsSymlink,
    Hidden,
    Flags(u32),
    Unknown {
        field: String,
        negate: bool,
    },
    Number {
        field: String,
        low: f64,
        high: f64,
        include_low: bool,
        include_high: bool,
        negate: bool,
    },
    Content {
        needle: String,
        sensitive: bool,
    },
    PropertyText(String, String),
    Matched {
        target: TextTarget,
        matcher: TextMatcher,
    },
}
/// Extra text modes preserve the ordinary folded-name fast path. Their operands
/// are deliberately excluded from trigram narrowing when normalization differs.
#[derive(Clone, Debug)]
pub enum TextTarget {
    Field(Field),
    Content,
    Property(String),
    Extension,
}
#[derive(Debug)]
pub struct TextMatcher {
    needle: String,
    finder: Box<Finder<'static>>,
    candidate_needle: String,
    regex: Option<Regex>,
    options: MatchOptions,
}
#[derive(Clone, Debug)]
struct MatchOptions {
    now: chrono::DateTime<Local>,
    field: Field,
    sensitive: bool,
    regex: bool,
    diacritics: bool,
    prefix: bool,
    suffix: bool,
    whole: bool,
    start: bool,
    end: bool,
    wildcards: bool,
    target: Option<TextTarget>,
    dot_all: bool,
    multiline: bool,
}
impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            now: Local::now(),
            field: Field::Name,
            sensitive: false,
            regex: false,
            diacritics: false,
            prefix: false,
            suffix: false,
            whole: false,
            start: false,
            end: false,
            wildcards: true,
            target: None,
            dot_all: false,
            multiline: false,
        }
    }
}
fn keyword(s: &str) -> String {
    s.to_ascii_lowercase().replace('-', "")
}
fn text_property(key: &str) -> bool {
    matches!(
        key,
        "artist"
            | "album"
            | "title"
            | "kind"
            | "author"
            | "subject"
            | "keywords"
            | "creator"
            | "producer"
            | "genre"
            | "copyright"
            | "composer"
            | "cameramake"
            | "cameramodel"
    )
}
fn numeric_property(key: &str) -> bool {
    matches!(
        key,
        "width"
            | "height"
            | "duration"
            | "pages"
            | "orientation"
            | "iso"
            | "focallength"
            | "aperture"
            | "exposuretime"
    )
}
fn relation_kind(key: &str) -> Option<(bool, Option<bool>)> {
    match key {
        "child" => Some((false, None)),
        "childfile" => Some((false, Some(false))),
        "childfolder" => Some((false, Some(true))),
        "descendant" => Some((true, None)),
        "descendantfile" => Some((true, Some(false))),
        "descendantfolder" => Some((true, Some(true))),
        _ => None,
    }
}
fn relation_scope(prefix: &str, base: &MatchOptions) -> Option<(bool, Option<bool>, MatchOptions)> {
    let mut options = base.clone();
    let mut keys = prefix.strip_suffix(':')?.split(':').peekable();
    while let Some(key) = keys.next() {
        let key = keyword(key);
        if keys.peek().is_none() {
            let (recursive, kind) = relation_kind(&key)?;
            return Some((recursive, kind, options));
        }
        if !modifier(&mut options, &key) {
            return None;
        }
    }
    None
}
/// Preserve old PDF indexes without rewriting the database or re-reading files.
fn property<'a>(file: &'a IndexedFile, key: &str) -> Option<&'a Value> {
    file.properties.get(key).or_else(|| {
        (key == "author" && file.extension.eq_ignore_ascii_case("pdf"))
            .then(|| file.properties.get("artist"))
            .flatten()
    })
}
fn modifier(options: &mut MatchOptions, key: &str) -> bool {
    match keyword(key).as_str() {
        "case" => options.sensitive = true,
        "nocase" => options.sensitive = false,
        "path" => options.field = Field::Path,
        "name" | "nopath" => options.field = Field::Name,
        "parent" => {
            options.field = Field::Parent;
            options.whole = true;
        }
        "pathpart" | "location" | "loc" => options.field = Field::Parent,
        "regex" => options.regex = true,
        "noregex" => options.regex = false,
        "diacritics" | "diac" => options.diacritics = true,
        "nodiacritics" | "nodiac" => options.diacritics = false,
        "wholewords" | "ww" => {
            options.prefix = true;
            options.suffix = true;
        }
        "nowholewords" | "noww" => {
            options.prefix = false;
            options.suffix = false;
        }
        "prefix" => options.prefix = true,
        "noprefix" => options.prefix = false,
        "suffix" => options.suffix = true,
        "nosuffix" => options.suffix = false,
        "whole" | "wholefilename" | "wfn" | "entire" => options.whole = true,
        "nowhole" | "nowholefilename" | "nowfn" | "noentire" => options.whole = false,
        "startwith" | "^" => options.start = true,
        "nostartwith" => options.start = false,
        "endwith" | "$" => options.end = true,
        "noendwith" => options.end = false,
        "nowildcards" => options.wildcards = false,
        "dotall" => options.dot_all = true,
        "nodotall" => options.dot_all = false,
        "multiline" => options.multiline = true,
        "nomultiline" => options.multiline = false,
        _ => return false,
    }
    true
}
fn scoped_options(prefix: &str, base: &MatchOptions) -> Option<(MatchOptions, Option<bool>)> {
    let mut options = base.clone();
    let mut is_dir = None;
    for key in prefix.strip_suffix(':')?.split(':') {
        if modifier(&mut options, key) {
            continue;
        }
        match keyword(key).as_str() {
            "file" => is_dir = Some(false),
            "folder" | "dir" => is_dir = Some(true),
            "content" => options.target = Some(TextTarget::Content),
            "extension" => options.target = Some(TextTarget::Extension),
            key if text_property(key) => options.target = Some(TextTarget::Property(keyword(key))),
            _ => return None,
        }
    }
    Some((options, is_dir))
}
fn restrict_type(query: Query, is_dir: Option<bool>) -> Query {
    match is_dir {
        Some(dir) => Query::And(vec![Query::Term(Term::IsDir(dir)), query]),
        None => query,
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    Name,
    Path,
    Parent,
}
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Atom(String, Option<usize>),
    L,
    R,
    Or,
    Not,
    And,
}

fn lex(input: &str) -> Result<Vec<Token>, String> {
    if input.len() > 65536 {
        return Err("Query exceeds 64 KiB".into());
    }
    let mut out = Vec::new();
    let mut it = input.chars().peekable();
    while let Some(&c) = it.peek() {
        if out.len() >= 8192 {
            return Err("Query exceeds 8192 tokens".into());
        }
        if c.is_whitespace() {
            it.next();
            continue;
        }
        match c {
            '(' | '<' => {
                it.next();
                out.push(Token::L);
                continue;
            }
            ')' | '>' => {
                it.next();
                out.push(Token::R);
                continue;
            }
            '|' => {
                it.next();
                out.push(Token::Or);
                continue;
            }
            '!' => {
                it.next();
                out.push(Token::Not);
                continue;
            }
            _ => {}
        }
        let mut s = String::new();
        let mut quoted = false;
        let mut had_quote = false;
        let mut literal_from = None;
        let mut regex_parentheses = 0usize;
        let mut regex_mode = false;
        let mut regex_escaped = false;
        let mut regex_class = false;
        let mut numeric_comparison = false;
        let mut component_start = 0usize;
        while let Some(&c) = it.peek() {
            if c == '"' {
                if !quoted && literal_from.is_none() {
                    literal_from = Some(s.len());
                }
                it.next();
                quoted = !quoted;
                had_quote = true;
                continue;
            }
            if c == '\\' && quoted {
                it.next();
                if matches!(it.peek(), Some('"') | Some('\\')) {
                    s.push(it.next().unwrap())
                } else {
                    s.push('\\')
                };
                continue;
            }
            if !quoted {
                if c.is_whitespace() {
                    break;
                }
                if regex_mode && !regex_escaped && !regex_class {
                    if c == '<' && s.ends_with(':') {
                        break;
                    }
                    if matches!(c, ')' | '>') && regex_parentheses == 0 {
                        break;
                    }
                    if c == '(' {
                        regex_parentheses += 1;
                    }
                    if c == ')' {
                        regex_parentheses -= 1;
                    }
                } else if !regex_mode
                    && (matches!(c, '(' | ')' | '|')
                        || ((c == '<' || c == '>') && !numeric_comparison && !s.ends_with('=')))
                {
                    break;
                }
            }
            if c == ':' {
                let key = keyword(&s[component_start..]);
                if key == "regex" {
                    regex_mode = true;
                }
                if key == "noregex" {
                    regex_mode = false;
                }
                numeric_comparison = numeric_property(&key)
                    || crate::relations::is_metric(&key)
                    || matches!(
                        key.as_str(),
                        "size"
                            | "width"
                            | "height"
                            | "duration"
                            | "pages"
                            | "dm"
                            | "dc"
                            | "modified"
                            | "created"
                            | "datemodified"
                            | "datecreated"
                    );
                component_start = s.len() + 1;
            } else {
                numeric_comparison = false;
            }
            if regex_mode && !quoted {
                if regex_escaped {
                    regex_escaped = false;
                } else if c == '\\' {
                    regex_escaped = true;
                } else if c == '[' {
                    regex_class = true;
                } else if c == ']' {
                    regex_class = false;
                }
            }
            s.push(c);
            it.next();
        }
        if quoted {
            return Err("Unclosed quotation mark".into());
        }
        if s.is_empty() && !had_quote {
            return Err("Unexpected query character".into());
        }
        out.push(match s.as_str() {
            "OR" if !had_quote => Token::Or,
            "AND" if !had_quote => Token::And,
            "NOT" if !had_quote => Token::Not,
            _ => Token::Atom(s, literal_from),
        });
    }
    Ok(out)
}

pub fn parse(input: &str, macros: &HashMap<String, String>) -> Result<Query, String> {
    parse_at(input, macros, Local::now())
}
/// Bind calendar and relative-date interpretation to the query lease creation time.
pub fn parse_at(
    input: &str,
    macros: &HashMap<String, String>,
    now: chrono::DateTime<Local>,
) -> Result<Query, String> {
    parse_depth(input, macros, 0, now)
}
fn parse_depth(
    input: &str,
    macros: &HashMap<String, String>,
    depth: usize,
    now: chrono::DateTime<Local>,
) -> Result<Query, String> {
    if depth > 16 {
        return Err("Macro expansion exceeds 16 levels (possible cycle)".into());
    }
    let tokens = lex(input)?;
    if tokens.is_empty() {
        return Ok(Query::All);
    }
    let mut parser = Parser {
        tokens,
        pos: 0,
        macros,
        depth,
        nesting: 0,
        remaining: Rc::new(Cell::new(8192)),
    };
    let options = MatchOptions {
        now,
        ..MatchOptions::default()
    };
    let query = parser.and(&options)?;
    if parser.pos != parser.tokens.len() {
        return Err("Unexpected closing group or trailing token".into());
    }
    Ok(query)
}
struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    macros: &'a HashMap<String, String>,
    depth: usize,
    nesting: usize,
    remaining: Rc<Cell<usize>>,
}
impl Parser<'_> {
    fn or(&mut self, options: &MatchOptions) -> Result<Query, String> {
        let mut queries = vec![self.unary(options)?];
        while self.tokens.get(self.pos) == Some(&Token::Or) {
            self.pos += 1;
            queries.push(self.unary(options)?);
        }
        Ok(if queries.len() == 1 {
            queries.remove(0)
        } else {
            Query::Or(queries)
        })
    }
    fn and(&mut self, options: &MatchOptions) -> Result<Query, String> {
        let mut queries = vec![self.or(options)?];
        while self.pos < self.tokens.len() && self.tokens[self.pos] != Token::R {
            if self.tokens[self.pos] == Token::And {
                self.pos += 1;
            }
            queries.push(self.or(options)?);
        }
        Ok(if queries.len() == 1 {
            queries.remove(0)
        } else {
            Query::And(queries)
        })
    }
    fn group(&mut self, options: &MatchOptions) -> Result<Query, String> {
        let query = self.and(options)?;
        if self.tokens.get(self.pos) != Some(&Token::R) {
            return Err("Unclosed search group".into());
        }
        self.pos += 1;
        Ok(query)
    }
    fn unary(&mut self, options: &MatchOptions) -> Result<Query, String> {
        if self.nesting >= 128 {
            return Err("Search expression nesting exceeds 128 levels".into());
        }
        let remaining = self.remaining.get();
        if remaining == 0 {
            return Err("Expanded query exceeds 8192 expressions".into());
        }
        self.remaining.set(remaining - 1);
        self.nesting += 1;
        let result = self.unary_inner(options);
        self.nesting -= 1;
        result
    }
    fn unary_inner(&mut self, options: &MatchOptions) -> Result<Query, String> {
        let token = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or("Missing search expression")?;
        self.pos += 1;
        match token {
            Token::Not => Ok(Query::Not(Box::new(self.unary(options)?))),
            Token::L => self.group(options),
            Token::Atom(s, literal_from) => {
                if literal_from != Some(0) && self.tokens.get(self.pos) == Some(&Token::L) {
                    if let Some((recursive, kind, scoped)) = relation_scope(&s, options) {
                        self.pos += 1;
                        let query = restrict_type(self.group(&scoped)?, kind);
                        return Ok(Query::Term(Term::Related {
                            recursive,
                            query: Box::new(query),
                        }));
                    }
                    if let Some((scoped, is_dir)) = scoped_options(&s, options) {
                        self.pos += 1;
                        return Ok(restrict_type(self.group(&scoped)?, is_dir));
                    }
                }
                if let Some(value) = (literal_from != Some(0))
                    .then(|| s.strip_suffix(':').and_then(|key| self.macros.get(key)))
                    .flatten()
                {
                    // Parse macro tokens with the inherited group scope, without textual substitution.
                    if self.depth >= 16 {
                        return Err("Macro expansion exceeds 16 levels (possible cycle)".into());
                    }
                    let mut parser = Parser {
                        tokens: lex(value)?,
                        pos: 0,
                        macros: self.macros,
                        depth: self.depth + 1,
                        nesting: self.nesting,
                        remaining: self.remaining.clone(),
                    };
                    if parser.tokens.is_empty() {
                        return Ok(Query::All);
                    }
                    let query = parser.and(options)?;
                    if parser.pos != parser.tokens.len() {
                        return Err("Unexpected closing group in macro".into());
                    }
                    return Ok(query);
                }
                atom_with_options(&s, options, literal_from)
            }
            _ => Err("Expected a search expression".into()),
        }
    }
}
fn wildcard_pattern(text: &str) -> String {
    let mut p = String::new();
    for c in text.chars() {
        match c {
            '*' => p.push_str("[^/]*"),
            '?' => p.push_str("[^/]"),
            c => {
                if ".+()[]{}^$|\\".contains(c) {
                    p.push('\\');
                }
                p.push(c);
            }
        }
    }
    format!("\\A(?:{p})\\z")
}
fn pattern(text: &str, sensitive: bool) -> Result<Regex, String> {
    // PCRE2 directives bound pathological backtracking and nested-pattern memory use.
    RegexBuilder::new()
        .utf(true)
        .ucp(true)
        .caseless(!sensitive)
        .jit_if_available(true)
        .build(&format!("(*LIMIT_MATCH=100000)(*LIMIT_DEPTH=1000){text}"))
        .map_err(|error| format!("Invalid PCRE2 pattern: {error}"))
}
fn atom_with_options(
    original: &str,
    inherited: &MatchOptions,
    literal_from: Option<usize>,
) -> Result<Query, String> {
    let mut s = original;
    let mut options = inherited.clone();
    let mut is_dir = None;
    loop {
        if literal_from.is_some_and(|offset| original.len() - s.len() >= offset) {
            break;
        }
        let Some((key, val)) = s.split_once(':') else {
            break;
        };
        if let Some((recursive, kind)) = relation_kind(&keyword(key)) {
            let inner = restrict_type(
                text_query(val, &options, TextTarget::Field(options.field))?,
                kind,
            );
            return Ok(restrict_type(
                Query::Term(Term::Related {
                    recursive,
                    query: Box::new(inner),
                }),
                is_dir,
            ));
        }
        if modifier(&mut options, key) {
            s = val;
            continue;
        }
        match keyword(key).as_str() {
            "file" => {
                is_dir = Some(false);
                s = val;
            }
            "folder" | "dir" => {
                is_dir = Some(true);
                s = val;
            }
            "extension" => {
                options.target = Some(TextTarget::Extension);
                s = val;
                break;
            }
            "ext" if options.regex => {
                options.target = Some(TextTarget::Extension);
                options.whole = true;
                s = val;
                break;
            }
            "content" => {
                options.target = Some(TextTarget::Content);
                s = val;
                break;
            }
            key if text_property(key) => {
                options.target = Some(TextTarget::Property(keyword(key)));
                s = val;
                break;
            }
            _ => break,
        }
    }
    if let Some(is_dir) = is_dir.filter(|_| s.is_empty()) {
        return Ok(Query::Term(Term::IsDir(is_dir)));
    }
    if options.target.is_none()
        && !options.regex
        && !literal_from.is_some_and(|offset| original.len() - s.len() >= offset)
    {
        if let Some((key, val)) = s.split_once(':') {
            let key = keyword(key);
            let term = match key.as_str() {
                "ext" | "extension" if !options.sensitive && !options.diacritics => Term::Extension(val.split(';').map(|s| fold_search(s.trim_start_matches('.'))).collect()),
                "ext" | "extension" => {
                    let queries = val.split(';').map(|v| { let mut o = options.clone(); o.whole = true;
                        text_query(v.trim_start_matches('.'), &o, TextTarget::Extension)
                    }).collect::<Result<Vec<_>, _>>()?;
                    return Ok(restrict_type(Query::Or(queries), is_dir));
                },
                "type" => match val.to_ascii_lowercase().as_str() { "file" => Term::IsDir(false), "folder" | "directory" => Term::IsDir(true), "symlink" | "link" => Term::IsSymlink, _ => return Err("type: accepts file, folder, or symlink".into()) },
                "hidden" if val.is_empty() => Term::Hidden, "symlink" if val.is_empty() => Term::IsSymlink,
                "empty" if val.is_empty() => number_term("childcount", "=0", false, options.now)?,
                "size" => number_term(&key, val, false, options.now)?,
                key if numeric_property(key) || crate::relations::is_metric(key) => number_term(key, val, false, options.now)?,
                "dm" | "datemodified" | "modified" => number_term("modified", val, true, options.now)?,
                "dc" | "datecreated" | "created" => number_term("created", val, true, options.now)?,
                "attrib" | "attributes" => {
                    let terms = val.chars().map(|c| match c.to_ascii_lowercase() {
                        'h' => Ok(Query::Term(Term::Hidden)), 'l' => Ok(Query::Term(Term::IsSymlink)),
                        _ => Err("attrib: supports macOS hidden (h) and symlink (l); use flags: for native flags".to_string()),
                    }).collect::<Result<Vec<_>, _>>()?;
                    if terms.is_empty() { return Err("attrib: requires at least one attribute".into()); }
                    return Ok(restrict_type(Query::And(terms), is_dir));
                },
                "flags" => {
                    let mut flags = 0;
                    for name in val.split(';') {
                        flags |= match keyword(name).as_str() {
                            "hidden" => 0x8000, "immutable" => 0x0002 | 0x0002_0000,
                            "append" => 0x0004 | 0x0004_0000, "nodump" => 0x0001,
                            "compressed" => 0x0020, "dataless" | "placeholder" => 0x4000_0000,
                            _ => return Err(format!("Unsupported native flag '{name}'")),
                        };
                    }
                    Term::Flags(flags)
                },
                _ => return Err(format!("Unsupported search function '{key}:' (Windows-only properties are not silently ignored)")),
            };
            return Ok(restrict_type(Query::Term(term), is_dir));
        }
    }
    if options.field == Field::Parent && options.whole && s.len() > 1 {
        s = s.trim_end_matches('/');
    }
    if options.field == Field::Name && s.contains('/') && options.target.is_none() {
        options.field = Field::Path;
    }
    let target = options
        .target
        .clone()
        .unwrap_or(TextTarget::Field(options.field));
    Ok(restrict_type(text_query(s, &options, target)?, is_dir))
}
fn text_query(s: &str, options: &MatchOptions, target: TextTarget) -> Result<Query, String> {
    let mut scoped_options = options.clone();
    scoped_options.target = Some(target.clone());
    let options = &scoped_options;
    let plain = !options.regex
        && !options.diacritics
        && !options.sensitive
        && !options.prefix
        && !options.suffix
        && !options.whole
        && !options.start
        && !options.end;
    let wildcard = !options.regex
        && options.wildcards
        && (s.contains('*') || s.contains('?'))
        && !matches!(target, TextTarget::Content);
    if plain && !wildcard {
        let needle = fold_search(s);
        let term = match target {
            TextTarget::Field(field) => Term::Text {
                field,
                finder: Finder::new(needle.as_bytes()).into_owned(),
                needle,
                sensitive: options.sensitive,
            },
            TextTarget::Content => Term::Content {
                needle,
                sensitive: options.sensitive,
            },
            TextTarget::Property(ref key) if !options.sensitive => {
                Term::PropertyText(key.clone(), needle)
            }
            _ => {
                return Ok(Query::Term(Term::Matched {
                    target,
                    matcher: TextMatcher::new(s, options, false)?,
                }))
            }
        };
        return Ok(Query::Term(term));
    }
    Ok(Query::Term(Term::Matched {
        target,
        matcher: TextMatcher::new(s, options, wildcard)?,
    }))
}
fn normalized(s: &str, options: &MatchOptions) -> String {
    if s.is_ascii() {
        return if options.sensitive {
            s.to_owned()
        } else {
            s.to_ascii_lowercase()
        };
    }
    let s = if options.diacritics {
        nfc(s)
    } else {
        s.nfd().filter(|c| !is_combining_mark(*c)).collect()
    };
    if options.sensitive {
        s
    } else {
        fold(&s)
    }
}
impl TextMatcher {
    fn new(s: &str, options: &MatchOptions, wildcard: bool) -> Result<Self, String> {
        let regex = if options.regex || wildcard {
            let mut source = if options.diacritics {
                nfc(s)
            } else {
                s.nfd().filter(|c| !is_combining_mark(*c)).collect()
            };
            if wildcard {
                source = wildcard_pattern(&source);
            }
            let word_class = if matches!(options.target, Some(TextTarget::Content)) {
                "\\p{L}\\p{N}\\p{M}_"
            } else {
                "\\p{L}\\p{N}\\p{M}"
            };
            if options.whole {
                source = format!("\\A(?:{source})\\z");
            } else {
                if options.start {
                    source = format!("\\A(?:{source})");
                }
                if options.end {
                    source = format!("(?:{source})\\z");
                }
                if options.prefix {
                    source = format!("(?<![{word_class}])(?:{source})");
                }
                if options.suffix {
                    source = format!("(?:{source})(?![{word_class}])");
                }
            }
            let flags = format!(
                "{}{}",
                if options.dot_all { "(?s)" } else { "" },
                if options.multiline { "(?m)" } else { "" }
            );
            Some(pattern(&format!("{flags}{source}"), options.sensitive)?)
        } else {
            None
        };
        let needle = normalized(s, options);
        Ok(Self {
            finder: Box::new(Finder::new(needle.as_bytes()).into_owned()),
            needle,
            candidate_needle: fold_search(s),
            regex,
            options: options.clone(),
        })
    }
    fn matches(&self, haystack: &str) -> Result<bool, String> {
        // ASCII is already NFC and has no removable diacritics. PCRE handles
        // case folding itself; sensitive literals also need no transformation.
        if haystack.is_ascii() {
            if let Some(regex) = &self.regex {
                return regex
                    .is_match(haystack.as_bytes())
                    .map_err(|error| format!("PCRE2 matching failed: {error}"));
            }
            if self.options.sensitive {
                return Ok(self.matches_normalized(haystack));
            }
        }
        // PCRE uses Unicode case matching itself; never case-fold regex source or text.
        let haystack = if self.regex.is_some() {
            if self.options.diacritics {
                nfc(haystack)
            } else {
                haystack.nfd().filter(|c| !is_combining_mark(*c)).collect()
            }
        } else {
            normalized(haystack, &self.options)
        };
        if let Some(regex) = &self.regex {
            return regex
                .is_match(haystack.as_bytes())
                .map_err(|error| format!("PCRE2 matching failed: {error}"));
        }
        Ok(self.matches_normalized(&haystack))
    }
    fn matches_file(&self, file: &IndexedFile, field: Field) -> Result<bool, String> {
        if self.regex.is_none() && !self.options.sensitive {
            if self.options.diacritics {
                return Ok(self.matches_normalized(match field {
                    Field::Name => file.folded_name.as_ref(),
                    Field::Path => file.folded_path.as_ref(),
                    Field::Parent => normalized_parent(file.folded_path.as_ref()),
                }));
            }
            return Ok(match field {
                Field::Name => self.matches_normalized(file.search_name.as_ref()),
                Field::Path => self.matches_normalized(file.search_path.as_ref()),
                Field::Parent => {
                    self.matches_normalized(normalized_parent(file.search_path.as_ref()))
                }
            });
        }
        self.matches(file.text(field))
    }
    fn matches_normalized(&self, haystack: &str) -> bool {
        if self.options.whole {
            return haystack == self.needle;
        }
        let content_word = matches!(self.options.target, Some(TextTarget::Content));
        let word =
            |c: char| c.is_alphanumeric() || (content_word && c == '_') || is_combining_mark(c);
        let mut offset = 0;
        while let Some(relative) = self.finder.find(&haystack.as_bytes()[offset..]) {
            let start = offset + relative;
            let end = start + self.needle.len();
            let boundary = (!self.options.start || start == 0)
                && (!self.options.end || end == haystack.len())
                && (!self.options.prefix
                    || !haystack[..start].chars().next_back().is_some_and(word))
                && (!self.options.suffix || !haystack[end..].chars().next().is_some_and(word));
            if boundary {
                return true;
            }
            let Some(c) = haystack[start..].chars().next() else {
                break;
            };
            offset = start + c.len_utf8();
        }
        false
    }
}
fn normalized_parent(path: &str) -> &str {
    std::path::Path::new(path)
        .parent()
        .and_then(std::path::Path::to_str)
        .unwrap_or("")
}
fn scalar(s: &str) -> Result<f64, String> {
    let n = s.trim().to_ascii_lowercase();
    let split = n.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(n.len());
    let value: f64 = n[..split]
        .parse()
        .map_err(|_| format!("Invalid number '{s}'"))?;
    let multiplier = match &n[split..] {
        "" | "b" => 1.,
        "k" | "kb" | "kib" => 1024.,
        "m" | "mb" | "mib" => 1024_f64.powi(2),
        "g" | "gb" | "gib" => 1024_f64.powi(3),
        "t" | "tb" | "tib" => 1024_f64.powi(4),
        _ => return Err(format!("Unknown unit in '{s}'")),
    };
    if !value.is_finite() || value < 0. {
        return Err("Expected a finite non-negative number".into());
    }
    let result = value * multiplier;
    if !result.is_finite() {
        return Err("Number exceeds supported range".into());
    }
    Ok(result)
}
/// CFCalendar follows the user's regional first-weekday preference (Sunday=1).
fn local_first_weekday() -> u32 {
    #[cfg(target_os = "macos")]
    unsafe {
        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CFCalendarCopyCurrent() -> *const std::ffi::c_void;
            fn CFCalendarGetFirstWeekday(calendar: *const std::ffi::c_void) -> isize;
            fn CFRelease(value: *const std::ffi::c_void);
        }
        let calendar = CFCalendarCopyCurrent();
        if !calendar.is_null() {
            let day = CFCalendarGetFirstWeekday(calendar);
            CFRelease(calendar);
            if (1..=7).contains(&day) {
                return day as u32 - 1;
            }
        }
    }
    1 // Monday when native regional preferences are unavailable.
}
fn calendar_period(
    s: &str,
    today: NaiveDate,
    first_weekday: u32,
) -> Result<(NaiveDate, NaiveDate), String> {
    let make = |y, m, d| {
        NaiveDate::from_ymd_opt(y, m, d).ok_or_else(|| format!("Date '{s}' is out of range"))
    };
    let next_month = |d: NaiveDate| {
        if d.month() == 12 {
            make(d.year() + 1, 1, 1)
        } else {
            make(d.year(), d.month() + 1, 1)
        }
    };
    let month = make(today.year(), today.month(), 1)?;
    let week = today
        - DateDuration::days(
            ((today.weekday().num_days_from_sunday() + 7 - first_weekday % 7) % 7) as i64,
        );
    let period = match s.to_ascii_lowercase().as_str() {
        "today" => (today, today + DateDuration::days(1)),
        "yesterday" => (today - DateDuration::days(1), today),
        "thisweek" => (week, week + DateDuration::days(7)),
        "lastweek" => (week - DateDuration::days(7), week),
        "thismonth" => (month, next_month(month)?),
        "lastmonth" => {
            let d = month - DateDuration::days(1);
            (make(d.year(), d.month(), 1)?, month)
        }
        "thisyear" => (make(today.year(), 1, 1)?, make(today.year() + 1, 1, 1)?),
        "lastyear" => (make(today.year() - 1, 1, 1)?, make(today.year(), 1, 1)?),
        _ => {
            if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) {
                let year = s.parse::<i32>().map_err(|_| "Invalid year")?;
                (make(year, 1, 1)?, make(year + 1, 1, 1)?)
            } else if s.len() == 7 {
                let d = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d")
                    .map_err(|_| format!("Invalid month '{s}'"))?;
                (d, next_month(d)?)
            } else {
                let d=NaiveDate::parse_from_str(s,"%Y-%m-%d").map_err(|_|format!("Invalid date '{s}'; use YYYY-MM-DD or today/yesterday/thisweek/lastweek/thismonth/lastmonth/thisyear/lastyear"))?;
                (d, d.succ_opt().ok_or("Date out of range")?)
            }
        }
    };
    Ok(period)
}
fn midnight<T: TimeZone>(date: NaiveDate, zone: &T) -> Result<i64, String> {
    let base = date.and_hms_opt(0, 0, 0).ok_or("Invalid midnight")?;
    // Some regions change clocks at midnight, and can even skip a calendar day.
    // Select the first representable instant, rather than assuming 86,400 seconds.
    for minute in 0..=1440 {
        if let Some(dt) = zone
            .from_local_datetime(&(base + DateDuration::minutes(minute)))
            .earliest()
        {
            return Ok(dt.timestamp());
        }
    }
    Err("Calendar boundary is not representable in the local timezone".into())
}
/// Relative durations end at the current second. Calendar months use chrono's
/// checked month arithmetic; fixed durations remain exact across DST changes.
fn relative_date_period(
    input: &str,
    now: chrono::DateTime<Local>,
) -> Result<Option<(f64, f64)>, String> {
    let split = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    if split == 0 || split == input.len() {
        return Ok(None);
    }
    let unit = input[split..].to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        "w" | "week" | "weeks" => 604800,
        "month" | "months" | "year" | "years" => 0,
        _ => return Ok(None),
    };
    let count = input[..split]
        .parse::<u32>()
        .map_err(|_| "Relative date duration is out of range")?;
    let start = if multiplier == 0 {
        let months = count
            .checked_mul(if unit.starts_with("year") { 12 } else { 1 })
            .ok_or("Relative date duration is out of range")?;
        now.checked_sub_months(chrono::Months::new(months))
    } else {
        now.checked_sub_signed(DateDuration::seconds(count as i64 * multiplier))
    };
    let start = start.ok_or("Relative date duration is out of range")?;
    Ok(Some((
        start.timestamp() as f64,
        now.timestamp() as f64 + 1.,
    )))
}
fn number_term(
    field: &str,
    input: &str,
    date: bool,
    now: chrono::DateTime<Local>,
) -> Result<Term, String> {
    if matches!(
        input.to_ascii_lowercase().as_str(),
        "unknown" | "!unknown" | "!=unknown"
    ) {
        return Ok(Term::Unknown {
            field: field.into(),
            negate: input.starts_with('!'),
        });
    }
    let negated_exact = input.strip_prefix("!=").map(|v| format!("={v}"));
    let negate = input.starts_with('!');
    let val = negated_exact
        .as_deref()
        .unwrap_or_else(|| input.strip_prefix('!').unwrap_or(input));
    if date {
        let today = now.date_naive();
        let first = local_first_weekday();
        let period = |v: &str| -> Result<(f64, f64), String> {
            if let Some(interval) = relative_date_period(v, now)? {
                return Ok(interval);
            }
            let (a, b) = calendar_period(v, today, first)?;
            Ok((midnight(a, &Local)? as f64, midnight(b, &Local)? as f64))
        };
        let (low, high) = if let Some((a, b)) = val.split_once("..") {
            (period(a)?.0, period(b)?.1)
        } else if let Some(v) = val.strip_prefix(">=") {
            (period(v)?.0, f64::INFINITY)
        } else if let Some(v) = val.strip_prefix("<=") {
            (f64::NEG_INFINITY, period(v)?.1)
        } else if let Some(v) = val.strip_prefix('>') {
            (period(v)?.1, f64::INFINITY)
        } else if let Some(v) = val.strip_prefix('<') {
            (f64::NEG_INFINITY, period(v)?.0)
        } else {
            period(
                val.strip_prefix("==")
                    .or_else(|| val.strip_prefix('='))
                    .unwrap_or(val),
            )?
        };
        if low >= high {
            return Err("Date range lower bound exceeds upper bound".into());
        }
        return Ok(Term::Number {
            field: field.into(),
            low,
            high,
            include_low: true,
            include_high: false,
            negate,
        });
    }
    let (low, high, il, ih) = if let Some((a, b)) = val.split_once("..") {
        (scalar(a)?, scalar(b)?, true, true)
    } else if let Some(v) = val.strip_prefix(">=") {
        (scalar(v)?, f64::INFINITY, true, true)
    } else if let Some(v) = val.strip_prefix("<=") {
        (f64::NEG_INFINITY, scalar(v)?, true, true)
    } else if let Some(v) = val.strip_prefix('>') {
        (scalar(v)?, f64::INFINITY, false, true)
    } else if let Some(v) = val.strip_prefix('<') {
        (f64::NEG_INFINITY, scalar(v)?, true, false)
    } else if let Some(v) = val.strip_prefix("==").or_else(|| val.strip_prefix('=')) {
        let n = scalar(v)?;
        (n, n, true, true)
    } else {
        let n = scalar(val)?;
        let lower = val.to_ascii_lowercase();
        let split = lower
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(lower.len());
        let decimals = lower[..split]
            .split_once('.')
            .map(|(_, d)| d.len())
            .unwrap_or(0);
        let unit = if split < lower.len() {
            scalar(&format!("1{}", &lower[split..]))?
        } else {
            1.
        };
        let step = unit * 10_f64.powi(-(decimals as i32));
        if step <= 0. {
            return Err("Numeric precision exceeds supported range".into());
        }
        (n, n + step, true, false)
    };
    if low > high {
        return Err("Range lower bound exceeds upper bound".into());
    }
    Ok(Term::Number {
        field: field.into(),
        low,
        high,
        include_low: il,
        include_high: ih,
        negate,
    })
}

impl Query {
    pub(crate) fn requires_relations(&self) -> bool {
        match self {
            Self::Term(Term::Related { .. } | Term::Resolved(_)) => true,
            Self::Term(Term::Number { field, .. } | Term::Unknown { field, .. }) => {
                crate::relations::is_metric(field)
            }
            Self::And(children) | Self::Or(children) => {
                children.iter().any(Self::requires_relations)
            }
            Self::Not(child) => child.requires_relations(),
            _ => false,
        }
    }
    /// Snapshot relations must distinguish unknown descendants from absence.
    /// This evaluator only consults already indexed properties/content.
    pub(crate) fn indexed_truth(
        &self,
        file: &IndexedFile,
        content: Option<&str>,
    ) -> Result<Option<bool>, String> {
        match self {
            Self::And(children) | Self::Or(children) => {
                let and = matches!(self, Self::And(_));
                let mut unknown = false;
                for child in children {
                    match child.indexed_truth(file, content)? {
                        Some(value) if value != and => return Ok(Some(!and)),
                        None => unknown = true,
                        _ => (),
                    }
                }
                Ok((!unknown).then_some(and))
            }
            Self::Not(child) => Ok(child.indexed_truth(file, content)?.map(|value| !value)),
            Self::Term(Term::Resolved(result)) => Ok(result.truth(file)),
            Self::Term(Term::Related { .. }) => {
                Err("Relationship predicates require a search snapshot".into())
            }
            Self::Term(
                Term::PropertyText(key, _)
                | Term::Matched {
                    target: TextTarget::Property(key),
                    ..
                },
            ) if property(file, key).is_none() => Ok(None),
            Self::Term(Term::Number { field, .. })
                if numeric_property(field)
                    && property(file, field).and_then(Value::as_f64).is_none() =>
            {
                Ok(None)
            }
            _ if content.is_none() && self.requires_content() => Ok(None),
            _ => self
                .matches_inner(file, content, &OnceCell::new())
                .map(Some),
        }
    }
    /// Conservative cache hint: date predicates may contain relative periods.
    /// The parser has already resolved macros and date operands into ranges, so
    /// absolute dates are included too. False positives only shorten cache reuse.
    pub fn may_depend_on_clock(&self) -> bool {
        match self {
            Self::Term(Term::Related { query, .. }) => query.may_depend_on_clock(),
            Self::Term(Term::Number { field, .. }) => {
                matches!(field.as_str(), "modified" | "created")
            }
            Self::And(queries) | Self::Or(queries) => queries.iter().any(Self::may_depend_on_clock),
            Self::Not(query) => query.may_depend_on_clock(),
            _ => false,
        }
    }

    pub fn requires_properties(&self) -> bool {
        match self {
            Self::Term(Term::Related { query, .. }) => query.requires_properties(),
            Self::Term(Term::PropertyText(..))
            | Self::Term(Term::Matched {
                target: TextTarget::Property(_),
                ..
            }) => true,
            Self::Term(Term::Number { field, .. }) | Self::Term(Term::Unknown { field, .. }) => {
                numeric_property(field)
            }
            Self::And(queries) | Self::Or(queries) => queries.iter().any(Self::requires_properties),
            Self::Not(query) => query.requires_properties(),
            _ => false,
        }
    }
    pub fn may_match_before_extraction(&self, file: &IndexedFile) -> Result<bool, String> {
        Ok(self.extraction_truth(file)? != Some(false))
    }
    fn extraction_truth(&self, file: &IndexedFile) -> Result<Option<bool>, String> {
        match self {
            Self::Term(Term::Related { .. }) => Ok(None),
            Self::Term(Term::Resolved(result)) => Ok(result.truth(file)),
            Self::Term(Term::Content { .. })
            | Self::Term(Term::PropertyText(..))
            | Self::Term(Term::Matched {
                target: TextTarget::Content | TextTarget::Property(_),
                ..
            }) => Ok(None),
            Self::Term(Term::Number { field, .. }) | Self::Term(Term::Unknown { field, .. })
                if numeric_property(field) =>
            {
                Ok(None)
            }
            Self::And(queries) => {
                let mut unknown = false;
                for query in queries {
                    match query.extraction_truth(file)? {
                        Some(false) => return Ok(Some(false)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown { None } else { Some(true) })
            }
            Self::Or(queries) => {
                let mut unknown = false;
                for query in queries {
                    match query.extraction_truth(file)? {
                        Some(true) => return Ok(Some(true)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown { None } else { Some(false) })
            }
            Self::Not(query) => Ok(query.extraction_truth(file)?.map(|v| !v)),
            _ => Ok(Some(self.matches(file, None)?)),
        }
    }
    pub fn may_match_without_content(&self, file: &IndexedFile) -> Result<bool, String> {
        Ok(self.metadata_truth(file)? != Some(false))
    }
    fn metadata_truth(&self, file: &IndexedFile) -> Result<Option<bool>, String> {
        match self {
            Self::Term(Term::Related { .. }) => Ok(None),
            Self::Term(Term::Resolved(result)) => Ok(result.truth(file)),
            Self::All => Ok(Some(true)),
            Self::Term(Term::Content { .. })
            | Self::Term(Term::Matched {
                target: TextTarget::Content,
                ..
            }) => Ok(None),
            Self::Term(term) => Ok(Some(term.matches(file, None)?)),
            Self::Not(query) => Ok(query.metadata_truth(file)?.map(|v| !v)),
            Self::And(queries) => {
                let mut unknown = false;
                for query in queries {
                    match query.metadata_truth(file)? {
                        Some(false) => return Ok(Some(false)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown { None } else { Some(true) })
            }
            Self::Or(queries) => {
                let mut unknown = false;
                for query in queries {
                    match query.metadata_truth(file)? {
                        Some(true) => return Ok(Some(true)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown { None } else { Some(false) })
            }
        }
    }

    pub fn requires_content(&self) -> bool {
        match self {
            Self::Term(Term::Related { query, .. }) => query.requires_content(),
            Self::Term(Term::Content { .. })
            | Self::Term(Term::Matched {
                target: TextTarget::Content,
                ..
            }) => true,
            Self::And(queries) | Self::Or(queries) => queries.iter().any(Self::requires_content),
            Self::Not(query) => query.requires_content(),
            _ => false,
        }
    }
    pub fn matches_available(
        &self,
        file: &IndexedFile,
        content: Option<&str>,
    ) -> Result<bool, String> {
        if content.is_none() && self.requires_content() {
            return Ok(self.metadata_truth(file)? == Some(true));
        }
        self.matches(file, content)
    }
    pub fn matches(&self, file: &IndexedFile, content: Option<&str>) -> Result<bool, String> {
        if self.requires_relations() {
            return Ok(self.indexed_truth(file, content)? == Some(true));
        }
        self.matches_inner(file, content, &OnceCell::new())
    }
    fn matches_inner(
        &self,
        file: &IndexedFile,
        content: Option<&str>,
        folded_content: &OnceCell<String>,
    ) -> Result<bool, String> {
        match self {
            Self::All => Ok(true),
            Self::And(queries) => {
                for query in queries {
                    if !query.matches_inner(file, content, folded_content)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Or(queries) => {
                for query in queries {
                    if query.matches_inner(file, content, folded_content)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Self::Not(query) => Ok(!query.matches_inner(file, content, folded_content)?),
            Self::Term(Term::Content {
                needle,
                sensitive: false,
            }) => Ok(content.is_some_and(|body| {
                folded_content
                    .get_or_init(|| fold_search(body))
                    .contains(needle)
            })),
            Self::Term(Term::Matched {
                target: TextTarget::Content,
                matcher,
            }) if matcher.regex.is_none()
                && !matcher.options.sensitive
                && !matcher.options.diacritics =>
            {
                Ok(content.is_some_and(|body| {
                    matcher.matches_normalized(folded_content.get_or_init(|| fold_search(body)))
                }))
            }
            Self::Term(term) => term.matches(file, content),
        }
    }
    // Required positive name literals are safe trigram accelerators; disjunction/negation are never narrowed.
    pub fn required_name_literals<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Term(Term::Text {
                field: Field::Name,
                needle,
                sensitive: false,
                ..
            }) if needle.len() >= 3 => out.push(needle),
            Self::Term(Term::Matched {
                target: TextTarget::Field(Field::Name),
                matcher,
            }) if matcher.regex.is_none() && matcher.candidate_needle.len() >= 3 => {
                out.push(&matcher.candidate_needle)
            }
            Self::And(queries) => {
                for query in queries {
                    query.required_name_literals(out)
                }
            }
            _ => {}
        }
    }
}
impl Term {
    pub(crate) fn matches_numeric_value(&self, value: Option<f64>) -> bool {
        match self {
            Self::Unknown { negate, .. } => value.is_none() != *negate,
            Self::Number {
                low,
                high,
                include_low,
                include_high,
                negate,
                ..
            } => value.is_some_and(|n| {
                let inside = (if *include_low { n >= *low } else { n > *low })
                    && (if *include_high { n <= *high } else { n < *high });
                inside != *negate
            }),
            _ => false,
        }
    }
    fn matches(&self, file: &IndexedFile, content: Option<&str>) -> Result<bool, String> {
        Ok(match self {
            Self::Related { .. } => {
                return Err("Relationship predicates require a search snapshot".into())
            }
            Self::Resolved(result) => result.truth(file) == Some(true),
            Self::Flags(mask) => file.flags & mask != 0,
            Self::Text {
                field,
                finder,
                sensitive,
                ..
            } => {
                if *sensitive {
                    finder.find(nfc(file.text(*field)).as_bytes()).is_some()
                } else {
                    match field {
                        Field::Name => finder.find(file.search_name.as_bytes()).is_some(),
                        Field::Path => finder.find(file.search_path.as_bytes()).is_some(),
                        Field::Parent => finder
                            .find(normalized_parent(file.search_path.as_ref()).as_bytes())
                            .is_some(),
                    }
                }
            }
            Self::Pattern { field, regex } => regex
                .is_match(nfc(file.text(*field)).as_bytes())
                .map_err(|error| format!("PCRE2 matching failed: {error}"))?,
            Self::Matched { target, matcher } => match target {
                TextTarget::Field(field) => matcher.matches_file(file, *field)?,
                TextTarget::Content => match content {
                    Some(s) => matcher.matches(s)?,
                    None => false,
                },
                TextTarget::Property(key) => match property(file, key).and_then(Value::as_str) {
                    Some(s) => matcher.matches(s)?,
                    None => false,
                },
                TextTarget::Extension => matcher.matches(&file.extension)?,
            },
            Self::Extension(extensions) => {
                if file.folded_extension.is_ascii() {
                    extensions.contains(&file.folded_extension)
                } else {
                    let extension = fold_search(&file.extension);
                    extensions.contains(&extension)
                }
            }
            Self::IsDir(dir) => file.is_dir == *dir,
            Self::IsSymlink => file.is_symlink,
            Self::Hidden => file.name.starts_with('.') || file.flags & 0x8000 != 0,
            Self::Content { needle, sensitive } => content.is_some_and(|s| {
                (if *sensitive { nfc(s) } else { fold_search(s) }).contains(needle)
            }),
            Self::PropertyText(key, needle) => property(file, key)
                .and_then(Value::as_str)
                .is_some_and(|s| fold_search(s).contains(needle)),
            Self::Unknown { field, negate } => {
                let unknown = match field.as_str() {
                    "size" => file.is_dir,
                    "modified" | "created" => false,
                    other => file.properties.get(other).and_then(Value::as_f64).is_none(),
                };
                if *negate {
                    !unknown
                } else {
                    unknown
                }
            }
            Self::Number {
                field,
                low,
                high,
                include_low,
                include_high,
                negate,
            } => {
                let n = match field.as_str() {
                    "size" => {
                        if file.is_dir {
                            None
                        } else {
                            Some(file.size as f64)
                        }
                    }
                    "modified" => Some(file.modified as f64),
                    "created" => Some(file.created as f64),
                    other => file.properties.get(other).and_then(Value::as_f64),
                };
                n.is_some_and(|n| {
                    let inside = (if *include_low { n >= *low } else { n > *low })
                        && (if *include_high { n <= *high } else { n < *high });
                    if *negate {
                        !inside
                    } else {
                        inside
                    }
                })
            }
        })
    }
}

/// Human numeric ordering without integer overflow, then deterministic path/id tie breaking at caller.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    natural_cmp_folded(&fold(a), &fold(b))
}
pub fn natural_cmp_folded(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // UTF-8 preserves Unicode scalar ordering lexicographically. ASCII digits
    // cannot occur inside a multibyte scalar, so byte slices preserve the old
    // char-based comparator while avoiding temporary Strings for digit runs.
    let a = a.as_bytes();
    let b = b.as_bytes();
    let (mut left, mut right) = (0, 0);
    while left < a.len() && right < b.len() {
        if a[left].is_ascii_digit() && b[right].is_ascii_digit() {
            while left < a.len() && a[left] == b'0' {
                left += 1;
            }
            while right < b.len() && b[right] == b'0' {
                right += 1;
            }
            let (left_start, right_start) = (left, right);
            while left < a.len() && a[left].is_ascii_digit() {
                left += 1;
            }
            while right < b.len() && b[right].is_ascii_digit() {
                right += 1;
            }
            let order = (left - left_start)
                .cmp(&(right - right_start))
                .then_with(|| a[left_start..left].cmp(&b[right_start..right]));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            let order = a[left].cmp(&b[right]);
            if order != Ordering::Equal {
                return order;
            }
            left += 1;
            right += 1;
        }
    }
    (a.len() - left).cmp(&(b.len() - right))
}

#[cfg(test)]
mod date_tests {
    use super::*;
    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }
    #[test]
    fn relative_periods_follow_calendar_boundaries() {
        let today = d(2024, 3, 1);
        for (keyword, start, end) in [
            ("today", d(2024, 3, 1), d(2024, 3, 2)),
            ("yesterday", d(2024, 2, 29), d(2024, 3, 1)),
            ("thisweek", d(2024, 2, 26), d(2024, 3, 4)),
            ("lastweek", d(2024, 2, 19), d(2024, 2, 26)),
            ("thismonth", d(2024, 3, 1), d(2024, 4, 1)),
            ("lastmonth", d(2024, 2, 1), d(2024, 3, 1)),
            ("thisyear", d(2024, 1, 1), d(2025, 1, 1)),
            ("lastyear", d(2023, 1, 1), d(2024, 1, 1)),
        ] {
            assert_eq!(
                calendar_period(keyword, today, 1).unwrap(),
                (start, end),
                "{keyword}"
            )
        }
        assert_eq!(
            calendar_period("thisweek", today, 0).unwrap(),
            (d(2024, 2, 25), d(2024, 3, 3))
        );
        assert_eq!(
            calendar_period("lastmonth", d(2024, 1, 1), 1).unwrap(),
            (d(2023, 12, 1), d(2024, 1, 1))
        );
        assert_eq!(
            calendar_period("2024-02", today, 1).unwrap(),
            (d(2024, 2, 1), d(2024, 3, 1))
        );
        assert_eq!(
            calendar_period("2024", today, 1).unwrap(),
            (d(2024, 1, 1), d(2025, 1, 1))
        );
    }
    #[test]
    fn midnight_intervals_account_for_dst() {
        let zone = chrono_tz::America::New_York;
        assert_eq!(
            midnight(d(2024, 3, 11), &zone).unwrap() - midnight(d(2024, 3, 10), &zone).unwrap(),
            23 * 3600
        );
        assert_eq!(
            midnight(d(2024, 11, 4), &zone).unwrap() - midnight(d(2024, 11, 3), &zone).unwrap(),
            25 * 3600
        );
    }
    #[test]
    fn natural_comparison_handles_big_numbers_and_unicode() {
        assert!(natural_cmp("report2.txt", "report10.txt").is_lt());
        assert!(natural_cmp(
            "项目9999999999999999999999999",
            "项目10000000000000000000000000"
        )
        .is_lt());
        assert!(natural_cmp("café2", "cafe\u{301}10").is_lt());
    }
}
