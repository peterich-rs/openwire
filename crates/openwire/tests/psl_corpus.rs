//! Cookie Domain behavior against the official Public Suffix List test vectors.
//!
//! Source: https://github.com/publicsuffix/list/blob/master/tests/tests.txt (CC0).
//! Expected values come from that file (PSL algorithm, including implicit `*`),
//! not from OpenWire or cookie_store internals.
//!
//! RFC 6265 §5.3: if Domain is a public suffix and is not identical to the
//! request-host, ignore the cookie.

use std::collections::HashMap;

use openwire::{CookieJar, Jar};
use url::Url;

const PSL_TESTS: &str = include_str!("../data/psl_tests.txt");

#[test]
fn official_psl_vectors_drive_cookie_domain_rules() {
    let cases = parse_psl_tests(PSL_TESTS);
    assert!(
        cases.len() >= 60,
        "vendored PSL tests.txt looks empty or truncated ({} cases)",
        cases.len()
    );

    let expected_by_host: HashMap<String, Option<String>> = cases
        .iter()
        .map(|case| (case.input.to_ascii_lowercase(), case.expected.clone()))
        .collect();

    let mut skipped_invalid = 0usize;
    for case in &cases {
        match apply_case(case, &expected_by_host) {
            Ok(()) => {}
            Err(SkipReason::InvalidHost) => skipped_invalid += 1,
        }
    }

    assert!(
        cases.len() - skipped_invalid >= 50,
        "too many PSL cookie cases skipped as invalid hosts: {skipped_invalid} of {}",
        cases.len()
    );
}

struct PslCase {
    input: String,
    expected: Option<String>,
}

enum SkipReason {
    InvalidHost,
}

fn parse_psl_tests(body: &str) -> Vec<PslCase> {
    let mut cases = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(input) = parts.next() else {
            continue;
        };
        let Some(expected) = parts.next() else {
            panic!("PSL test line missing expected column: {line}");
        };
        if input.eq_ignore_ascii_case("null") {
            continue;
        }
        cases.push(PslCase {
            input: input.to_owned(),
            expected: if expected.eq_ignore_ascii_case("null") {
                None
            } else {
                Some(expected.to_owned())
            },
        });
    }
    cases
}

fn apply_case(
    case: &PslCase,
    expected_by_host: &HashMap<String, Option<String>>,
) -> Result<(), SkipReason> {
    match case.expected.as_deref() {
        None => apply_public_suffix(&case.input),
        Some(reg) => apply_registrable(&case.input, reg, expected_by_host),
    }
}

fn apply_public_suffix(input: &str) -> Result<(), SkipReason> {
    if input.starts_with('.') {
        return Err(SkipReason::InvalidHost);
    }

    let child = url_for_host(&format!("leaf.{input}"))?;
    let sibling = url_for_host(&format!("other.{input}"))?;
    let unrelated = url_for_host("unrelated.test")?;

    let jar = Jar::new();
    jar.add_cookie_str(&format!("psl=1; Domain={input}; Path=/"), &child);
    assert!(
        cookie_header(&jar, &sibling).is_none(),
        "PSL tests.txt: {input} has no registrable domain; Domain={input} from {child} must not be visible on {sibling}"
    );
    assert!(
        cookie_header(&jar, &unrelated).is_none(),
        "public suffix Domain={input} must not leak to {unrelated}"
    );

    let exact = url_for_host(input)?;
    let host_only = Jar::new();
    host_only.add_cookie_str(&format!("host=1; Domain={input}; Path=/"), &exact);
    assert!(
        cookie_header(&host_only, &sibling).is_none(),
        "RFC 6265 §5.3: Domain={input} set from exact public-suffix host {exact} must stay host-only"
    );
    Ok(())
}

fn apply_registrable(
    input: &str,
    reg: &str,
    expected_by_host: &HashMap<String, Option<String>>,
) -> Result<(), SkipReason> {
    let setter_host = if input.eq_ignore_ascii_case(reg) {
        format!("www.{reg}")
    } else {
        input.to_owned()
    };
    let setter = url_for_host(&setter_host)?;
    let sibling = url_for_host(&format!("other-label.{reg}"))?;
    let unrelated = url_for_host("unrelated.test")?;

    let jar = Jar::new();
    jar.add_cookie_str(&format!("ok=1; Domain={reg}; Path=/"), &setter);
    let sibling_cookies = cookie_header(&jar, &sibling);
    assert!(
        sibling_cookies
            .as_deref()
            .is_some_and(|value| value.contains("ok=1")),
        "PSL tests.txt: registrable Domain={reg} from {setter} must be visible on {sibling}, got {sibling_cookies:?}"
    );
    assert!(
        cookie_header(&jar, &unrelated).is_none(),
        "Domain={reg} must not leak to {unrelated}"
    );

    if let Some(parent) = parent_domain(reg) {
        let parent_is_public_suffix = matches!(
            expected_by_host.get(&parent.to_ascii_lowercase()),
            Some(None)
        );
        if parent_is_public_suffix {
            let parent_jar = Jar::new();
            parent_jar.add_cookie_str(&format!("bad=1; Domain={parent}; Path=/"), &setter);
            assert!(
                cookie_header(&parent_jar, &sibling).is_none(),
                "PSL tests.txt: {parent} is a public suffix; Domain={parent} from {setter} must not be visible on {sibling}"
            );
        }
    }
    Ok(())
}

fn parent_domain(reg: &str) -> Option<String> {
    let rest = reg.split_once('.')?.1;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

fn url_for_host(host: &str) -> Result<Url, SkipReason> {
    Url::parse(&format!("https://{host}/")).map_err(|_| SkipReason::InvalidHost)
}

fn cookie_header(jar: &Jar, url: &Url) -> Option<String> {
    jar.cookies(url)
        .and_then(|value| value.to_str().ok().map(str::to_owned))
}
