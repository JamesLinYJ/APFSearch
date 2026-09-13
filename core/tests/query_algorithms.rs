use filesearch_core::{
    index_store::{IndexedFile, SearchSnapshot},
    query::{self, Query},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};
fn entry(name: &str, id: i64) -> IndexedFile {
    let mut e: IndexedFile = serde_json::from_value(json!({"id":id,"path":format!("/algorithm-fixture/{name}"),"name":name,"extension":Path::new(name).extension().unwrap_or_default().to_string_lossy(),"size":2048,"modified":1726185600_i64,"created":1726099200_i64,"changed":1726185600_i64,"is_dir":false,"is_symlink":false,"file_id":id,"parent_id":3,"volume_id":"APFS-test","flags":0,"properties":{"title":"alpha"}})).unwrap();
    e.prepare();
    e
}
fn parse(s: &str) -> Query {
    query::parse(s, &HashMap::new()).unwrap_or_else(|e| panic!("{s}: {e}"))
}
#[test]
fn fixed_windows_reference_modifiers_groups_and_content() {
    let fixture: Value = serde_json::from_str(include_str!(
        "fixtures/everything-1.5.0.1423b-algorithms.json"
    ))
    .unwrap();
    let files = fixture["files"].as_object().unwrap();
    let mut results = Vec::new();
    for row in fixture["results"].as_array().unwrap() {
        let text = row["query"].as_str().unwrap();
        // Unknown functions are deliberately diagnosed, rather than silently returning no matches.
        if text == "unknown:foo" {
            assert!(query::parse(text, &HashMap::new()).is_err());
            continue;
        }
        let q = parse(text);
        let expected: BTreeSet<_> = row["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_owned())
            .collect();
        let actual: BTreeSet<_> = files
            .iter()
            .enumerate()
            .filter_map(|(i, (name, body))| {
                q.matches(&entry(name, i as i64), body.as_str())
                    .unwrap()
                    .then_some(name.clone())
            })
            .collect();
        assert_eq!(actual, expected, "{text}");
        results.push(json!({"query":text,"passed":true,"expected":expected,"actual":actual}));
    }
    if let Ok(path) = std::env::var("APF_QUERY_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&json!({"reference":fixture["reference"],"boundary":"Rust query matching over identical file names and text; reference collected from isolated Windows instance","passed":results.len(),"total":results.len(),"results":results,"differences":["Unknown functions return an explicit parser error.","APFS text is canonical NFC normalized, including when explicit diacritics matching is enabled."]})).unwrap()).unwrap();
    }
}
#[test]
fn substring_candidates_are_a_superset_for_unicode_and_boolean_combinations() {
    let names = [
        "café.txt",
        "cafe\u{301}.txt",
        "CAFÉ.txt",
        "Straße.txt",
        "文档报告.txt",
        "e\u{301}.txt",
        "alpha.txt",
        "missing.pdf",
        "🇨🇳.txt",
    ];
    let snapshot = SearchSnapshot::new(
        names
            .iter()
            .enumerate()
            .map(|(i, n)| entry(n, i as i64))
            .collect(),
        1,
    );
    for text in [
        "cafe",
        "CAFÉ",
        "é",
        "e",
        "文",
        "报告",
        "ss",
        "strasse",
        "🇨🇳",
        "cafe | missing",
        "!cafe",
        "<cafe|alpha> !ext:pdf",
        "cafe | alpha ext:txt",
        "!<cafe|alpha>",
        "diacritics:cafe",
        "case:cafe",
        "ww:cafe",
    ] {
        let q = parse(text);
        let candidates = snapshot.candidates(&q);
        for (i, e) in snapshot.entries.iter().enumerate() {
            if q.matches(e, None).unwrap() {
                assert!(
                    candidates.as_ref().is_none_or(|v| v.contains(i as u32)),
                    "candidate omitted {} for {text}",
                    e.name
                );
            }
        }
    }
    assert!(parse("cafe").matches(&entry("café.txt", 1), None).unwrap());
    assert!(!parse("diacritics:cafe")
        .matches(&entry("café.txt", 1), None)
        .unwrap());
    assert!(parse("diacritics:café")
        .matches(&entry("cafe\u{301}.txt", 1), None)
        .unwrap());
}
#[test]
fn extraction_prefilters_never_drop_a_possible_match_under_nested_not_or_and() {
    let mut expressions = vec![
        "ext:txt".to_owned(),
        "content:alpha".to_owned(),
        "regex:content:^beta$".to_owned(),
        "title:alpha".to_owned(),
    ];
    let leaves = expressions.clone();
    for a in &leaves {
        for b in &leaves {
            for op in [" ", " | "] {
                expressions.push(format!("<{a}{op}{b}>"));
                expressions.push(format!("!<{a}{op}!{b}>"));
            }
        }
    }
    let base = expressions.clone();
    for a in &base {
        for b in &leaves {
            expressions.push(format!("!<{a} | !{b}>"));
            expressions.push(format!("<{a} !{b}>"));
        }
    }
    for text in expressions {
        let q = parse(&text);
        for name in ["file.txt", "file.pdf"] {
            let mut e = entry(name, 1);
            for title in ["alpha", "beta"] {
                e.properties = json!({"title":title});
                for body in ["alpha", "beta", "alpha beta", ""] {
                    let actual = q.matches(&e, Some(body)).unwrap();
                    if actual {
                        assert!(
                            q.may_match_before_extraction(&e).unwrap(),
                            "extraction omitted {text}"
                        );
                        assert!(
                            q.may_match_without_content(&e).unwrap(),
                            "content omitted {text}"
                        );
                    }
                    if q.matches_available(&e, None).unwrap() {
                        assert!(actual, "unknown content became a false positive: {text}");
                    }
                }
            }
        }
    }
}
#[test]
fn scoped_modifiers_override_locally_and_file_group_keeps_negation_inside() {
    let e = entry("ALPHA beta.txt", 1);
    assert!(parse("case:<ALPHA nocase:BETA>").matches(&e, None).unwrap());
    assert!(!parse("case:<alpha nocase:BETA>").matches(&e, None).unwrap());
    assert!(parse("file:<!missing>").matches(&e, None).unwrap());
    let mut dir = e.clone();
    dir.is_dir = true;
    assert!(!parse("file:<!missing>").matches(&dir, None).unwrap());
    assert!(parse("!file:<missing>").matches(&dir, None).unwrap());
    assert!(parse("content:<alpha !gamma>")
        .matches(&e, Some("ALPHA beta"))
        .unwrap());
    assert!(!parse("content:<alpha !gamma>")
        .matches_available(&e, None)
        .unwrap());
    assert!(parse("regex:gr(a|e)y")
        .matches(&entry("gray.txt", 1), None)
        .unwrap());
    assert!(parse("no-wildcards:?")
        .matches(&entry("why?.txt", 1), None)
        .unwrap());
    assert!(!parse("whole:alpha")
        .matches(&entry("alpha.txt", 1), None)
        .unwrap());
    assert!(parse("CASE:EXT:TXT")
        .matches(&entry("README.TXT", 1), None)
        .unwrap());
    let macros = HashMap::from([("pair".to_string(), "alpha beta".to_string())]);
    assert!(query::parse("content:<pair:>", &macros)
        .unwrap()
        .matches(&e, Some("alpha beta"))
        .unwrap());
}
#[test]
fn malformed_regex_group_and_numeric_boundaries_are_errors_or_precise() {
    for s in ["content:<alpha", "content:<>", "case:<foo>|", "regex:[", "width:1zz", "size:999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999tb"] { assert!(query::parse(s,&HashMap::new()).is_err(),"{s}"); }
    for n in [0, 1023, 1024, 1025, 1535, 1536, 1638, 1639, 2047, 2048] {
        let mut e = entry("size.bin", 1);
        e.size = n;
        for (q, wanted) in [
            ("size:1kb", (1024..2048).contains(&n)),
            ("size:=1kb", n == 1024),
            ("size:!=1kb", n != 1024),
            ("size:!1kb", !(1024..2048).contains(&n)),
            ("size:1.5kb", (1536..1639).contains(&n)),
            ("size:>1kb", n > 1024),
            ("SIZE:>=1kb", n >= 1024),
        ] {
            assert_eq!(parse(q).matches(&e, None).unwrap(), wanted, "{q} size={n}");
        }
    }
    // Wildcard matching is the whole value, including valid filename newlines.
    assert!(parse("name:foo*")
        .matches(&entry("foo\nbar", 1), None)
        .unwrap());
    assert!(!parse("name:foo?")
        .matches(&entry("fooX\n", 1), None)
        .unwrap());
}

#[test]
fn regex_lexer_preserves_escaped_punctuation_and_limits_recursive_work() {
    for (q, name) in [
        (r"regex:\)", "right).txt"),
        (r"regex:[(]", "left(.txt"),
        (r"<regex:^foo$ | whole:bar>", "bar"),
        (r"regex:(?<word>foo)", "foo"),
        (r"regex:(?<=foo)bar", "foobar"),
    ] {
        assert!(parse(q).matches(&entry(name, 1), None).unwrap(), "{q}");
    }
    assert!(query::parse(
        &format!("{}x{}", "<".repeat(200), ">".repeat(200)),
        &HashMap::new()
    )
    .is_err());
    assert!(query::parse(&format!("{}x", "!".repeat(200)), &HashMap::new()).is_err());
    assert!(query::parse(&"x".repeat(65537), &HashMap::new()).is_err());
    let mut macros = HashMap::new();
    macros.insert("level0".to_string(), "x".to_string());
    for i in 1..16 {
        macros.insert(
            format!("level{i}"),
            format!("level{}: level{}:", i - 1, i - 1),
        );
    }
    assert!(query::parse("level15:", &macros).is_err());
}

#[test]
fn parent_scope_unknown_numeric_values_and_relative_dates() {
    let e = entry("file.txt", 1);
    assert!(parse("parent:/algorithm-fixture/")
        .matches(&e, None)
        .unwrap());
    assert!(!parse("parent:/algorithm").matches(&e, None).unwrap());
    assert!(parse("path-part:algorithm").matches(&e, None).unwrap());
    let mut nested = e.clone();
    nested.path = "/algorithm-fixture/sub/file.txt".into();
    nested.prepare();
    assert!(!parse("parent:/algorithm-fixture")
        .matches(&nested, None)
        .unwrap());
    assert!(parse("location:/algorithm-fixture")
        .matches(&nested, None)
        .unwrap());
    assert!(parse("width:unknown").matches(&e, None).unwrap());
    assert!(parse("width:unknown")
        .may_match_before_extraction(&e)
        .unwrap());
    let mut known = e.clone();
    known.properties = json!({"width":0});
    assert!(!parse("width:unknown").matches(&known, None).unwrap());
    assert!(parse("width:!=unknown").matches(&known, None).unwrap());
    assert!(!parse("size:unknown").matches(&e, None).unwrap());
    let mut dir = e.clone();
    dir.is_dir = true;
    assert!(parse("size:unknown").matches(&dir, None).unwrap());
    let mut recent = e.clone();
    recent.modified = chrono::Local::now().timestamp() - 3600;
    assert!(parse("dm:2hours").matches(&recent, None).unwrap());
    assert!(!parse("dm:30mins").matches(&recent, None).unwrap());
    assert!(parse("dm:2days").matches(&recent, None).unwrap());
    assert!(parse("dm:1month").matches(&recent, None).unwrap());
    assert!(parse("dm:1year").matches(&recent, None).unwrap());
    assert!(parse(r#"suffix:" a ""#)
        .matches(&entry(" a a ", 1), None)
        .unwrap());
}

#[test]
fn lease_query_clock_and_extension_property_semantics_are_stable() {
    use chrono::{Duration, Local, TimeZone};
    let now = Local.with_ymd_and_hms(2024, 3, 31, 12, 0, 0).unwrap();
    let mut e = entry("file.jpg2", 1);
    e.modified = (now - Duration::hours(23)).timestamp();
    let q = query::parse_at("dm:1day", &HashMap::new(), now).unwrap();
    assert!(q.matches(&e, None).unwrap());
    assert!(
        !query::parse_at("dm:1day", &HashMap::new(), now + Duration::hours(2))
            .unwrap()
            .matches(&e, None)
            .unwrap()
    );
    assert!(parse("extension:jpg").matches(&e, None).unwrap());
    assert!(!parse("ext:jpg").matches(&e, None).unwrap());
    assert!(parse("regex:extension:^jpg[0-9]$")
        .matches(&e, None)
        .unwrap());
    assert!(parse("ext:cafe")
        .matches(&entry("file.café", 1), None)
        .unwrap());
    assert!(!parse("diacritics:ext:cafe")
        .matches(&entry("file.café", 1), None)
        .unwrap());
}

#[test]
fn quotes_escape_function_names_and_operator_characters() {
    for (q, name) in [
        (r#""size:bad""#, "size:bad.txt"),
        (r#"name:"title:raw""#, "title:raw.txt"),
        (r#""a|b""#, "a|b.txt"),
        (r#""<tag>""#, "<tag>.txt"),
        (r#"case:"a:b""#, "a:b.txt"),
    ] {
        assert!(parse(q).matches(&entry(name, 1), None).unwrap(), "{q}");
    }
    let macros = HashMap::from([("docs".to_string(), "ext:pdf".to_string())]);
    assert!(query::parse(r#""docs:""#, &macros)
        .unwrap()
        .matches(&entry("docs:.txt", 1), None)
        .unwrap());
}
