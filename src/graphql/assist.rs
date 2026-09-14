//! Schema-aware help while writing a query: what to suggest at the cursor and what to show
//! when hovering a name. Works on the text alone with a forgiving scan, so half-written
//! queries still get suggestions.

use std::ops::Range;

use super::{OperationKind, Schema, TypeDef, TypeKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    Field,
    Argument,
    Type,
    EnumValue,
    Variable,
    Keyword,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub label: String,
    pub kind: SuggestionKind,
    /// A short type or signature shown next to the label.
    pub detail: Option<String>,
    pub documentation: Option<String>,
    pub deprecated: bool,
}

impl Suggestion {
    fn new(label: impl Into<String>, kind: SuggestionKind) -> Self {
        Self {
            label: label.into(),
            kind,
            detail: None,
            documentation: None,
            deprecated: false,
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn documentation(mut self, documentation: Option<&String>) -> Self {
        self.documentation = documentation.filter(|d| !d.trim().is_empty()).cloned();
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Token<'a> {
    Name(&'a str),
    Punct(char),
    Spread,
    Value,
}

/// Tokens of `text`, and whether it ends inside a string or comment.
fn tokenize(text: &str) -> (Vec<Token<'_>>, bool) {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' | b',' => i += 1,
            b'#' => match text[i..].find('\n') {
                Some(end) => i += end + 1,
                None => return (tokens, true),
            },
            b'"' if text[i..].starts_with("\"\"\"") => match text[i + 3..].find("\"\"\"") {
                Some(end) => {
                    i += 3 + end + 3;
                    tokens.push(Token::Value);
                }
                None => return (tokens, true),
            },
            b'"' => {
                let mut j = i + 1;
                loop {
                    match bytes.get(j) {
                        None | Some(b'\n') => return (tokens, true),
                        Some(b'\\') => j += 2,
                        Some(b'"') => break,
                        Some(_) => j += 1,
                    }
                }
                i = j + 1;
                tokens.push(Token::Value);
            }
            b'.' if text[i..].starts_with("...") => {
                i += 3;
                tokens.push(Token::Spread);
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let start = i;
                while i < bytes.len() && is_name_byte(bytes[i]) {
                    i += 1;
                }
                tokens.push(Token::Name(&text[start..i]));
            }
            b'0'..=b'9' | b'-' => {
                while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    i += 1;
                }
                tokens.push(Token::Value);
            }
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b':' | b'!' | b'$' | b'=' | b'@' | b'|' | b'&' => {
                tokens.push(Token::Punct(b as char));
                i += 1;
            }
            _ => i += 1,
        }
    }
    (tokens, false)
}

fn is_name_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

#[derive(Clone, Debug, PartialEq)]
enum Frame {
    /// Inside `{ … }` selecting from a type (unknown when the schema doesn't say).
    Selection(Option<String>),
    /// Inside a field's `( … )`, with the argument whose value is being written.
    Args {
        owner: Option<String>,
        field: Option<String>,
        arg: Option<String>,
    },
    /// An operation's `($id: ID!, …)`.
    VarDefs,
    /// A list, object or directive argument value we don't look inside.
    Value,
}

/// Where a position is in a document.
#[derive(Debug, Default)]
struct Scan<'a> {
    stack: Vec<Frame>,
    prev: Option<Token<'a>>,
    /// The type name for the next `{`, from `on Type`.
    pending: Option<String>,
    pending_root: Option<OperationKind>,
    /// The last token was `on` in a fragment or inline fragment, so a type name comes next.
    expect_type: bool,
    spread: bool,
    after_directive: bool,
    last_field: Option<String>,
    variables: Vec<String>,
    in_literal: bool,
}

impl<'a> Scan<'a> {
    fn new(schema: &Schema, text: &'a str) -> Self {
        let (tokens, in_literal) = tokenize(text);
        let mut scan = Scan {
            in_literal,
            ..Default::default()
        };
        for token in tokens {
            scan.step(schema, token);
            scan.prev = Some(token);
        }
        scan
    }

