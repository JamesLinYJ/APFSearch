//! Frozen released grammar is a bounded-depth behavior oracle, never a runtime fallback.
use super::*;
mod reference {
    use super::*;
    use std::{cell::Cell, rc::Rc};
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
}

#[test]
fn iterative_parser_preserves_reference_trees_and_errors() {
    let macros = HashMap::from([
        ("docs".into(), "ext:txt | ext:pdf".into()),
        ("scoped".into(), "case:<alpha | beta> !ext:zip".into()),
        ("empty".into(), "".into()),
        ("broken".into(), "alpha >".into()),
        ("cycle".into(), "cycle:".into()),
    ]);
    let now = Local::now();
    let compare = |text: &str| {
        let actual = super::parse_at(text, &macros, now).map(|query| format!("{query:?}"));
        let expected = reference::parse_at(text, &macros, now).map(|query| format!("{query:?}"));
        assert_eq!(actual, expected, "{text}");
    };
    for text in [
        "",
        "alpha | beta gamma",
        "alpha beta | gamma",
        "!docs: empty:",
        "file:<!scoped:>",
        "case:<docs: empty:>",
        "child:<docs: | beta>",
        "ancestor:<case:alpha | beta>",
        "broken:",
        "cycle:",
        "empty: <x>",
        "<>",
        "!",
        "|x",
        "x &",
        "x |",
        "<x",
        "x>",
        "<x |>",
        "docs:<x>",
        r#""docs:""#,
        r"regex:(?<word>foo)",
        r"regex:[(]",
        "path:<alpha | beta>",
        "file:folder:<x>",
        "regex:<alpha | beta>",
        "case:<alpha <nocase:beta>>",
    ] {
        compare(text);
    }
    fn expression(seed: &mut u64, depth: usize) -> String {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let choice = *seed as usize;
        if depth == 0 {
            return [
                "alpha",
                "报告",
                "docs:",
                "empty:",
                "case:Alpha",
                "ext:pdf",
                r#"regex:"a.*b""#,
                r#""literal:term""#,
            ][choice % 8]
                .into();
        }
        let left = expression(seed, depth - 1);
        match choice % 7 {
            0 => format!("!{left}"),
            1 => format!("<{left}>"),
            2 => format!("case:<{left}>"),
            3 => format!("file:<{left}>"),
            4 => format!("{left} | {}", expression(seed, depth - 1)),
            5 => format!("{left} {}", expression(seed, depth - 1)),
            _ => format!("path:<{left}>"),
        }
    }
    let mut seed = 0x8c3a_205d_16e7_9b41;
    for _ in 0..1_024 {
        let text = expression(&mut seed, 5);
        compare(&text);
        compare(&format!("<{text}"));
        compare(&format!("{text} |"));
        compare(&format!("{text}>"));
    }
}

#[test]
fn nested_groups_negations_and_macros_do_not_consume_recursive_stack() {
    // Deliberately SMALLER than the default test/XPC worker stack. The input
    // limit must be enforced even in unoptimized Intel builds.
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let macros = HashMap::new();
            for depth in [1, 16, 64, 127] {
                let groups = format!("{}x{}", "<".repeat(depth), ">".repeat(depth));
                assert!(super::parse(&groups, &macros).is_ok());
                assert!(super::parse(&format!("{}x", "!".repeat(depth)), &macros).is_ok());
            }
            for depth in [128, 200, 8_000] {
                assert!(
                    super::parse(
                        &format!("{}x{}", "<".repeat(depth), ">".repeat(depth)),
                        &macros
                    )
                    .is_err()
                );
                assert!(super::parse(&format!("{}x", "!".repeat(depth)), &macros).is_err());
            }
            let mut macros = HashMap::from([("level0".into(), "x".into())]);
            for level in 1..=16 {
                macros.insert(format!("level{level}"), format!("level{}:", level - 1));
            }
            assert!(super::parse("level15:", &macros).is_ok());
            assert!(super::parse("level16:", &macros).is_err());
            for level in 1..16 {
                macros.insert(
                    format!("level{level}"),
                    format!("level{}: level{}:", level - 1, level - 1),
                );
            }
            assert!(super::parse("level15:", &macros).is_err());
            let siblings = "<!<x | y>> ".repeat(256);
            assert!(super::parse(&siblings, &HashMap::new()).is_ok());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
#[ignore = "Release parser comparison; batch timing is not end-to-end query latency"]
fn parser_latency_profile() {
    use std::{hint::black_box, time::Instant};
    let macros = HashMap::from([("docs".into(), "ext:txt | ext:pdf".into())]);
    let now = Local::now();
    let mut records = Vec::new();
    for (case, text) in [
        "",
        "crossover",
        "报告",
        "path:/Applications",
        "size:>10mb",
        "dm:2020..2025",
        "ext:pdf",
        "alpha | beta !ext:zip",
        "case:<docs: !draft>",
        r"path:regex:^/Applications/[^/]+\.app/Contents/",
        "<<<<<<<<alpha | beta>>>>>>>>",
        "!!!!!!!!alpha",
    ]
    .into_iter()
    .enumerate()
    {
        let mut samples = [Vec::new(), Vec::new()];
        for round in 0..40 {
            for variant in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                let started = Instant::now();
                for _ in 0..200 {
                    let parsed = if variant == 0 {
                        reference::parse_at(black_box(text), &macros, now)
                    } else {
                        super::parse_at(black_box(text), &macros, now)
                    };
                    black_box(parsed.unwrap());
                }
                samples[variant].push(started.elapsed().as_secs_f64() * 1e9 / 200.);
            }
        }
        records.push(
            serde_json::json!({"case":case,"baseline_ns_per_parse":samples[0],
            "candidate_ns_per_parse":samples[1]}),
        );
    }
    println!(
        "{}",
        serde_json::json!({"scope":"Alternating batch averages, same-process released recursive parser vs iterative parser; excludes matching, XPC and UI","cases":records})
    );
}
