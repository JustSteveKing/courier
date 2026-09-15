//! Schema-aware help while writing a query: what to suggest at the cursor and what to show
//! when hovering a name. Works on the text alone with a forgiving scan, so half-written
//! queries still get suggestions.

use std::ops::Range;

use rust_i18n::t;

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

/// Tokens of `text` with their byte ranges, and whether it ends inside a string or comment.
fn tokenize(text: &str) -> (Vec<(Token<'_>, Range<usize>)>, bool) {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let token = match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' | b',' => {
                i += 1;
                continue;
            }
            b'#' => match text[i..].find('\n') {
                Some(end) => {
                    i += end + 1;
                    continue;
                }
                None => return (tokens, true),
            },
            b'"' if text[i..].starts_with("\"\"\"") => match text[i + 3..].find("\"\"\"") {
                Some(end) => {
                    i += 3 + end + 3;
                    Token::Value
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
                Token::Value
            }
            b'.' if text[i..].starts_with("...") => {
                i += 3;
                Token::Spread
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                while i < bytes.len() && is_name_byte(bytes[i]) {
                    i += 1;
                }
                Token::Name(&text[start..i])
            }
            b'0'..=b'9' | b'-' => {
                while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    i += 1;
                }
                Token::Value
            }
            b @ (b'{' | b'}' | b'(' | b')' | b'[' | b']' | b':' | b'!' | b'$' | b'=' | b'@' | b'|' | b'&') => {
                i += 1;
                Token::Punct(b as char)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        tokens.push((token, start..i));
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
    /// Collected only when validating.
    problems: Option<Vec<Problem>>,
    /// The field most recently selected in the innermost selection set, until its
    /// arguments and selection are complete.
    field_use: Option<FieldUse>,
}

#[derive(Debug)]
struct FieldUse {
    owner: String,
    name: String,
    span: Range<usize>,
    args: Vec<String>,
}

impl<'a> Scan<'a> {
    fn new(schema: &Schema, text: &'a str) -> Self {
        Self::run(schema, text, false)
    }

    fn run(schema: &Schema, text: &'a str, validate: bool) -> Self {
        let (tokens, in_literal) = tokenize(text);
        let mut scan = Scan {
            in_literal,
            problems: validate.then(Vec::new),
            ..Default::default()
        };
        for (index, (token, span)) in tokens.iter().enumerate() {
            if validate {
                let next = tokens.get(index + 1).map(|(t, _)| *t);
                scan.check(schema, *token, span.clone(), next);
            }
            scan.step(schema, *token);
            scan.prev = Some(*token);
        }
        scan
    }

    fn report(&mut self, range: Range<usize>, kind: ProblemKind) {
        if let Some(problems) = &mut self.problems {
            problems.push(Problem { range, kind });
        }
    }

    /// Checks `token` against the schema before it's applied to the scan.
    fn check(&mut self, schema: &Schema, token: Token<'a>, span: Range<usize>, next: Option<Token<'a>>) {
        match token {
            Token::Name(_) if self.prev == Some(Token::Punct('@')) => {}
            Token::Name(name) if self.prev == Some(Token::Punct('$')) => {
                let declaring = self.stack.last() == Some(&Frame::VarDefs);
                if !declaring && !self.stack.is_empty() && !self.variables.iter().any(|v| v == name) {
                    self.report(span, ProblemKind::UndeclaredVariable(name.to_string()));
                }
            }
            Token::Name(name) if self.expect_type => {
                if schema.get(name).is_none() {
                    self.report(span, ProblemKind::UnknownType(name.to_string()));
                }
            }
            Token::Name(name) => match self.stack.last().cloned() {
                None => {
                    let kind = match name {
                        "mutation" => Some(OperationKind::Mutation),
                        "subscription" => Some(OperationKind::Subscription),
                        _ => None,
                    };
                    if let Some(kind) = kind
                        && self.prev.is_none_or(|p| p == Token::Punct('}'))
                        && schema.root_name(kind).is_none()
                    {
                        self.report(span, ProblemKind::NoRootType(name.to_string()));
                    }
                }
                Some(Frame::Selection(owner)) if !self.spread && next != Some(Token::Punct(':')) => {
                    self.finish_field(schema, false);
                    // Inside a scalar's `{ }`, which is already reported on the field.
                    let Some(owner) = owner
                        .as_deref()
                        .and_then(|t| schema.get(t))
                        .filter(|t| !matches!(t.kind, TypeKind::Scalar | TypeKind::Enum))
                    else {
                        return;
                    };
                    let is_query_root = schema.query_type.as_deref() == Some(owner.name.as_str());
                    match owner.field(name) {
                        Some(_) => {
                            self.field_use = Some(FieldUse {
                                owner: owner.name.clone(),
                                name: name.to_string(),
                                span,
                                args: Vec::new(),
                            })
                        }
                        None if name == "__typename" => {}
                        None if is_query_root && matches!(name, "__schema" | "__type") => {}
                        None if owner.kind == TypeKind::Union => self.report(
                            span,
                            ProblemKind::FieldOnUnion {
                                field: name.to_string(),
                                union: owner.name.clone(),
                            },
                        ),
                        None => self.report(
                            span,
                            ProblemKind::UnknownField {
                                field: name.to_string(),
                                owner: owner.name.clone(),
                            },
                        ),
                    }
                }
                Some(Frame::Args { owner, field, arg }) => {
                    let Some(field_def) = owner
                        .as_deref()
                        .zip(field.as_deref())
                        .and_then(|(owner, field)| schema.get(owner)?.field(field))
                    else {
                        return;
                    };
                    if next == Some(Token::Punct(':')) {
                        if field_def.args.iter().any(|a| a.name == name) {
                            if let Some(used) = &mut self.field_use
                                && Some(&used.name) == field.as_ref()
                            {
                                used.args.push(name.to_string());
                            }
                        } else {
                            self.report(
                                span,
                                ProblemKind::UnknownArgument {
                                    argument: name.to_string(),
                                    field: format!("{}.{}", owner.unwrap_or_default(), field_def.name),
                                },
                            );
                        }
                    } else if self.prev == Some(Token::Punct(':'))
                        && let Some(enum_type) = arg
                            .as_deref()
                            .and_then(|arg| field_def.args.iter().find(|a| a.name == arg))
                            .and_then(|arg| schema.get(arg.ty.named()))
                            .filter(|t| t.kind == TypeKind::Enum)
                        && name != "null"
                        && !enum_type.enum_values.iter().any(|v| v.name == name)
                    {
                        self.report(
                            span,
                            ProblemKind::UnknownEnumValue {
                                value: name.to_string(),
                                enum_type: enum_type.name.clone(),
                            },
                        );
                    }
                }
                Some(Frame::VarDefs) if matches!(self.prev, Some(Token::Punct(':' | '['))) => match schema.get(name) {
                    None => self.report(span, ProblemKind::UnknownType(name.to_string())),
                    Some(t) if !t.kind.is_input() => self.report(span, ProblemKind::NotInputType(name.to_string())),
                    Some(_) => {}
                },
                _ => {}
            },
            Token::Spread | Token::Punct('}') if matches!(self.stack.last(), Some(Frame::Selection(_))) => {
                self.finish_field(schema, false);
            }
            Token::Punct('{')
                if matches!(self.stack.last(), Some(Frame::Selection(_))) && !self.spread && self.pending.is_none() =>
            {
                self.finish_field(schema, true);
            }
            _ => {}
        }
    }

    /// Checks the last selected field once its arguments and selection (if any) are known.
    fn finish_field(&mut self, schema: &Schema, has_selection: bool) {
        let Some(used) = self.field_use.take() else {
            return;
        };
        let Some(field) = schema.get(&used.owner).and_then(|t| t.field(&used.name)) else {
            return;
        };
        for arg in &field.args {
            let required = arg.ty.kind == TypeKind::NonNull && arg.default_value.is_none();
            if required && !used.args.contains(&arg.name) {
                self.report(
                    used.span.clone(),
                    ProblemKind::MissingArgument {
                        field: used.name.clone(),
                        argument: format!("{}: {}", arg.name, arg.ty),
                    },
                );
            }
        }
        let Some(returns) = schema.get(field.ty.named()) else {
            return;
        };
        let composite = matches!(returns.kind, TypeKind::Object | TypeKind::Interface | TypeKind::Union);
        if composite && !has_selection {
            self.report(
                used.span,
                ProblemKind::NeedsSelection {
                    field: used.name,
                    returns: field.ty.to_string(),
                },
            );
        } else if !composite && has_selection {
            self.report(
                used.span,
                ProblemKind::NoSubfields {
                    field: used.name,
                    returns: field.ty.to_string(),
                },
            );
        }
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

/// Something wrong with a query, at a byte range.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub range: Range<usize>,
    pub kind: ProblemKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProblemKind {
    UnknownField { field: String, owner: String },
    FieldOnUnion { field: String, union: String },
    UnknownArgument { argument: String, field: String },
    MissingArgument { field: String, argument: String },
    UnknownEnumValue { value: String, enum_type: String },
    UnknownType(String),
    NotInputType(String),
    UndeclaredVariable(String),
    NoRootType(String),
    NeedsSelection { field: String, returns: String },
    NoSubfields { field: String, returns: String },
}

impl Problem {
    /// Missing arguments are warnings (the server may still answer); the rest are errors.
    pub fn is_warning(&self) -> bool {
        matches!(self.kind, ProblemKind::MissingArgument { .. })
    }

    pub fn message(&self) -> String {
        match &self.kind {
            ProblemKind::UnknownField { field, owner } => t!("graphql.unknown_field", field = field, owner = owner),
            ProblemKind::FieldOnUnion { field, union } => t!("graphql.field_on_union", field = field, union = union),
            ProblemKind::UnknownArgument { argument, field } => {
                t!("graphql.unknown_argument", argument = argument, field = field)
            }
            ProblemKind::MissingArgument { field, argument } => {
                t!("graphql.missing_argument", field = field, argument = argument)
            }
            ProblemKind::UnknownEnumValue { value, enum_type } => {
                t!("graphql.unknown_enum_value", value = value, enum_type = enum_type)
            }
            ProblemKind::UnknownType(name) => t!("graphql.unknown_type", name = name),
            ProblemKind::NotInputType(name) => t!("graphql.not_input_type", name = name),
            ProblemKind::UndeclaredVariable(name) => t!("graphql.undeclared_variable", name = name),
            ProblemKind::NoRootType(operation) => t!("graphql.no_root_type", operation = operation),
            ProblemKind::NeedsSelection { field, returns } => {
                t!("graphql.needs_selection", field = field, returns = returns)
            }
            ProblemKind::NoSubfields { field, returns } => t!("graphql.no_subfields", field = field, returns = returns),
        }
        .to_string()
    }
}

/// Checks a query against the schema: unknown fields, arguments, enum values, types and
/// variables, missing required arguments, and missing or impossible selections. Text that
/// doesn't parse is skipped rather than reported, so a half-typed query stays quiet.
pub fn validate(schema: &Schema, text: &str) -> Vec<Problem> {
    Scan::run(schema, text, true).problems.unwrap_or_default()
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

    /// Problems in `query` as (kind, underlined text).
    fn problems(query: &str) -> Vec<(ProblemKind, String)> {
        validate(&petstore(), query)
            .into_iter()
            .map(|p| (p.kind, query[p.range].to_string()))
            .collect()
    }

    #[test]
    fn valid_queries_have_no_problems() {
        let query = r#"
            query Pets($species: Species, $id: ID!) {
              pets(first: 10, species: $species) {
                id
                name
                ... on Dog { goodBoy }
                ...petFields
                owner { name }
              }
              favourite: pet(id: $id) { __typename id }
              search(text: "x") { ... on Owner { name } }
              __schema { types { name } }
            }
            mutation { adopt(input: { petId: "1", ownerName: "Sam" }) { id } }
            fragment petFields on Pet { species }
        "#;
        assert_eq!(problems(query), []);
        assert_eq!(problems("{ pets(species: DOG) { id } }"), []);
    }

    #[test]
    fn reports_unknown_names() {
        use ProblemKind::*;
        let unknown_field = |field: &str, owner: &str| UnknownField {
            field: field.into(),
            owner: owner.into(),
        };
        assert_eq!(
            problems("{ pets { id nmae owner { email } } }"),
            [
                (unknown_field("nmae", "Pet"), "nmae".into()),
                (unknown_field("email", "Owner"), "email".into()),
            ]
        );
        assert_eq!(
            problems("{ pets(limit: 1) { id } }"),
            [(
                UnknownArgument {
                    argument: "limit".into(),
                    field: "Query.pets".into()
                },
                "limit".into()
            )]
        );
        assert_eq!(
            problems("{ pets(species: HAMSTER) { id } }"),
            [(
                UnknownEnumValue {
                    value: "HAMSTER".into(),
                    enum_type: "Species".into()
                },
                "HAMSTER".into()
            )]
        );
        assert_eq!(
            problems("query ($n: Integer, $p: Pet) { pets { ... on Parrot { id } } }"),
            [
                (UnknownType("Integer".into()), "Integer".into()),
                (NotInputType("Pet".into()), "Pet".into()),
                (UnknownType("Parrot".into()), "Parrot".into()),
            ]
        );
        assert_eq!(
            problems("{ pets(first: $count) { id } }"),
            [(UndeclaredVariable("count".into()), "count".into())]
        );
        assert_eq!(
            problems("subscription { pets { id } }"),
            [(NoRootType("subscription".into()), "subscription".into())]
        );
        assert_eq!(
            problems("{ search(text: \"x\") { name } }"),
            [(
                FieldOnUnion {
                    field: "name".into(),
                    union: "SearchResult".into()
                },
                "name".into()
            )]
        );
    }

    #[test]
    fn reports_selection_and_argument_mistakes() {
        use ProblemKind::*;
        assert_eq!(
            problems("{ pets }"),
            [(
                NeedsSelection {
                    field: "pets".into(),
                    returns: "[Pet!]!".into()
                },
                "pets".into()
            )]
        );
        assert_eq!(
            problems("{ pets { name { first } } }"),
            [(
                NoSubfields {
                    field: "name".into(),
                    returns: "String!".into()
                },
                "name".into()
            )]
        );
        assert_eq!(
            problems("{ pet { id } }"),
            [(
                MissingArgument {
                    field: "pet".into(),
                    argument: "id: ID!".into()
                },
                "pet".into()
            )]
        );
        let problem = &validate(&petstore(), "{ pet { id } }")[0];
        assert!(problem.is_warning());
        assert!(!problem.message().is_empty());
    }

    #[test]
    fn half_typed_queries_stay_mostly_quiet() {
        // Still typing the selection or an argument list: nothing is complete enough to judge.
        assert_eq!(problems("{ pets { id "), []);
        assert_eq!(problems("{ pets(first: "), []);
        assert_eq!(problems("{ pets { ... on "), []);
        assert_eq!(problems("{ pets { id } # a comment with { braces"), []);
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