    fn step(&mut self, schema: &Schema, token: Token<'a>) {
        let directive_name = self.after_directive;
        self.after_directive = false;
        match token {
            Token::Name(_) if self.prev == Some(Token::Punct('@')) => {
                self.after_directive = true;
            }
            Token::Name(name) if self.prev == Some(Token::Punct('$')) => {
                if self.stack.last() == Some(&Frame::VarDefs) && !self.variables.iter().any(|v| v == name) {
                    self.variables.push(name.to_string());
                }
            }
            Token::Name(name) => match self.stack.last() {
                None | Some(Frame::Selection(_)) if self.expect_type => {
                    self.pending = Some(name.to_string());
                    self.expect_type = false;
                }
                None => match name {
                    "query" => self.pending_root = Some(OperationKind::Query),
                    "mutation" => self.pending_root = Some(OperationKind::Mutation),
                    "subscription" => self.pending_root = Some(OperationKind::Subscription),
                    "fragment" => self.pending_root = None,
                    "on" => self.expect_type = true,
                    _ => {}
                },
                Some(Frame::Selection(_)) => {
                    if name == "on" && self.spread {
                        self.expect_type = true;
                    } else if self.spread {
                        // A named fragment spread: `...petFields`.
                        self.spread = false;
                        self.last_field = None;
                    } else {
                        self.last_field = Some(name.to_string());
                    }
                }
                _ => {}
            },
            Token::Spread => {
                self.spread = true;
                self.last_field = None;
            }
            Token::Punct('{') => {
                let frame = match self.stack.last() {
                    None => {
                        let root = self.pending_root.take().unwrap_or(OperationKind::Query);
                        let ty = self
                            .pending
                            .take()
                            .or_else(|| schema.root_name(root).map(str::to_string));
                        Frame::Selection(ty)
                    }
                    Some(Frame::Selection(current)) => {
                        let ty = if let Some(ty) = self.pending.take() {
                            Some(ty)
                        } else if self.spread {
                            current.clone()
                        } else {
                            self.last_field
                                .as_deref()
                                .zip(current.as_deref())
                                .and_then(|(field, current)| schema.get(current)?.field(field))
                                .map(|field| field.ty.named().to_string())
                        };
                        Frame::Selection(ty)
                    }
                    Some(_) => Frame::Value,
                };
                self.stack.push(frame);
                self.spread = false;
                self.expect_type = false;
                self.last_field = None;
            }
            Token::Punct('}') => {
                self.stack.pop();
                self.last_field = None;
                self.spread = false;
            }
            Token::Punct('(') => {
                let frame = match self.stack.last() {
                    _ if directive_name => Frame::Value,
                    None => Frame::VarDefs,
                    Some(Frame::Selection(owner)) => Frame::Args {
                        owner: owner.clone(),
                        field: self.last_field.clone(),
                        arg: None,
                    },
                    Some(_) => Frame::Value,
                };
                self.stack.push(frame);
            }
            Token::Punct(')') => {
                self.stack.pop();
            }
            Token::Punct('[') => {
                if matches!(self.stack.last(), Some(Frame::Args { .. } | Frame::Value)) {
                    self.stack.push(Frame::Value);
                }
            }
            Token::Punct(']') => {
                if self.stack.last() == Some(&Frame::Value) {
                    self.stack.pop();
                }
            }
            Token::Punct(':') => match self.stack.last_mut() {
                Some(Frame::Selection(_)) => self.last_field = None,
                Some(Frame::Args { arg, .. }) => {
                    if let Some(Token::Name(name)) = self.prev {
                        *arg = Some(name.to_string());
                    }
                }
                _ => {}
            },
            Token::Punct(_) | Token::Value => {}
        }
    }
}

/// The identifier around (or ending at) `offset`.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let bytes = text.as_bytes();
    let offset = offset.min(text.len());
    let mut start = offset;
    while start > 0 && is_name_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    start..end
}

