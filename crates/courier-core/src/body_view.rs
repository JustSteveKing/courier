//! Making sense of a response body: what kind it is, and reading XML the way JSONPath reads
//! JSON. Pure functions, shared by the app's viewers and anything else that needs them.

use quick_xml::Reader;
use quick_xml::events::Event;
use rust_i18n::t;

/// How a response body is best shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyView {
    Json,
    Xml,
    /// Shown as source, with a button to open it in a browser.
    Html,
    /// The image format, as gpui names them: `png`, `jpeg`, `gif`, `webp`, `svg`, `bmp`, `tiff`.
    Image(&'static str),
    Text,
    /// Not text: shown as hex.
    Binary,
}

impl BodyView {
    /// Decides from the Content-Type, falling back to what the body looks like.
    pub fn of(content_type: Option<&str>, body: &str) -> Self {
        let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
        let mime = content_type.split(';').next().unwrap_or_default().trim();
        if let Some(format) = image_format(mime) {
            return Self::Image(format);
        }
        if mime.contains("json") || mime.ends_with("+json") {
            return Self::Json;
        }
        if mime == "text/html" || mime == "application/xhtml+xml" {
            return Self::Html;
        }
        if mime.contains("xml") {
            return Self::Xml;
        }
        if !mime.is_empty() && !mime.starts_with("text/") && !is_texty(mime) {
            return Self::Binary;
        }
        sniff(body)
    }

    /// The editor language for this kind of body.
    pub fn language(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Xml | Self::Html => "xml",
            Self::Image(_) | Self::Text | Self::Binary => "text",
        }
    }

    /// Whether the filter box applies here, and what it filters with.
    pub fn filter_kind(self) -> Option<&'static str> {
        match self {
            Self::Json => Some("jsonpath"),
            Self::Xml | Self::Html => Some("xpath"),
            _ => None,
        }
    }
}

fn image_format(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" | "image/x-ms-bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => return None,
    })
}

/// Types that are text without saying `text/`.
fn is_texty(mime: &str) -> bool {
    mime.contains("json")
        || mime.contains("xml")
        || mime.contains("javascript")
        || mime.contains("x-www-form-urlencoded")
        || mime.contains("yaml")
        || mime == "application/graphql"
}

/// No usable Content-Type: guess from the body itself.
fn sniff(body: &str) -> BodyView {
    let start = body.trim_start();
    if start.starts_with('{') || start.starts_with('[') {
        return BodyView::Json;
    }
    if start.starts_with("<!doctype html") || start.starts_with("<html") {
        return BodyView::Html;
    }
    if start.starts_with("<?xml") || start.starts_with('<') {
        return BodyView::Xml;
    }
    // A replacement character means the bytes weren't UTF-8 to begin with.
    if body.contains('\u{fffd}') || body.contains('\0') {
        return BodyView::Binary;
    }
    BodyView::Text
}

/// One element of a parsed document.
#[derive(Debug, PartialEq)]
struct Node {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<Node>,
    /// The element's own text, with the whitespace between tags dropped.
    text: String,
}

fn parse(xml: &str) -> Result<Node, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = false;
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    loop {
        match reader.read_event() {
            Err(e) => return Err(t!("body.xml_invalid", error = e.to_string()).to_string()),
            Ok(Event::Eof) => break,
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let empty = matches!(event, Event::Empty(_));
                let start = match &event {
                    Event::Start(start) | Event::Empty(start) => start.clone(),
                    _ => unreachable!(),
                };
                let node = Node {
                    name: name_of(start.name().as_ref()),
                    attributes: start
                        .attributes()
                        .flatten()
                        .map(|a| {
                            (
                                name_of(a.key.as_ref()),
                                a.unescape_value().map(|v| v.into_owned()).unwrap_or_default(),
                            )
                        })
                        .collect(),
                    children: Vec::new(),
                    text: String::new(),
                };
                // `<tag/>` has no end event of its own.
                match (empty, stack.last_mut()) {
                    (true, Some(parent)) => parent.children.push(node),
                    (true, None) => root = Some(node),
                    (false, _) => stack.push(node),
                }
            }
            Ok(Event::End(_)) => {
                if let Some(node) = stack.pop() {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => root = Some(node),
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(node) = stack.last_mut() {
                    let value = text.xml_content().map(|v| v.into_owned()).unwrap_or_default();
                    node.text.push_str(value.trim());
                }
            }
            Ok(Event::CData(data)) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            _ => {}
        }
    }
    // Anything still open (an unclosed tag) closes here, outermost last.
    while let Some(node) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => root = Some(node),
        }
    }
    root.ok_or_else(|| t!("body.xml_empty").to_string())
}

