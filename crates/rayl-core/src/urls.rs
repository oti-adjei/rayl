//! Compacting tracking-heavy URLs out of message bodies.
//!
//! Marketing mail routinely spends more bytes on tracking parameters than on content: a
//! single LinkedIn link observed in development was ~700 bytes, of which the meaningful part
//! was ~60. For an agent reading mail, that is context spent on nothing, and it causes
//! truncation to discard real content in favour of tokens.
//!
//! The approach is a deny list, not an allow list. Query parameters often carry meaning —
//! Cloudflare's invoice links put the destination in `?to=` — so dropping unrecognised
//! parameters would break working links. Only known trackers and implausibly long values go.

/// Parameters that exist solely to identify the reader or the campaign.
const TRACKING_PARAMS: &[&str] = &[
    // Generic campaign tagging
    "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content", "utm_id",
    "utm_name", "utm_cid", "utm_reader", "utm_social", "utm_place",
    // LinkedIn
    "lipi", "licu", "lici", "midtoken", "midsig", "trk", "trkemail", "eid", "otptoken",
    "li_fat_id", "refid",
    // Mailchimp, HubSpot, Marketo, Eloqua, Pardot
    "mc_cid", "mc_eid", "_hsenc", "_hsmi", "hsctatracking", "mkt_tok", "elqtrackid",
    "elqtrack", "pk_campaign", "pk_kwd", "s_cid",
    // Ad-network click ids
    "fbclid", "gclid", "gbraid", "wbraid", "msclkid", "dclid", "twclid", "ttclid",
    "igshid", "yclid", "rb_clickid", "vero_id", "vero_conv",
    // Misc senders
    "ck_subscriber_id", "sc_campaign", "sc_channel", "sc_content", "sc_outcome",
    "sc_geo", "sc_country", "ml_subscriber", "ml_subscriber_hash", "spm", "spmref",
];

/// A parameter value longer than this is treated as an opaque tracking blob. Set high so
/// that genuine tokens — unsubscribe links, signed URLs — survive.
const MAX_PARAM_VALUE: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    pub text: String,
    /// How many URLs were shortened.
    pub urls: usize,
    pub bytes_saved: usize,
}

/// Rewrites every URL in `text`, dropping tracking parameters.
pub fn compact(text: &str) -> Compaction {
    let mut out = String::with_capacity(text.len());
    let mut urls = 0usize;
    let mut bytes_saved = 0usize;
    let mut cursor = 0usize;

    while let Some(start) = find_url(text, cursor) {
        out.push_str(&text[cursor..start]);

        let end = url_end(text, start);
        let raw = &text[start..end];
        let compacted = compact_url(raw);

        if compacted.len() < raw.len() {
            urls += 1;
            bytes_saved += raw.len() - compacted.len();
        }
        out.push_str(&compacted);
        cursor = end;
    }
    out.push_str(&text[cursor..]);

    Compaction { text: out, urls, bytes_saved }
}

/// Byte offset of the next `http://` or `https://`, case-insensitive.
fn find_url(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'h' || bytes[i] == b'H' {
            for scheme in ["https://", "http://"] {
                if bytes
                    .get(i..i + scheme.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(scheme.as_bytes()))
                    && text.is_char_boundary(i)
                {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Where the URL ends: at whitespace or a delimiter, with trailing sentence punctuation
/// excluded so `see https://x.test/y.` does not swallow the full stop.
fn url_end(text: &str, start: usize) -> usize {
    let mut end = text.len();
    for (offset, ch) in text[start..].char_indices() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '|') {
            end = start + offset;
            break;
        }
    }

    // Trim trailing punctuation that is almost certainly prose, not URL.
    let mut trimmed = &text[start..end];
    while let Some(last) = trimmed.chars().last() {
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}') {
            trimmed = &trimmed[..trimmed.len() - last.len_utf8()];
        } else {
            break;
        }
    }

    start + trimmed.len()
}