fn field_suggestions(ty: &TypeDef) -> Vec<Suggestion> {
    let mut suggestions: Vec<_> = ty
        .fields
        .iter()
        .map(|f| {
            let mut s = Suggestion::new(&f.name, SuggestionKind::Field)
                .detail(f.ty.to_string())
                .documentation(f.description.as_ref());
            s.deprecated = f.is_deprecated;
            s
        })
        .collect();
    suggestions.push(Suggestion::new("__typename", SuggestionKind::Field).detail("String!"));
    suggestions
}

fn type_suggestions<'s>(types: impl Iterator<Item = &'s TypeDef>) -> Vec<Suggestion> {
    types
        .map(|t| {
            Suggestion::new(&t.name, SuggestionKind::Type)
                .detail(t.kind.label())
                .documentation(t.description.as_ref())
        })
        .collect()
}

/// What to suggest for the word being typed at `offset`: the byte range it replaces and
/// the suggestions whose names start with it (case-insensitively).
pub fn complete(schema: &Schema, text: &str, offset: usize) -> (Range<usize>, Vec<Suggestion>) {
    let offset = offset.min(text.len());
    let word = word_at(text, offset);
    let range = word.start..offset;
    if !text.is_char_boundary(word.start) || text[word.start..].starts_with(|c: char| c.is_ascii_digit()) {
        return (range, Vec::new());
    }
    let scan = Scan::new(schema, &text[..word.start]);
    if scan.in_literal {
        return (range, Vec::new());
    }
    let keywords = |words: &[&str]| -> Vec<Suggestion> {
        words
            .iter()
            .map(|w| Suggestion::new(*w, SuggestionKind::Keyword))
            .collect()
    };

    let suggestions = match (scan.stack.last(), scan.prev) {
        (Some(Frame::VarDefs), Some(Token::Punct('$'))) => Vec::new(),
        (_, Some(Token::Punct('$'))) => scan
            .variables
            .iter()
            .map(|v| Suggestion::new(v, SuggestionKind::Variable))
            .collect(),
        (_, Some(Token::Punct('@'))) => keywords(&["include", "skip"]),
        (None, _) if scan.expect_type => type_suggestions(
            schema
                .user_types()
                .filter(|t| matches!(t.kind, TypeKind::Object | TypeKind::Interface | TypeKind::Union)),
        ),
        (None, None | Some(Token::Punct('}'))) => keywords(&["query", "mutation", "subscription", "fragment"]),
        (None, _) => Vec::new(),
        (Some(Frame::Selection(Some(ty))), prev) => match schema.get(ty) {
            None => Vec::new(),
            Some(_) if prev == Some(Token::Spread) => keywords(&["on"]),
            Some(def) if scan.expect_type => {
                let possible: Vec<&TypeDef> = if def.possible_types.is_empty() {
                    vec![def]
                } else {
                    def.possible_types
                        .iter()
                        .filter_map(|t| schema.get(t.named()))
                        .collect()
                };
                type_suggestions(possible.into_iter())
            }
            Some(def) => field_suggestions(def),
        },
        (Some(Frame::Args { owner, field, arg }), prev) => {
            let field = owner
                .as_deref()
                .zip(field.as_deref())
                .and_then(|(owner, field)| schema.get(owner)?.field(field));
            match (field, prev) {
                (None, _) => Vec::new(),
                (Some(field), Some(Token::Punct(':'))) => {
                    let arg_type = arg
                        .as_deref()
                        .and_then(|arg| field.args.iter().find(|a| a.name == arg))
                        .and_then(|arg| schema.get(arg.ty.named()));
                    match arg_type {
                        Some(t) if t.kind == TypeKind::Enum => t
                            .enum_values
                            .iter()
                            .map(|v| {
                                let mut s = Suggestion::new(&v.name, SuggestionKind::EnumValue)
                                    .detail(&t.name)
                                    .documentation(v.description.as_ref());
                                s.deprecated = v.is_deprecated;
                                s
                            })
                            .collect(),
                        Some(t) if t.name == "Boolean" => keywords(&["true", "false"]),
                        _ => Vec::new(),
                    }
                }
                (Some(field), _) => field
                    .args
                    .iter()
                    .map(|a| {
                        Suggestion::new(&a.name, SuggestionKind::Argument)
                            .detail(a.ty.to_string())
                            .documentation(a.description.as_ref())
                    })
                    .collect(),
            }
        }
        (Some(Frame::VarDefs), Some(Token::Punct(':' | '['))) => {
            type_suggestions(schema.user_types().filter(|t| t.kind.is_input()))
        }
        _ => Vec::new(),
    };

    let prefix = text[range.clone()].to_ascii_lowercase();
    let suggestions = suggestions
        .into_iter()
        .filter(|s| s.label.to_ascii_lowercase().starts_with(&prefix))
        .collect();
    (range, suggestions)
}