/// Drops any namespace prefix: `soap:Body` is matched as `Body`.
fn name_of(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    name.rsplit(':').next().unwrap_or(&name).to_string()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn write_node(node: &Node, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    out.push_str(&pad);
    out.push('<');
    out.push_str(&node.name);
    for (name, value) in &node.attributes {
        out.push_str(&format!(" {name}=\"{}\"", escape(value)));
    }
    if node.children.is_empty() && node.text.is_empty() {
        out.push_str("/>\n");
        return;
    }
    out.push('>');
    if node.children.is_empty() {
        out.push_str(&escape(&node.text));
        out.push_str(&format!("</{}>\n", node.name));
        return;
    }
    out.push('\n');
    if !node.text.is_empty() {
        out.push_str(&format!("{pad}  {}\n", escape(&node.text)));
    }
    for child in &node.children {
        write_node(child, depth + 1, out);
    }
    out.push_str(&format!("{pad}</{}>\n", node.name));
}

/// Re-indents XML, like pretty-printing JSON. Returns the text unchanged if it doesn't parse.
pub fn pretty_xml(xml: &str) -> String {
    match parse(xml) {
        Ok(root) => {
            let mut out = String::new();
            write_node(&root, 0, &mut out);
            out.trim_end().to_string()
        }
        Err(_) => xml.to_string(),
    }
}

/// What a step of a path selects.
#[derive(Debug, PartialEq)]
enum Step {
    /// A child element by name, or any child for `*`.
    Child {
        name: String,
        index: Option<usize>,
    },
    /// The same, but at any depth below.
    Descendant {
        name: String,
        index: Option<usize>,
    },
    Attribute(String),
    Text,
}

fn parse_path(path: &str) -> Result<Vec<Step>, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(t!("body.xpath_empty").to_string());
    }
    let mut steps = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        let descendant = if let Some(after) = rest.strip_prefix("//") {
            rest = after;
            true
        } else {
            rest = rest.strip_prefix('/').unwrap_or(rest);
            // The first step of a relative path ("book/title") is a descendant search, so
            // "title" finds a title wherever it is.
            steps.is_empty() && !path.starts_with('/')
        };
        let end = rest.find('/').unwrap_or(rest.len());
        let (step, after) = rest.split_at(end);
        rest = after;
        let step = step.trim();
        if step.is_empty() {
            return Err(t!("body.xpath_step_empty").to_string());
        }
        if step == "text()" {
            steps.push(Step::Text);
            continue;
        }
        if let Some(attribute) = step.strip_prefix('@') {
            if attribute.is_empty() {
                return Err(t!("body.xpath_attribute_empty").to_string());
            }
            steps.push(Step::Attribute(attribute.to_string()));
            continue;
        }
        let (name, index) = match step.split_once('[') {
            Some((name, rest)) => {
                let number = rest
                    .strip_suffix(']')
                    .and_then(|n| n.trim().parse::<usize>().ok())
                    .filter(|n| *n > 0)
                    .ok_or_else(|| t!("body.xpath_index", step = step).to_string())?;
                (name.trim().to_string(), Some(number - 1))
            }
            None => (step.to_string(), None),
        };
        if name.is_empty() {
            return Err(t!("body.xpath_step_empty").to_string());
        }
        steps.push(if descendant {
            Step::Descendant { name, index }
        } else {
            Step::Child { name, index }
        });
    }
    Ok(steps)
}

fn matches(node: &Node, name: &str) -> bool {
    name == "*" || node.name == name
}

fn descendants<'a>(node: &'a Node, name: &str, found: &mut Vec<&'a Node>) {
    for child in &node.children {
        if matches(child, name) {
            found.push(child);
        }
        descendants(child, name, found);
    }
}

/// What a path selected: elements, or the strings of attributes and text.
#[derive(Debug, PartialEq)]
pub enum Selection {
    Elements(Vec<String>),
    Values(Vec<String>),
}

impl Selection {
    pub fn count(&self) -> usize {
        match self {
            Self::Elements(items) | Self::Values(items) => items.len(),
        }
    }

    /// The matches as text to show, one after another.
    pub fn text(&self) -> String {
        match self {
            Self::Elements(items) => items.join("\n"),
            Self::Values(items) => items.join("\n"),
        }
    }
}