fn compact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };

    // Preserve any fragment; it is addressing, not tracking.
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };

    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && is_worth_keeping(pair))
        .collect();

    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn is_worth_keeping(pair: &str) -> bool {
    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
    let name = name.trim().to_ascii_lowercase();

    if TRACKING_PARAMS.contains(&name.as_str()) {
        return false;
    }
    // An unrecognised parameter carrying a huge opaque value is a tracker in all but name.
    if value.len() > MAX_PARAM_VALUE {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_linkedin_tracking_but_keeps_the_destination() {
        // Shortened form of a real URL from a LinkedIn job alert.
        let url = "https://www.linkedin.com/comm/company/harwiltechnologies/jobs?lipi=urn%3Ali%3Apage%3Aemail_01&midToken=AQFyktvnBe6IMA&midSig=2ymB_uCwRqoYw1&trk=eml-recent-0-link&trkEmail=eml-recent-0-link-null&eid=cdu7t7-gf&otpToken=MzhlMjIxZjE4NGVj";
        let result = compact(url);

        assert_eq!(
            result.text,
            "https://www.linkedin.com/comm/company/harwiltechnologies/jobs"
        );
        assert_eq!(result.urls, 1);
        assert!(result.bytes_saved > 100);
    }

    #[test]
    fn keeps_a_query_parameter_that_carries_the_destination() {
        // Cloudflare puts the actual target in ?to=; dropping it would break the link.
        let url = "https://dash.cloudflare.com/?to=/9b8d97b4/billing/invoices";
        let result = compact(url);
        assert_eq!(result.text, url, "nothing here is tracking");
        assert_eq!(result.urls, 0);
        assert_eq!(result.bytes_saved, 0);
    }

    #[test]
    fn keeps_meaningful_params_while_dropping_trackers_in_the_same_url() {
        let url = "https://shop.test/item?id=42&utm_source=news&color=blue&gclid=xyz";
        let result = compact(url);
        assert_eq!(result.text, "https://shop.test/item?id=42&color=blue");
    }

    #[test]
    fn drops_implausibly_long_opaque_values() {
        let long = "a".repeat(MAX_PARAM_VALUE + 1);
        let url = format!("https://x.test/p?sig={long}&page=2");
        let result = compact(&url);
        assert_eq!(result.text, "https://x.test/p?page=2");
    }

    #[test]
    fn keeps_a_token_short_enough_to_be_genuine() {
        // Unsubscribe links need their token; only absurdly long values are dropped.
        let url = "https://news.test/unsubscribe?token=abc123def456";
        assert_eq!(compact(url).text, url);
    }

    #[test]
    fn parameter_names_are_matched_case_insensitively() {
        let url = "https://x.test/p?UTM_Source=mail&MidToken=abc&keep=1";
        assert_eq!(compact(url).text, "https://x.test/p?keep=1");
    }

    #[test]
    fn urls_without_a_query_are_untouched() {
        let url = "https://example.test/path/to/page";
        let result = compact(url);
        assert_eq!(result.text, url);
        assert_eq!(result.urls, 0);
    }

    #[test]
    fn preserves_surrounding_prose_and_trailing_punctuation() {
        let text = "Visit https://x.test/p?utm_source=mail. Then reply.";
        let result = compact(text);
        assert_eq!(result.text, "Visit https://x.test/p. Then reply.");
    }

    #[test]
    fn handles_several_urls_in_one_body() {
        let text = "One https://a.test/x?gclid=1 and two https://b.test/y?utm_id=2 done";
        let result = compact(text);
        assert_eq!(result.text, "One https://a.test/x and two https://b.test/y done");
        assert_eq!(result.urls, 2);
    }

    #[test]
    fn keeps_the_fragment() {
        let url = "https://docs.test/guide?utm_source=mail#enable-invoices";
        assert_eq!(compact(url).text, "https://docs.test/guide#enable-invoices");
    }

    #[test]
    fn non_ascii_text_around_urls_is_preserved() {
        // The encoding bugs in the HTML stripper came from exactly this shape.
        let text = "Copyright \u{a9} Acme \u{1f600} https://acme.test/p?utm_source=mail \u{e9}nd";
        let result = compact(text);
        assert_eq!(
            result.text,
            "Copyright \u{a9} Acme \u{1f600} https://acme.test/p \u{e9}nd"
        );
    }

    #[test]
    fn a_url_immediately_after_a_multibyte_character_is_found() {
        let text = "\u{1f600}https://x.test/p?gclid=1";
        assert_eq!(compact(text).text, "\u{1f600}https://x.test/p");
    }

    #[test]
    fn empty_and_scheme_only_input_do_not_panic() {
        assert_eq!(compact("").text, "");
        assert_eq!(compact("https://").text, "https://");
        assert_eq!(compact("no urls here").text, "no urls here");
    }

    #[test]
    fn a_trailing_question_mark_is_prose_not_an_empty_query() {
        // "did you see https://x.test/p?" ends a sentence; the ? is not part of the URL and
        // must survive in the text rather than being absorbed and dropped.
        assert_eq!(compact("http://?").text, "http://?");
        assert_eq!(
            compact("see https://x.test/p?").text,
            "see https://x.test/p?"
        );
        // A ? with parameters after it is a real query and still gets cleaned.
        assert_eq!(
            compact("see https://x.test/p?utm_source=a?").text,
            "see https://x.test/p?"
        );
    }

    #[test]
    fn uppercase_scheme_is_recognised() {
        let text = "HTTPS://X.test/p?utm_source=mail";
        assert_eq!(compact(text).text, "HTTPS://X.test/p");
    }

    #[test]
    fn dropping_every_parameter_removes_the_question_mark() {
        assert_eq!(compact("https://x.test/p?utm_source=a&gclid=b").text, "https://x.test/p");
    }
}