/// Markdown docs for the name at `offset`, with its byte range.
pub fn hover(schema: &Schema, text: &str, offset: usize) -> Option<(Range<usize>, String)> {
    let word = word_at(text, offset);
    if word.is_empty() {
        return None;
    }
    let name = &text[word.clone()];
    let scan = Scan::new(schema, &text[..word.start]);
    if scan.in_literal || matches!(scan.prev, Some(Token::Punct('$' | '@'))) {
        return None;
    }

    let type_doc = |name: &str| {
        let t = schema.get(name)?;
        Some(with_description(
            format!("```graphql\n{} {}\n```", t.kind.label(), t.name),
            t.description.as_deref(),
            None,
        ))
    };

    let doc = match scan.stack.last() {
        _ if scan.expect_type => type_doc(name),
        Some(Frame::VarDefs) if matches!(scan.prev, Some(Token::Punct(':' | '['))) => type_doc(name),
        Some(Frame::Selection(Some(ty))) if scan.prev != Some(Token::Spread) => {
            let field = schema.get(ty)?.field(name)?;
            Some(with_description(
                format!("```graphql\n{}\n```", field.signature()),
                field.description.as_deref(),
                field
                    .is_deprecated
                    .then(|| field.deprecation_reason.as_deref().unwrap_or("")),
            ))
        }
        Some(Frame::Args { owner, field, arg }) => {
            let field = schema.get(owner.as_deref()?)?.field(field.as_deref()?)?;
            if scan.prev == Some(Token::Punct(':')) {
                let arg = field.args.iter().find(|a| Some(&a.name) == arg.as_ref())?;
                let enum_type = schema.get(arg.ty.named())?;
                let value = enum_type.enum_values.iter().find(|v| v.name == name)?;
                Some(with_description(
                    format!("```graphql\n{}.{}\n```", enum_type.name, value.name),
                    value.description.as_deref(),
                    value
                        .is_deprecated
                        .then(|| value.deprecation_reason.as_deref().unwrap_or("")),
                ))
            } else {
                let arg = field.args.iter().find(|a| a.name == name)?;
                Some(with_description(
                    format!("```graphql\n{}: {}\n```", arg.name, arg.ty),
                    arg.description.as_deref(),
                    None,
                ))
            }
        }
        _ => None,
    }?;
    Some((word, doc))
}