/// Reads XML with a small subset of XPath: `/feed/entry`, `//title`, `//entry[2]/@id`,
/// `//title/text()` and `*` for any element. Enough to pull a value out of a response.
pub fn select_xml(xml: &str, path: &str) -> Result<Selection, String> {
    let root = parse(xml)?;
    let steps = parse_path(path)?;
    let mut current: Vec<&Node> = vec![&root];
    // A path starting at the root names the root itself: "/feed/entry" starts inside <feed>.
    for (position, step) in steps.iter().enumerate() {
        match step {
            Step::Child { name, index } | Step::Descendant { name, index } => {
                let mut next: Vec<&Node> = Vec::new();
                for node in &current {
                    match step {
                        Step::Descendant { .. } => {
                            if position == 0 && matches(node, name) {
                                next.push(node);
                            }
                            descendants(node, name, &mut next);
                        }
                        _ => {
                            if position == 0 && matches(node, name) && steps.len() == 1 {
                                next.push(node);
                            } else if position == 0 && matches(node, name) {
                                // The root itself; its children are matched by the next step.
                                next.push(node);
                            } else {
                                next.extend(node.children.iter().filter(|child| matches(child, name)));
                            }
                        }
                    }
                }
                if let Some(index) = index {
                    next = next.into_iter().skip(*index).take(1).collect();
                }
                current = next;
            }
            Step::Attribute(name) => {
                let values = current
                    .iter()
                    .filter_map(|node| {
                        node.attributes
                            .iter()
                            .find(|(attribute, _)| attribute == name)
                            .map(|(_, value)| value.clone())
                    })
                    .collect();
                return Ok(Selection::Values(values));
            }
            Step::Text => {
                let values = current.iter().map(|node| node.text.clone()).collect();
                return Ok(Selection::Values(values));
            }
        }
    }
    let elements = current
        .into_iter()
        .map(|node| {
            let mut out = String::new();
            write_node(node, 0, &mut out);
            out.trim_end().to_string()
        })
        .collect();
    Ok(Selection::Elements(elements))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Courier releases</title>
  <entry id="1"><title>0.1.0</title><summary>First</summary></entry>
  <entry id="2"><title>0.2.0</title><summary>Second</summary></entry>
</feed>"#;

    #[test]
    fn picks_a_view_from_the_content_type_then_the_body() {
        assert_eq!(
            BodyView::of(Some("application/json; charset=utf-8"), "{}"),
            BodyView::Json
        );
        assert_eq!(BodyView::of(Some("application/problem+json"), "{}"), BodyView::Json);
        assert_eq!(BodyView::of(Some("text/html"), ""), BodyView::Html);
        assert_eq!(BodyView::of(Some("application/xml"), ""), BodyView::Xml);
        assert_eq!(BodyView::of(Some("image/png"), ""), BodyView::Image("png"));
        assert_eq!(BodyView::of(Some("application/pdf"), ""), BodyView::Binary);
        assert_eq!(BodyView::of(Some("text/csv"), "a,b"), BodyView::Text);
        // No type: go by the body.
        assert_eq!(BodyView::of(None, " [1, 2]"), BodyView::Json);
        assert_eq!(BodyView::of(None, "<!doctype html><p>hi"), BodyView::Html);
        assert_eq!(BodyView::of(None, "<rss><channel/></rss>"), BodyView::Xml);
        assert_eq!(BodyView::of(None, "plain words"), BodyView::Text);
        assert_eq!(BodyView::of(None, "PK\u{3}\u{4}\0\u{fffd}"), BodyView::Binary);
        assert_eq!(BodyView::Xml.filter_kind(), Some("xpath"));
        assert_eq!(BodyView::Text.filter_kind(), None);
    }

    #[test]
    fn indents_xml() {
        let pretty = pretty_xml("<a><b x=\"1\">hi</b><c/></a>");
        assert_eq!(pretty, "<a>\n  <b x=\"1\">hi</b>\n  <c/>\n</a>");
        assert_eq!(pretty_xml("not xml at all"), "not xml at all", "left alone");
    }

    #[test]
    fn selects_with_a_small_xpath() {
        let values = |path: &str| match select_xml(FEED, path) {
            Ok(Selection::Values(values)) => values,
            other => panic!("{path}: {other:?}"),
        };
        let elements = |path: &str| match select_xml(FEED, path) {
            Ok(Selection::Elements(items)) => items,
            other => panic!("{path}: {other:?}"),
        };
        assert_eq!(values("//entry/title/text()"), ["0.1.0", "0.2.0"]);
        assert_eq!(values("//entry/@id"), ["1", "2"]);
        assert_eq!(values("//entry[2]/title/text()"), ["0.2.0"]);
        assert_eq!(values("/feed/title/text()"), ["Courier releases"], "the root is named");
        assert_eq!(values("title/text()").len(), 3, "a bare name searches everywhere");
        assert_eq!(
            elements("//entry[1]"),
            ["<entry id=\"1\">\n  <title>0.1.0</title>\n  <summary>First</summary>\n</entry>"]
        );
        assert_eq!(select_xml(FEED, "//nothing").unwrap().count(), 0);
        assert_eq!(values("//entry[1]/*/text()"), ["0.1.0", "First"], "* takes any child");

        assert!(select_xml(FEED, "").is_err());
        assert!(select_xml(FEED, "//entry[x]").is_err());
        assert!(select_xml("<a><b>", "//b").is_ok(), "an unclosed tag still reads");
    }
}