fn with_description(signature: String, description: Option<&str>, deprecated: Option<&str>) -> String {
    let mut doc = signature;
    if let Some(description) = description.filter(|d| !d.trim().is_empty()) {
        doc.push_str("\n\n");
        doc.push_str(description.trim());
    }
    if let Some(reason) = deprecated {
        doc.push_str("\n\n*Deprecated");
        if !reason.is_empty() {
            doc.push_str(": ");
            doc.push_str(reason);
        }
        doc.push('*');
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphql::tests::petstore;

    /// Suggestion labels at the `|` in `query`.
    fn labels(query: &str) -> Vec<String> {
        let offset = query.find('|').expect("cursor");
        let text = query.replace('|', "");
        let (range, suggestions) = complete(&petstore(), &text, offset);
        assert_eq!(range.end, offset);
        suggestions.into_iter().map(|s| s.label).collect()
    }

    #[test]
    fn suggests_root_fields_and_nested_fields() {
        assert_eq!(labels("{ |"), ["pets", "pet", "search", "oldPets", "__typename"]);
        assert_eq!(labels("query Pets { pe| }"), ["pets", "pet"]);
        assert_eq!(labels("mutation { |"), ["adopt", "__typename"]);
        assert_eq!(labels("{ pets { owner { na| } } }"), ["name"]);
        assert_eq!(
            labels("{ pets { id } pet(id: 1) { |"),
            ["id", "name", "species", "owner", "__typename"]
        );
        assert_eq!(labels("{ favourite: pet(id: 1) { own|"), ["owner"]);
    }

    #[test]
    fn suggests_arguments_values_and_variables() {
        assert_eq!(labels("{ pets(|"), ["first", "species"]);
        assert_eq!(labels("{ pets(first: 10, sp|"), ["species"]);
        assert_eq!(labels("{ pets(species: |"), ["DOG", "CAT", "FERRET"]);
        assert_eq!(labels("query ($n: Int, $s: Species) { pets(first: $|"), ["n", "s"]);
        assert_eq!(
            labels("query ($n: |"),
            ["Int", "String", "Boolean", "ID", "Species", "AdoptInput"]
        );
        assert!(labels("query ($|").is_empty(), "naming a new variable");
    }

    #[test]
    fn follows_fragments() {
        assert_eq!(labels("{ pets { ... on Dog { goo| } } }"), ["goodBoy"]);
        assert_eq!(labels("{ pets { ... on |"), ["Dog", "Cat"]);
        assert_eq!(labels("{ pets { ...|"), ["on"]);
        assert_eq!(
            labels("{ search(text: \"x\") { ... on Owner { |"),
            ["id", "name", "pets", "__typename"]
        );
        assert_eq!(labels("fragment F on Cat { li|"), ["lives"]);
        assert_eq!(labels("fragment F on |").len(), 7);
        assert_eq!(labels("{ pets { ...F ... @include(if: true) { na|"), ["name"]);
    }

    #[test]
    fn stays_quiet_where_nothing_fits() {
        assert!(labels("{ pets(first: \"|").is_empty(), "inside a string");
        assert!(labels("{ # pe|").is_empty(), "inside a comment");
        assert!(labels("{ unknown { |").is_empty(), "unknown field");
        assert_eq!(labels("|"), ["query", "mutation", "subscription", "fragment"]);
        assert!(labels("{ pets(first: 1|").is_empty(), "a number");
    }

    #[test]
    fn hovers_fields_arguments_types_and_values() {
        let schema = petstore();
        let doc = |query: &str| {
            let offset = query.find('|').unwrap();
            let text = query.replace('|', "");
            hover(&schema, &text, offset).map(|(_, doc)| doc)
        };
        let pets = doc("{ pe|ts { id } }").unwrap();
        assert!(pets.contains("pets(first: Int, species: Species): [Pet!]!"), "{pets}");
        assert!(pets.contains("Pets in the store, newest first."));
        assert!(doc("{ old|Pets }").unwrap().contains("*Deprecated: Use pets*"));
        assert!(doc("{ pets(fi|rst: 1) }").unwrap().contains("first: Int"));
        assert!(doc("{ pets(species: D|OG) }").unwrap().contains("Woof."));
        assert!(doc("{ pets { ... on D|og { id } } }").unwrap().contains("A good dog."));
        assert!(
            doc("query ($input: Adopt|Input!) { id }")
                .unwrap()
                .contains("input AdoptInput")
        );
        assert_eq!(doc("{ pets { nope| } }"), None);

        let (range, _) = hover(&schema, "{ pets }", 4).unwrap();
        assert_eq!(range, 2..6);
    }
}
