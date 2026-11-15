//! Source content: the form, the closed vocabulary, and the one render.
//!
//! This is the only place where a repository file's bytes can become different bytes without
//! reading the target. Three decisions shape it:
//!
//! - The form is read from the syntax tree, never from the file name (§1.5). A file that is not
//!   valid UTF-8, or that does not contain `{{`, has no tree and is copied byte for byte.
//! - The vocabulary is closed by tag kind and by name (§4.2). The AST can witness the kind and
//!   the name, but not `~` or a repeated hash key, so one bounded text scan covers those two.
//! - Rendering happens once, before any target is read (§4.10). A patch body is rendered on its
//!   own; the text outside blocks is never rendered and must hold no tag but a comment.

use std::ops::Range;
use std::path::MAIN_SEPARATOR_STR;

use handlebars::template::{HelperTemplate, Parameter, TemplateElement};
use handlebars::{
    Context, Handlebars, Helper, HelperDef, HelperResult, Output, Path, PathSeg, RenderContext,
    RenderError, RenderErrorReason, Renderable, Template,
};

use crate::diag::{Code, Diag, Error};
use crate::lexical;
use crate::patch::{Boundaries, Selector};
use crate::seal;

/// The two forms of §1.5. Only a `patch` block pushes a file into patch form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Whole,
    Patch,
}

/// One `patch` declaration after rendering: the bytes the source asks for, not yet located in
/// the target. `index` is the source order (1-based), which every `E-PA1` block must name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub index: usize,
    pub selector: Selector,
    /// Text bytes, or the decoded ciphertext when `seal` is set.
    pub body: Vec<u8>,
    /// The 1-based source order among this entry's seals, or `None` for a text body. The marker
    /// is what keeps an unfilled seal (`body` empty) distinct from a deletion (`body` empty).
    pub seal: Option<usize>,
}

/// One seal body as stored, and the container that gives it a position.
///
/// `body` is empty exactly when the source holds `{{#seal}}{{/seal}}`: an unfilled placeholder.
/// That is not an error here — `edit` fills it and `absorb` skips it — the plan reports `E-E1`
/// when a run needs bytes that are not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealSite {
    pub seal: usize,
    pub container: Container,
    pub body: Vec<u8>,
}

/// A seal fills either an entire file or the body of a patch block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Container {
    Whole,
    Patch,
}

/// A source file after the content pipeline. The bytes of a whole file are final; a patch file
/// contributes declarations and is resolved against the target's current bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    Whole(Vec<u8>),
    Sealed(SealSite),
    Patch(Vec<Block>),
}

impl Rendered {
    pub fn form(&self) -> Form {
        match self {
            Rendered::Whole(_) | Rendered::Sealed(_) => Form::Whole,
            Rendered::Patch(_) => Form::Patch,
        }
    }

    /// Whether the source holds a `seal` node at all. The predicate is syntactic (§5.4): a value
    /// that merely looks like a secret is not protected, and a seal that failed to render never
    /// reaches here.
    pub fn sealed(&self) -> bool {
        match self {
            Rendered::Whole(_) => false,
            Rendered::Sealed(_) => true,
            Rendered::Patch(blocks) => blocks.iter().any(|block| block.seal.is_some()),
        }
    }
}

/// What `absorb` and `edit` may treat as a hole, read from the tree without rendering.
///
/// `Plain` and `Template` are whole-form answers; `Patch` lists only the sealed bodies, because
/// a text body's collection is the patch write-back's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outline {
    /// The whole file is a literal, with or without escaped delimiters.
    Plain,
    /// Whole form with exactly one seal and no other bytes: the seal body is the hole.
    Whole(SealSite),
    /// Whole form with a template node whose value the source does not hold.
    Template,
    /// Patch form: the sealed bodies, in source order.
    Patch(Vec<SealSite>),
}

/// The engine and its data, constructed once per run.
///
/// The registry is not rebuilt per entry: the helpers are stateless and the data is one map, so
/// one context serves the whole plan. `set_strict_mode` makes a reference to a missing key an
/// error instead of an empty byte, which is what §4.3 requires of every reference.
pub struct Renderer {
    registry: Handlebars<'static>,
    context: Context,
}

impl Renderer {
    pub fn new<D: serde::Serialize>(data: D) -> Result<Self, Error> {
        let mut registry = Handlebars::new();
        registry.register_escape_fn(handlebars::no_escape);
        registry.set_strict_mode(true);
        registry.register_helper("env", Box::new(Env));
        registry.register_helper("which", Box::new(Which));
        registry.register_helper("join", Box::new(Join));
        let context =
            Context::wraps(data).map_err(|e| Error::bare(format!("template data: {e}")))?;
        Ok(Self { registry, context })
    }

    /// Content for one source file. Reads no target; every failure happens before any write.
    pub fn render(&self, spell: &str, source: &[u8]) -> Result<Rendered, Error> {
        let Some(template) = parse_source(spell, source)? else {
            return Ok(Rendered::Whole(source.to_vec()));
        };
        match source_form(spell, source, &template)? {
            SourceForm::Text => Ok(Rendered::Whole(self.render_template(spell, &template)?)),
            SourceForm::Sealed(site) => Ok(Rendered::Sealed(site)),
            SourceForm::Patch(specs) => Ok(Rendered::Patch(self.render_blocks(spell, specs)?)),
        }
    }

    fn render_template(&self, spell: &str, template: &Template) -> Result<Vec<u8>, Error> {
        let mut render_context = RenderContext::new(None);
        template
            .renders(&self.registry, &self.context, &mut render_context)
            .map(String::into_bytes)
            .map_err(|e| render_error(spell, &e))
    }

    fn render_blocks(&self, spell: &str, specs: Vec<BlockSpec<'_>>) -> Result<Vec<Block>, Error> {
        specs
            .into_iter()
            .enumerate()
            .map(|(offset, spec)| {
                let index = offset + 1;
                let (body, seal) = match spec.body {
                    BodySpec::Seal(site) => (site.body, Some(site.seal)),
                    BodySpec::Text(template) => {
                        let body = self.render_template(spell, template)?;
                        if body.is_empty() && !template.elements.is_empty() {
                            return Err(empty_rendered_body(spell, index));
                        }
                        (body, None)
                    }
                };
                Ok(Block { index, selector: spec.selector, body, seal })
            })
            .collect()
    }
}

struct BlockSpec<'a> {
    name: Option<String>,
    selector: Selector,
    body: BodySpec<'a>,
}

/// A block body before rendering. A seal body is already final: it is ciphertext, not a
/// template, and rendering it would change the bytes the seal has to open.
enum BodySpec<'a> {
    Text(&'a Template),
    Seal(SealSite),
}

enum SourceForm<'a> {
    Text,
    Sealed(SealSite),
    Patch(Vec<BlockSpec<'a>>),
}

fn parse_source(spell: &str, source: &[u8]) -> Result<Option<Template>, Error> {
    parsable(source)
        .map(|text| Template::compile(text).map_err(|error| compile_error(spell, &error)))
        .transpose()
}

fn source_form<'a>(
    spell: &str,
    source: &[u8],
    template: &'a Template,
) -> Result<SourceForm<'a>, Error> {
    match form(template) {
        Form::Whole => {
            lexical::check(spell, source)?;
            match whole_shape(spell, template)? {
                WholeShape::Template => Ok(SourceForm::Text),
                WholeShape::Seal(site) => Ok(SourceForm::Sealed(site)),
            }
        }
        Form::Patch => {
            let specs = check_patch_form(spell, template)?;
            lexical::check(spell, source)?;
            Ok(SourceForm::Patch(specs))
        }
    }
}

fn has_expressions(template: &Template) -> bool {
    template.elements.iter().any(|element| !matches!(element, TemplateElement::RawString(_)))
}

/// A source can be parsed as a template when it is UTF-8 and contains a tag delimiter. valid UTF-8 and at least one `{{`. Anything else has no tree.
fn parsable(source: &[u8]) -> Option<&str> {
    if !source.windows(2).any(|pair| pair == b"{{") {
        return None;
    }
    std::str::from_utf8(source).ok()
}

fn form(template: &Template) -> Form {
    if has_patch(&template.elements) { Form::Patch } else { Form::Whole }
}

fn has_patch(elements: &[TemplateElement]) -> bool {
    elements.iter().any(|element| match element {
        TemplateElement::HelperBlock(block) => {
            is_patch(block) || block.template.as_ref().is_some_and(|body| has_patch(&body.elements))
        }
        _ => false,
    })
}

fn is_patch(block: &HelperTemplate) -> bool {
    matches!(&block.name, Parameter::Name(name) if name == "patch")
}

fn is_seal(block: &HelperTemplate) -> bool {
    matches!(&block.name, Parameter::Name(name) if name == "seal")
}

fn is_seal_element(element: &TemplateElement) -> bool {
    matches!(element, TemplateElement::HelperBlock(block) if is_seal(block))
}

/// §4.2 for a whole file: expressions and comments are rendered, blocks are not.
fn check_whole(spell: &str, template: &Template) -> Result<(), Error> {
    check_nodes(spell, &template.elements, template)
}

/// A whole file is either a template or one exclusive seal. The branch is on the tree, so a file
/// with a seal never reaches the renderer: its bytes are the key's answer, not an expression's.
fn whole_shape(spell: &str, template: &Template) -> Result<WholeShape, Error> {
    if !template.elements.iter().any(is_seal_element) {
        check_whole(spell, template)?;
        return Ok(WholeShape::Template);
    }
    let site = exclusive_seal(spell, &template.elements, template, Container::Whole, 1)?;
    Ok(WholeShape::Seal(site))
}

enum WholeShape {
    Template,
    Seal(SealSite),
}

/// §4.6 for the top level of a patch file: text, comments, and `patch` blocks; nothing else,
/// because text outside blocks is not rendered and is not allowed to hold a node.
fn check_patch_form<'a>(spell: &str, template: &'a Template) -> Result<Vec<BlockSpec<'a>>, Error> {
    let mut specs = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut next_seal = 0;
    for (index, element) in template.elements.iter().enumerate() {
        let at = position(template, index);
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::HelperBlock(block) if is_patch(block) => {
                let spec = check_patch(spell, block, at, &mut next_seal)?;
                if let Some(name) = &spec.name
                    && !names.insert(name.clone())
                {
                    return Err(Diag::new(Code::R1, spell)
                        .missing("a unique patch name")
                        .current(format!("line {}, column {}: duplicate name {name:?}", at.0, at.1))
                        .why("the name selects one patch within this file")
                        .fix("rename one of the patches")
                        .into());
                }
                specs.push(spec);
            }
            _ => {
                return Err(forbidden(
                    spell,
                    at,
                    "a tag outside any `patch` block",
                    "a patch file renders only block bodies; text outside them is for humans",
                    "move the tag inside a `patch` block, or remove it",
                ));
            }
        }
    }
    Ok(specs)
}

/// The closure rule for a context where tags are rendered: a whole file or a patch body.
fn check_nodes(spell: &str, elements: &[TemplateElement], owner: &Template) -> Result<(), Error> {
    for (index, element) in elements.iter().enumerate() {
        let at = position(owner, index);
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::Expression(expression) => check_expression(spell, expression, at)?,
            TemplateElement::HelperBlock(block) if is_seal(block) => {
                return Err(seal_not_exclusive(spell, at));
            }
            TemplateElement::HelperBlock(block) => {
                return Err(forbidden(
                    spell,
                    at,
                    format!("block `{}`", name_of(&block.name)),
                    "`patch` and `seal` are the only blocks, and a block cannot nest inside another",
                    "use a `patch` block at the top level, or remove this block",
                ));
            }
            other => {
                return Err(forbidden(
                    spell,
                    at,
                    tag_kind(other),
                    "the vocabulary is closed by node kind, not only by name",
                    "rewrite it as `{{ name ... }}`, `{{#patch ...}}`, or a comment",
                ));
            }
        }
    }
    Ok(())
}

fn check_expression(
    spell: &str,
    expression: &HelperTemplate,
    at: (usize, usize),
) -> Result<(), Error> {
    check_expression_shape(spell, expression, at)?;
    check_call(spell, expression, at)
}

/// The constructs a closed form has no place for, before the name is looked at: a block used as
/// an expression, an `else` chain, a block parameter, and a hash argument.
fn check_expression_shape(
    spell: &str,
    expression: &HelperTemplate,
    at: (usize, usize),
) -> Result<(), Error> {
    if expression.block || expression.template.is_some() || expression.inverse.is_some() {
        return Err(forbidden(
            spell,
            at,
            "a block passed as an expression",
            "the form has no conditional construct",
            "remove the block",
        ));
    }
    if expression.chain {
        return Err(forbidden(
            spell,
            at,
            "`{{else}}`",
            "the form has no conditional construct",
            "remove the chain",
        ));
    }
    if expression.block_param.is_some() {
        return Err(forbidden(
            spell,
            at,
            "a block parameter (`as |x|`)",
            "the form has no iteration construct",
            "remove the block parameter",
        ));
    }
    if !expression.hash.is_empty() {
        return Err(forbidden(
            spell,
            at,
            "a hash argument on a function",
            "only `patch` accepts locating arguments",
            "pass the value as a positional argument",
        ));
    }
    Ok(())
}

fn check_call(spell: &str, expression: &HelperTemplate, at: (usize, usize)) -> Result<(), Error> {
    match &expression.name {
        Parameter::Name(name) => check_function(spell, name, expression, at),
        Parameter::Path(path) => {
            if !is_bare_key(path) || !expression.params.is_empty() {
                return Err(forbidden(
                    spell,
                    at,
                    "a bare data key",
                    "the data plane is flat: no prefix, no nesting, no index",
                    "use a key defined in `[data]`, or call `env` / `which` / `join`",
                ));
            }
            Ok(())
        }
        other => Err(forbidden(
            spell,
            at,
            format!("name `{}`", name_of(other)),
            "a call needs a function name or a data key",
            "rewrite it as `{{ name ... }}` or remove the tag",
        )),
    }
}

fn check_function(
    spell: &str,
    name: &str,
    expression: &HelperTemplate,
    at: (usize, usize),
) -> Result<(), Error> {
    let arity = match name {
        "env" | "which" => 1,
        "join" => 2,
        other => {
            return Err(forbidden(
                spell,
                at,
                format!("name `{other}`"),
                "only `env`, `which` and `join` are callable, and `patch` is a block",
                "use one of the three functions, a data key, or remove the tag",
            ));
        }
    };
    let count = expression.params.len();
    if name == "join" {
        if count < arity {
            return Err(forbidden(
                spell,
                at,
                "at least two path segments for `join`",
                "`join` concatenates segments; one segment has nothing to join",
                "pass two or more segments",
            ));
        }
    } else if count != arity {
        return Err(forbidden(
            spell,
            at,
            format!("exactly one argument for `{name}`"),
            "this function reads one value",
            "pass exactly one argument",
        ));
    }
    for param in &expression.params {
        check_param(spell, param, at)?;
    }
    Ok(())
}

fn check_param(spell: &str, param: &Parameter, at: (usize, usize)) -> Result<(), Error> {
    match param {
        Parameter::Literal(value)
            if value.is_string()
                || value.is_boolean()
                || value.as_i64().is_some()
                || value.as_u64().is_some() =>
        {
            Ok(())
        }
        Parameter::Literal(_) => Err(forbidden(
            spell,
            at,
            "a string, an integer or a boolean",
            "only scalars are values; arrays, objects and floats are not",
            "rewrite the argument as a scalar or a data key",
        )),
        Parameter::Path(path) if is_bare_key(path) => Ok(()),
        Parameter::Path(_) => Err(forbidden(
            spell,
            at,
            "a bare data key",
            "the data plane is flat: no prefix, no nesting, no index",
            "use a key defined in `[data]`",
        )),
        Parameter::Name(_) | Parameter::Subexpression(_) => Err(forbidden(
            spell,
            at,
            "a string, an integer, a boolean or a data key",
            "subexpressions and computed arguments are not part of the vocabulary",
            "pass a literal or a data key",
        )),
        _ => Err(forbidden(
            spell,
            at,
            "a string, an integer, a boolean or a data key",
            "this argument shape is not part of the vocabulary",
            "pass a literal or a data key",
        )),
    }
}

fn check_patch<'a>(
    spell: &str,
    block: &'a HelperTemplate,
    at: (usize, usize),
    next_seal: &mut usize,
) -> Result<BlockSpec<'a>, Error> {
    check_patch_shape(spell, block, at)?;
    let (selector, name) = parse_patch(spell, block, at)?;
    let body = block.template.as_ref().expect("a block always has a body");
    let body = if body.elements.iter().any(is_seal_element) {
        *next_seal += 1;
        BodySpec::Seal(exclusive_seal(spell, &body.elements, body, Container::Patch, *next_seal)?)
    } else {
        check_nodes(spell, &body.elements, body)?;
        BodySpec::Text(body)
    };
    Ok(BlockSpec { name, selector, body })
}

/// "Only" is about bytes and nodes together: a non-empty `raw` is bytes that belong to the
/// container, and a comment or expression is a node whose source would make `edit`'s slicing and
/// `absorb`'s hole disagree. A zero-byte `raw` is allowed because standalone trimming produces
/// one beside a seal on its own line without adding a byte.
fn exclusive_seal(
    spell: &str,
    elements: &[TemplateElement],
    owner: &Template,
    container: Container,
    ordinal: usize,
) -> Result<SealSite, Error> {
    let mut found = None;
    for (index, element) in elements.iter().enumerate() {
        let at = position(owner, index);
        match element {
            TemplateElement::RawString(text) if text.is_empty() => {}
            TemplateElement::HelperBlock(block) if is_seal(block) => {
                if found.is_some() {
                    return Err(second_seal(spell, at));
                }
                found = Some(check_seal(spell, block, at, container.clone(), ordinal)?);
            }
            _ => return Err(seal_not_exclusive(spell, at)),
        }
    }
    Ok(found.expect("the caller saw a seal element"))
}

/// One seal block: no arguments, a body of literal base64, and a decoded ciphertext or `None`
/// when the placeholder is empty.
fn check_seal(
    spell: &str,
    block: &HelperTemplate,
    at: (usize, usize),
    container: Container,
    ordinal: usize,
) -> Result<SealSite, Error> {
    if !block.params.is_empty()
        || !block.hash.is_empty()
        || block.block_param.is_some()
        || block.inverse.is_some()
        || block.chain
    {
        return Err(seal_with_arguments(spell, at, ordinal));
    }
    let body = block.template.as_ref().expect("a block always has a body");
    let mut text = Vec::new();
    for element in &body.elements {
        match element {
            TemplateElement::RawString(raw) => text.extend_from_slice(raw.as_bytes()),
            other => return Err(seal_body_not_literal(spell, at, ordinal, tag_kind(other))),
        }
    }
    let body = if text.is_empty() {
        Vec::new()
    } else {
        seal::decode_body(&text).ok_or_else(|| seal_body_not_base64(spell, at, ordinal, &text))?
    };
    Ok(SealSite { seal: ordinal, container, body })
}

/// The tree without rendering: what `absorb` and `edit` may write back through.
///
/// Lists the source shape and its seals without evaluating expressions or opening any seal.
pub fn outline(spell: &str, source: &[u8]) -> Result<Outline, Error> {
    let Some(template) = parse_source(spell, source)? else {
        return Ok(Outline::Plain);
    };
    Ok(match source_form(spell, source, &template)? {
        SourceForm::Text if has_expressions(&template) => Outline::Template,
        SourceForm::Text => Outline::Plain,
        SourceForm::Sealed(site) => Outline::Whole(site),
        SourceForm::Patch(specs) => Outline::Patch(
            specs
                .into_iter()
                .filter_map(|spec| match spec.body {
                    BodySpec::Seal(site) => Some(site),
                    BodySpec::Text(_) => None,
                })
                .collect(),
        ),
    })
}

/// A patch declaration and the source span used when writing its body back.
pub(crate) struct SourceBlock {
    pub(crate) name: Option<String>,
    pub(crate) block: Block,
    pub(crate) span: Range<usize>,
    pub(crate) writable: bool,
}

/// A validated source and the spans a write-back operation can replace.
pub(crate) enum SourceLayout {
    Plain,
    Template,
    Sealed(SealSite, Range<usize>),
    Patch(Vec<SourceBlock>),
}

pub(crate) fn source_layout(spell: &str, source: &[u8]) -> Result<SourceLayout, Error> {
    let Some(template) = parse_source(spell, source)? else {
        return Ok(SourceLayout::Plain);
    };
    match source_form(spell, source, &template)? {
        SourceForm::Text if has_expressions(&template) => Ok(SourceLayout::Template),
        SourceForm::Text => Ok(SourceLayout::Plain),
        SourceForm::Sealed(site) => {
            let tags = lexical::tags(source, lexical::Escapes::Grammar);
            let mut spans = body_spans(source, &tags, "seal");
            if spans.len() != 1 {
                return Err(collection_error(spell, "matching seal tags"));
            }
            Ok(SourceLayout::Sealed(site, spans.remove(0)))
        }
        SourceForm::Patch(specs) => source_blocks(spell, source, specs).map(SourceLayout::Patch),
    }
}

fn source_blocks(
    spell: &str,
    source: &[u8],
    specs: Vec<BlockSpec<'_>>,
) -> Result<Vec<SourceBlock>, Error> {
    let tags = lexical::tags(source, lexical::Escapes::Grammar);
    let spans = body_spans(source, &tags, "patch");
    let seals = body_spans(source, &tags, "seal");
    if specs.len() != spans.len() {
        return Err(collection_error(spell, "matching patch tags"));
    }
    specs
        .into_iter()
        .zip(spans)
        .enumerate()
        .map(|(index, (spec, span))| {
            let (body, seal, span, writable) = match spec.body {
                BodySpec::Text(body) => {
                    let mut literal = Vec::new();
                    let mut writable = true;
                    for element in &body.elements {
                        match element {
                            TemplateElement::RawString(text) => {
                                literal.extend_from_slice(text.as_bytes())
                            }
                            _ => {
                                literal.clear();
                                writable = false;
                                break;
                            }
                        }
                    }
                    (literal, None, span, writable)
                }
                BodySpec::Seal(site) => {
                    let span = seals
                        .get(site.seal - 1)
                        .ok_or_else(|| collection_error(spell, "matching seal tags"))?;
                    (site.body, Some(site.seal), span.clone(), true)
                }
            };
            Ok(SourceBlock {
                name: spec.name,
                block: Block { index: index + 1, selector: spec.selector, body, seal },
                span,
                writable,
            })
        })
        .collect()
}

pub(crate) fn collection_error(spell: &str, missing: &str) -> Error {
    Diag::new(Code::Ab1, spell)
        .missing(missing)
        .current("the patch body cannot be collected from the target")
        .why("the source must keep the same position and hold literal bytes after collection")
        .fix("use a literal patch body or one seal")
        .into()
}

fn body_spans(source: &[u8], tags: &[lexical::Tag], name: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut open = None;
    for tag in tags {
        match block_tag(tag.inner(source), name) {
            Some(true) => open = Some(tag.close + 2),
            Some(false) => {
                if let Some(start) = open.take() {
                    spans.push(start..tag.open);
                }
            }
            None => {}
        }
    }
    spans
}

/// The byte spans of the seal bodies in `source`, in source order.
///
/// `edit` slices a source's bodies out and writes new ones back in; the spans are what makes a
/// write land in the body and nowhere else. The walk reads tags as the grammar does, so a
/// `{{#seal}}` inside a comment or behind a backslash is text. `None` means the tags do not pair
/// — a tree could never produce that, but an edited temporary file can.
pub(crate) fn seal_spans(source: &[u8]) -> Option<Vec<std::ops::Range<usize>>> {
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    for tag in lexical::tags(source, lexical::Escapes::Wire) {
        match block_tag(tag.inner(source), "seal") {
            Some(true) => {
                if open.is_some() {
                    return None;
                }
                open = Some(tag.close + 2);
            }
            Some(false) => spans.push(open.take()?..tag.open),
            None => {}
        }
    }
    if open.is_some() {
        return None;
    }
    Some(spans)
}

/// Whether a tag is a seal's open or close, read from its inner text. `seal` takes no arguments,
/// so a tag that carries any is not a seal tag: the tree refuses that source before this runs.
fn block_tag(inner: &[u8], name: &str) -> Option<bool> {
    let (opening, found, rest) = block_name(inner)?;
    if found != name.as_bytes() || ((!opening || name == "seal") && !rest.is_empty()) {
        return None;
    }
    Some(opening)
}

fn block_name(inner: &[u8]) -> Option<(bool, &[u8], &[u8])> {
    let mut cursor = skip_air(inner, 0);
    let opening = match inner.get(cursor)? {
        b'#' => true,
        b'/' => false,
        _ => return None,
    };
    cursor = skip_air(inner, cursor + 1);
    let start = cursor;
    while inner.get(cursor).is_some_and(|&byte| is_name(byte)) {
        cursor += 1;
    }
    Some((opening, &inner[start..cursor], &inner[skip_air(inner, cursor)..]))
}

fn skip_air(inner: &[u8], mut cursor: usize) -> usize {
    while inner.get(cursor).is_some_and(|&byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'~'))
    {
        cursor += 1;
    }
    cursor
}

fn is_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

/// Encodes a whole-file literal. Binary files bypass template parsing in both directions.
pub(crate) fn literal_source(spell: &str, mut target: Vec<u8>) -> Result<Vec<u8>, Error> {
    if parsable(&target).is_some() {
        target = escape(spell, &target)?;
    }
    Ok(target)
}

/// §4.2's reverse encoding for a text hole. The grammar consumes `\{{{{` as one escape, so
/// an opening brace run is written in groups of four, then two or three, then any single brace.
///
/// The one target byte sequence with no source spelling is a backslash immediately before `{{`:
/// one backslash is consumed and two or more become a real tag. A non-UTF-8 target that contains
/// `{{` has the same problem one level up — its source would not be parsed as a template — so it
/// is refused rather than written as bytes that the next `apply` would copy verbatim.
pub fn escape(spell: &str, target: &[u8]) -> Result<Vec<u8>, Error> {
    let mut encoded = Vec::with_capacity(target.len());
    let mut offset = 0;
    while offset < target.len() {
        let rest = &target[offset..];
        if rest.starts_with(b"\\{{") {
            return Err(backslash_tag(spell, offset, target));
        }
        let count = if rest.starts_with(b"{{{{") {
            4
        } else if rest.starts_with(b"{{{") {
            3
        } else if rest.starts_with(b"{{") {
            2
        } else {
            1
        };
        if count > 1 {
            encoded.push(b'\\');
        }
        encoded.extend_from_slice(&rest[..count]);
        offset += count;
    }
    if encoded.len() != target.len() && std::str::from_utf8(&encoded).is_err() {
        return Err(binary_template(spell));
    }
    Ok(encoded)
}

fn check_patch_shape(spell: &str, block: &HelperTemplate, at: (usize, usize)) -> Result<(), Error> {
    if block.block_param.is_some() {
        return Err(forbidden(
            spell,
            at,
            "no block parameter",
            "`patch` is not an iterator",
            "remove `as |x|`",
        ));
    }
    if block.inverse.is_some() || block.chain {
        return Err(forbidden(
            spell,
            at,
            "no `{{else}}`",
            "`patch` has no conditional form",
            "remove the `else` branch",
        ));
    }
    Ok(())
}

fn parse_patch(
    spell: &str,
    block: &HelperTemplate,
    at: (usize, usize),
) -> Result<(Selector, Option<String>), Error> {
    let invalid = |current: &str| {
        forbidden(
            spell,
            at,
            "one valid patch selector",
            current,
            "use patch \"between\" after=... before=... or patch \"line\" prefix=... [insert_before=...]",
        )
    };
    let [mode] = block.params.as_slice() else {
        return Err(invalid("patch needs a single mode: between or line"));
    };
    let mode = literal_text(mode).ok_or_else(|| invalid("the mode must be a string literal"))?;
    let mut values = std::collections::BTreeMap::new();
    for (key, param) in &block.hash {
        if !matches!(key.as_str(), "after" | "before" | "prefix" | "insert_before" | "name") {
            return Err(invalid(&format!("unknown patch parameter {key}")));
        }
        let value = literal_text(param)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| invalid(&format!("{key} needs a non-empty string literal")))?;
        values.insert(key.as_str(), value);
    }
    let name = values.remove("name");
    if name.as_ref().is_some_and(|text| {
        !text.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }) {
        return Err(invalid("name uses ASCII letters, digits, underscores, or hyphens"));
    }
    let within = match (values.remove("after"), values.remove("before")) {
        (None, None) => None,
        (Some(after), Some(before)) if after != before => Some(Boundaries { after, before }),
        _ => return Err(invalid("after and before must be distinct and supplied together")),
    };
    let line_prefix =
        |text: &str| !text.starts_with([' ', '\t']) && crate::patch::single_line(text.as_bytes());
    let selector = match mode.as_str() {
        "between" if values.is_empty() => {
            within.map(Selector::Between).ok_or_else(|| invalid("between needs after and before"))
        }
        "line" => {
            let prefix = values
                .remove("prefix")
                .filter(|text| line_prefix(text))
                .ok_or_else(|| invalid("line needs an unindented single-line prefix"))?;
            let insert_before = values.remove("insert_before");
            if insert_before.as_ref().is_some_and(|text| {
                !line_prefix(text) || text.starts_with(&prefix) || prefix.starts_with(text)
            }) {
                return Err(invalid("insert_before needs a distinct, unindented line prefix"));
            }
            Ok(Selector::Line { prefix, within, insert_before })
        }
        _ => Err(invalid("choose between or line and use its supported parameters")),
    }?;
    Ok((selector, name))
}

fn literal_text(param: &Parameter) -> Option<String> {
    match param {
        Parameter::Literal(value) => value.as_str().map(str::to_owned),
        _ => None,
    }
}

fn is_bare_key(path: &Path) -> bool {
    matches!(path, Path::Relative((segments, _)) if segments.len() == 1
        && matches!(segments[0], PathSeg::Named(_)))
}

fn name_of(name: &Parameter) -> String {
    match name {
        Parameter::Name(name) => name.clone(),
        Parameter::Path(path) => match path {
            Path::Relative((segments, _)) => segments
                .iter()
                .map(|segment| match segment {
                    PathSeg::Named(name) => name.clone(),
                    _ => "?".to_string(),
                })
                .collect::<Vec<_>>()
                .join("."),
            Path::Local((_, name, _)) => name.clone(),
        },
        Parameter::Literal(value) => value.to_string(),
        Parameter::Subexpression(_) => "( ... )".to_string(),
        _ => "?".to_string(),
    }
}

fn tag_kind(element: &TemplateElement) -> String {
    match element {
        TemplateElement::RawString(_) => "text".to_string(),
        TemplateElement::HtmlExpression(_) => {
            "an unescaped expression (`{{{ ... }}}` or `{{& ...}}`)".to_string()
        }
        TemplateElement::Expression(_) => "an expression".to_string(),
        TemplateElement::HelperBlock(_) => "a block".to_string(),
        TemplateElement::DecoratorExpression(_) | TemplateElement::DecoratorBlock(_) => {
            "a decorator".to_string()
        }
        TemplateElement::PartialExpression(_) | TemplateElement::PartialBlock(_) => {
            "a partial".to_string()
        }
        TemplateElement::Comment(_) => "a comment".to_string(),
        _ => "an unknown tag".to_string(),
    }
}

fn position(template: &Template, index: usize) -> (usize, usize) {
    template.mapping.get(index).map_or((1, 1), |mapping| (mapping.0, mapping.1))
}

// ---------------------------------------------------------------------------------------------
// The three functions.
// ---------------------------------------------------------------------------------------------

struct Env;

impl HelperDef for Env {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        let name = argument(helper, 0, "env")?;
        match std::env::var(&name) {
            Ok(value) => {
                out.write(&value)?;
                Ok(())
            }
            Err(std::env::VarError::NotPresent) => {
                Err(failure(format!("environment variable `{name}` is not defined")))
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(failure(format!("environment variable `{name}` is not valid UTF-8")))
            }
        }
    }
}

struct Which;

impl HelperDef for Which {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        let name = argument(helper, 0, "which")?;
        let found = which::which(&name)
            .map_err(|e| failure(format!("no executable `{name}` on PATH: {e}")))?;
        let text = found
            .to_str()
            .ok_or_else(|| failure(format!("executable `{name}` has a path that is not UTF-8")))?;
        out.write(text)?;
        Ok(())
    }
}

struct Join;

impl HelperDef for Join {
    fn call<'reg: 'rc, 'rc>(
        &self,
        helper: &Helper<'rc>,
        _: &'reg Handlebars<'reg>,
        _: &'rc Context,
        _: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        if helper.params().len() < 2 {
            return Err(failure("`join` needs at least two path segments".to_string()));
        }
        let mut segments = Vec::with_capacity(helper.params().len());
        for index in 0..helper.params().len() {
            segments.push(argument(helper, index, "join")?);
        }
        out.write(&segments.join(MAIN_SEPARATOR_STR))?;
        Ok(())
    }
}

/// The textual form of one argument. A string keeps its bytes; an integer, a float or a boolean
/// is written as its literal text; a missing data key is a failure, never an empty string.
fn argument(helper: &Helper<'_>, index: usize, name: &str) -> Result<String, RenderError> {
    let param = helper
        .param(index)
        .ok_or_else(|| failure(format!("`{name}` is missing argument {}", index + 1)))?;
    if param.is_value_missing() {
        return Err(failure(format!(
            "`{name}` argument {} refers to a key that is not in the data plane",
            index + 1
        )));
    }
    let value = param.value();
    if let Some(text) = value.as_str() {
        return Ok(text.to_owned());
    }
    if let Some(number) = value.as_i64() {
        return Ok(number.to_string());
    }
    if let Some(number) = value.as_u64() {
        return Ok(number.to_string());
    }
    if let Some(number) = value.as_f64() {
        return Ok(number.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Ok(flag.to_string());
    }
    Err(failure(format!("`{name}` argument {} is not a scalar", index + 1)))
}

fn failure(text: String) -> RenderError {
    RenderErrorReason::Other(text).into()
}

// ---------------------------------------------------------------------------------------------
// Diagnostics.
// ---------------------------------------------------------------------------------------------

fn compile_error(spell: &str, error: &handlebars::TemplateError) -> Error {
    let at = error.pos().unwrap_or((1, 1));
    let reason = error.reason().to_string();
    Diag::new(Code::R1, spell)
        .missing("a template this form can parse")
        .current(format!("line {}, column {}: {reason}", at.0, at.1))
        .why("the syntax tree could not be built, so no form and no expected bytes exist")
        .fix("fix the tag; a literal `{{` is written `\\{{`")
        .into()
}

fn render_error(spell: &str, error: &RenderError) -> Error {
    let at = match (error.line_no, error.column_no) {
        (Some(line), Some(column)) => (line, column),
        _ => (1, 1),
    };
    let reason = error.reason().to_string();
    Diag::new(Code::R1, spell)
        .missing("a value the template refers to")
        .current(format!("line {}, column {}: {reason}", at.0, at.1))
        .why("a missing value has no bytes to write, and rendering happens before any write")
        .fix("define it in `[data]`, export the variable, or put the executable on PATH")
        .into()
}

pub(crate) fn forbidden(
    spell: &str,
    at: (usize, usize),
    missing: impl Into<String>,
    why: impl Into<String>,
    fix: impl Into<String>,
) -> Error {
    Diag::new(Code::R1, spell)
        .missing(missing)
        .current(format!("line {}, column {}: a tag this form does not define", at.0, at.1))
        .why(why)
        .fix(fix)
        .into()
}

fn empty_rendered_body(spell: &str, index: usize) -> Error {
    Diag::new(Code::Pa1, spell)
        .missing(format!("bytes in the body of patch block {index}"))
        .current("the source body is not empty, but it renders to zero bytes")
        .why("a body that renders empty would silently delete; that is the worst failure direction")
        .fix("to delete, put the two tags back to back; to write a newline, use a data key or a body line")
        .into()
}

fn seal_with_arguments(spell: &str, at: (usize, usize), seal: usize) -> Error {
    Diag::new(Code::R1, spell)
        .missing(format!("a `seal` block with no arguments (seal {seal})"))
        .current(format!("line {}, column {}: the seal has parameters", at.0, at.1))
        .why("a seal does not locate anything; its container decides which bytes it holds")
        .fix("remove the arguments and let the file or the patch block position the seal")
        .into()
}

fn second_seal(spell: &str, at: (usize, usize)) -> Error {
    Diag::new(Code::R1, spell)
        .missing("at most one seal per container")
        .current(format!("line {}, column {}: a second seal in the same container", at.0, at.1))
        .why("two seals would each claim the container's whole bytes, and neither could say which")
        .fix("keep one seal, or move the other into its own patch block")
        .into()
}

fn seal_not_exclusive(spell: &str, at: (usize, usize)) -> Error {
    Diag::new(Code::R1, spell)
        .missing("a container that holds only the seal")
        .current(format!("line {}, column {}: other content shares the seal's container", at.0, at.1))
        .why("the ciphertext holds the container's whole final bytes; absorb cannot tell which bytes belong to it")
        .fix("move the other content out of the container, or remove the seal")
        .into()
}

fn seal_body_not_literal(spell: &str, at: (usize, usize), seal: usize, kind: String) -> Error {
    Diag::new(Code::R1, spell)
        .missing(format!("literal base64 in the body of seal {seal}"))
        .current(format!("line {}, column {}: the body also holds {kind}", at.0, at.1))
        .why("a seal body is ciphertext, not a template; anything rendered inside it would change the bytes")
        .fix("replace the tag with the base64 body, or move it outside the container")
        .into()
}

fn seal_body_not_base64(spell: &str, at: (usize, usize), seal: usize, text: &[u8]) -> Error {
    Diag::new(Code::R1, spell)
        .missing(format!("a single-line base64 body for seal {seal}"))
        .current(format!(
            "line {}, column {}: {} bytes that are not canonical base64 on one line",
            at.0,
            at.1,
            text.len()
        ))
        .why("a body is base64 of the binary age output; whitespace and other bytes cannot be decrypted")
        .fix("run `tuckin edit <spell>` to write the body, or paste the base64 output of age")
        .into()
}

fn backslash_tag(spell: &str, offset: usize, target: &[u8]) -> Error {
    let (line, column) = line_column(target, offset);
    Diag::new(Code::Ab1, spell)
        .missing("a source spelling of the target bytes")
        .current(format!(
            "line {line}, column {column}: a backslash immediately before `{{` at byte {offset}"
        ))
        .why("one backslash is consumed and two or more become a real tag, so no source can produce `\\{{`")
        .fix("change the target bytes, or store the entry in a form that does not start with a backslash")
        .into()
}

fn binary_template(spell: &str) -> Error {
    Diag::new(Code::Ab1, spell)
        .missing("a UTF-8 source that can hold this target")
        .current("the target is not UTF-8 and contains `{{`")
        .why("a non-UTF-8 source is copied byte for byte, so a backslash added for `{{` would not be consumed")
        .fix("change the target bytes, or keep the entry out of the repository")
        .into()
}

fn line_column(bytes: &[u8], offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for byte in &bytes[..offset] {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::MAIN_SEPARATOR_STR;

    use super::*;

    fn plain() -> Renderer {
        Renderer::new(HashMap::<&str, &str>::new()).expect("an empty data plane serialises")
    }

    fn with_data(pairs: &[(&'static str, &'static str)]) -> Renderer {
        Renderer::new(pairs.iter().copied().collect::<HashMap<_, _>>()).expect("a data plane")
    }

    fn whole(renderer: &Renderer, source: &str) -> Vec<u8> {
        match renderer.render("home/x", source.as_bytes()) {
            Ok(Rendered::Whole(bytes)) => bytes,
            Ok(Rendered::Sealed(_)) | Ok(Rendered::Patch(_)) => panic!("expected the whole form"),
            Err(err) => panic!("{err:?}"),
        }
    }

    fn blocks(renderer: &Renderer, source: &str) -> Vec<Block> {
        match renderer.render("home/x", source.as_bytes()) {
            Ok(Rendered::Patch(blocks)) => blocks,
            Ok(Rendered::Whole(_)) | Ok(Rendered::Sealed(_)) => panic!("expected the patch form"),
            Err(err) => panic!("{err:?}"),
        }
    }

    fn seal_site(renderer: &Renderer, source: &str) -> SealSite {
        match renderer.render("home/x", source.as_bytes()) {
            Ok(Rendered::Sealed(site)) => site,
            other => panic!("expected one whole-file seal, got {other:?}"),
        }
    }

    fn refused(renderer: &Renderer, source: &str) -> Diag {
        match renderer.render("home/x", source.as_bytes()) {
            Err(Error::Diag(diag)) => diag,
            other => panic!("expected one diagnostic, got {other:?}"),
        }
    }

    // --- form and passthrough ------------------------------------------------------------------

    #[test]
    fn a_source_without_double_braces_is_copied_byte_for_byte() {
        let renderer = plain();
        assert_eq!(whole(&renderer, ""), b"");
        assert_eq!(whole(&renderer, "plain\n"), b"plain\n");
        assert_eq!(
            renderer.render("home/x", b"\xff\xfe{{not utf8").unwrap(),
            Rendered::Whole(b"\xff\xfe{{not utf8".to_vec())
        );
    }

    #[test]
    fn one_backslash_makes_a_literal_tag() {
        assert_eq!(whole(&plain(), "a\\{{x}}b"), b"a{{x}}b");
    }

    #[test]
    fn a_data_key_is_written_without_html_escaping() {
        let renderer = with_data(&[("x", "<b>&")]);
        assert_eq!(whole(&renderer, "{{x}}"), b"<b>&");
    }

    #[test]
    fn a_missing_data_key_is_r1_with_a_position() {
        let err = refused(&plain(), "a\n{{x}}");
        assert_eq!(err.code(), Code::R1);
        assert!(err.to_string().contains("line 2, column 1"));
    }

    #[test]
    fn a_source_that_is_only_a_comment_is_whole_form() {
        let renderer = plain();
        match renderer.render("home/x", b"{{! note }}").unwrap() {
            Rendered::Whole(bytes) => assert_eq!(bytes, b""),
            Rendered::Sealed(_) | Rendered::Patch(_) => {
                panic!("a comment does not make a seal or a patch file")
            }
        }
    }

    // --- the closed vocabulary -----------------------------------------------------------------

    #[test]
    fn only_the_three_names_are_callable() {
        let err = refused(&plain(), "{{range 1}}");
        assert_eq!(err.code(), Code::R1);
        assert!(err.to_string().contains("`range`"));
    }

    #[test]
    fn a_partial_is_refused_by_kind() {
        assert!(refused(&plain(), "{{> part}}").to_string().contains("a partial"));
    }

    #[test]
    fn a_decorator_is_refused_by_kind() {
        assert!(refused(&plain(), "{{* deco}}").to_string().contains("a decorator"));
    }

    #[test]
    fn triple_braces_are_refused_by_kind() {
        assert!(refused(&plain(), "{{{x}}}").to_string().contains("{{{ ... }}}"));
    }

    #[test]
    fn the_legacy_ampersand_spelling_is_refused_by_the_same_kind() {
        let renderer = with_data(&[("x", "V")]);
        let err = refused(&renderer, "{{&x}}");
        assert_eq!(err.code(), Code::R1);
        assert!(err.to_string().contains("{{& ...}}"));
    }

    #[test]
    fn a_raw_block_is_refused_as_a_block() {
        let err = refused(&plain(), "{{{{raw}}}}x{{{{/raw}}}}");
        assert!(err.to_string().contains("block `raw`"));
    }

    #[test]
    fn a_block_is_refused_in_a_whole_file() {
        assert!(refused(&plain(), "{{#if x}}a{{/if}}").to_string().contains("block `if`"));
    }

    #[test]
    fn a_hash_argument_on_a_function_is_refused() {
        let err = refused(&plain(), "{{env \"HOME\" fallback=\"x\"}}");
        assert!(err.to_string().contains("hash argument"));
    }

    #[test]
    fn a_subexpression_argument_is_refused() {
        let err = refused(&plain(), "{{join (env \"A\") \"b\"}}");
        assert!(err.to_string().contains("subexpressions"));
    }

    #[test]
    fn a_nested_key_is_refused_because_the_data_plane_is_flat() {
        assert!(refused(&plain(), "{{a.b}}").to_string().contains("flat"));
    }

    #[test]
    fn whitespace_control_is_refused_even_though_the_ast_forgets_it() {
        assert!(refused(&plain(), "a{{~x}}b").to_string().contains("`~`"));
        assert!(refused(&plain(), "a{{x~}}b").to_string().contains("`~`"));
    }

    #[test]
    fn a_tilde_inside_a_comment_is_text() {
        assert_eq!(whole(&plain(), "a{{! ~ }}b"), b"ab");
    }

    // --- the three functions -------------------------------------------------------------------

    #[test]
    fn env_reads_a_variable_that_is_defined() {
        let out = whole(&plain(), "{{env \"CARGO_PKG_NAME\"}}");
        assert_eq!(out, b"tuckin");
    }

    #[test]
    fn env_reports_a_missing_variable_as_r1() {
        let err = refused(&plain(), "{{env \"TUCKIN_NO_SUCH_VAR_5D1F\"}}");
        assert_eq!(err.code(), Code::R1);
        assert!(err.to_string().contains("TUCKIN_NO_SUCH_VAR_5D1F"));
    }

    #[test]
    fn which_writes_an_absolute_path() {
        #[cfg(windows)]
        let name = "cmd.exe";
        #[cfg(unix)]
        let name = "sh";
        let out = whole(&plain(), &format!("{{{{which \"{name}\"}}}}"));
        let text = std::str::from_utf8(&out).expect("a path is UTF-8");
        let path = std::path::Path::new(text);
        assert!(path.is_absolute(), "{path:?}");
        assert_eq!(path.file_name().and_then(|part| part.to_str()), Some(name));
    }

    #[test]
    fn which_reports_a_missing_name_as_r1() {
        let err = refused(&plain(), "{{which \"tuckin-no-such-binary-4f1c\"}}");
        assert_eq!(err.code(), Code::R1);
        assert!(err.to_string().contains("tuckin-no-such-binary-4f1c"));
    }

    #[test]
    fn join_uses_the_platform_separator() {
        let out = whole(&plain(), "{{join \"a\" \"b\" \"c\"}}");
        assert_eq!(out, format!("a{MAIN_SEPARATOR_STR}b{MAIN_SEPARATOR_STR}c").into_bytes());
    }

    #[test]
    fn join_needs_two_segments() {
        assert!(refused(&plain(), "{{join \"a\"}}").to_string().contains("at least two"));
    }

    // --- patch form ----------------------------------------------------------------------------

    #[test]
    fn patch_bodies_are_rendered_and_numbered_from_one() {
        let renderer = with_data(&[("x", "X")]);
        let blocks = blocks(
            &renderer,
            "head\n{{#patch \"between\" after=\"A\" before=\"B\"}}\nbody {{x}}\n{{/patch}}\n{{#patch \"between\" after=\"B\" before=\"C\"}}b2{{/patch}}\ntail\n",
        );
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].index, 1);
        assert_eq!(blocks[0].body, b"body X\n");
        assert_eq!(blocks[1].index, 2);
        assert_eq!(blocks[1].body, b"b2");
    }

    #[test]
    fn a_tag_outside_a_patch_block_is_refused() {
        let err =
            refused(&plain(), "{{x}}{{#patch \"between\" after=\"A\" before=\"B\"}}b{{/patch}}");
        assert!(err.to_string().contains("outside any `patch` block"));
    }

    #[test]
    fn indentation_before_a_standalone_close_tag_is_not_body() {
        let blocks = blocks(
            &plain(),
            "{{#patch \"between\" after=\"A\" before=\"B\"}}\nbody\n  {{/patch}}\n",
        );
        assert_eq!(blocks[0].body, b"body\n");
    }

    #[test]
    fn an_empty_between_body_clears_the_range() {
        let blocks = blocks(&plain(), "{{#patch \"between\" after=\"A\" before=\"B\"}}{{/patch}}");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].body, b"");
    }

    #[test]
    fn old_patch_parameters_are_refused() {
        for source in [
            r#"{{#patch at="A" to="B" done="C"}}b{{/patch}}"#,
            r#"{{#patch after="A" before="B"}}b{{/patch}}"#,
            r#"{{#patch "between" after="A" before="B" from="a"}}b{{/patch}}"#,
        ] {
            assert_eq!(refused(&plain(), source).code(), Code::R1);
        }
    }

    #[test]
    fn a_body_that_renders_empty_is_pa1() {
        let renderer = with_data(&[("x", "")]);
        let err =
            refused(&renderer, "{{#patch \"between\" after=\"A\" before=\"B\"}}{{x}}{{/patch}}");
        assert_eq!(err.code(), Code::Pa1);
        assert!(err.to_string().contains("patch block 1"));
    }

    #[test]
    fn a_body_that_is_only_a_newline_renders_empty_and_is_pa1() {
        let err = refused(&plain(), "{{#patch \"between\" after=\"A\" before=\"B\"}}\n{{/patch}}");
        assert_eq!(err.code(), Code::Pa1);
    }

    #[test]
    fn a_patch_body_may_hold_a_comment() {
        let blocks = blocks(
            &plain(),
            "{{#patch \"between\" after=\"A\" before=\"B\"}}{{! note }}b{{/patch}}",
        );
        assert_eq!(blocks[0].body, b"b");
    }

    #[test]
    fn a_patch_block_cannot_nest() {
        let err = refused(
            &plain(),
            "{{#patch \"between\" after=\"A\" before=\"B\"}}{{#patch \"between\" after=\"B\" before=\"C\"}}b{{/patch}}{{/patch}}",
        );
        assert!(err.to_string().contains("cannot nest"));
    }

    // --- seal containers ----------------------------------------------------------------------

    /// `AAAA` is canonical base64 for three zero bytes. The content layer only decodes; whether
    /// it opens is the plan's question, with the identity.
    #[test]
    fn a_whole_file_that_is_one_seal_yields_its_container_and_ciphertext() {
        let site = seal_site(&plain(), "{{#seal}}AAAA{{/seal}}");
        assert_eq!(site.seal, 1);
        assert_eq!(site.container, Container::Whole);
        assert_eq!(site.body, vec![0, 0, 0]);
    }

    #[test]
    fn a_seal_beside_any_other_node_or_byte_is_r1() {
        for source in [
            "x{{#seal}}AAAA{{/seal}}",
            "{{#seal}}AAAA{{/seal}}\n",
            "{{! note }}{{#seal}}AAAA{{/seal}}",
            "{{x}}{{#seal}}AAAA{{/seal}}",
        ] {
            let err = refused(&plain(), source);
            assert_eq!(err.code(), Code::R1, "{source:?}");
            assert!(err.to_string().contains("shares the seal"), "{source:?}");
        }
    }

    #[test]
    fn two_seals_in_one_container_are_r1() {
        let err = refused(&plain(), "{{#seal}}AAAA{{/seal}}{{#seal}}AAAA{{/seal}}");
        assert!(err.to_string().contains("second seal"));
    }

    #[test]
    fn a_seal_takes_no_arguments() {
        let err = refused(&plain(), "{{#seal name=\"x\"}}AAAA{{/seal}}");
        assert!(err.to_string().contains("seal 1"));
        assert!(err.to_string().contains("parameters"));
    }

    #[test]
    fn a_seal_body_is_literal_ciphertext() {
        let err = refused(&plain(), "{{#seal}}{{x}}{{/seal}}");
        assert!(err.to_string().contains("literal base64"));
        assert!(err.to_string().contains("seal 1"));
    }

    #[test]
    fn a_seal_body_that_is_not_canonical_base64_is_r1() {
        let err = refused(&plain(), "{{#seal}}not base64!{{/seal}}");
        assert_eq!(err.code(), Code::R1);
        assert!(err.to_string().contains("canonical base64"));
    }

    /// An unfilled placeholder is a legal source: `edit` fills it, `absorb` skips it, and the plan
    /// reports `E-E1` when a run needs bytes. The empty body plus the marker is how that state is
    /// kept distinct from a patch deletion, which is also an empty body.
    #[test]
    fn an_empty_seal_is_a_site_with_no_bytes() {
        let site = seal_site(&plain(), "{{#seal}}{{/seal}}");
        assert_eq!(site.seal, 1);
        assert!(site.body.is_empty());
        assert!(plain().render("home/x", b"{{#seal}}{{/seal}}").unwrap().sealed());
    }

    #[test]
    fn a_patch_body_seal_carries_its_interval() {
        let blocks = blocks(
            &plain(),
            "{{#patch \"between\" after=\"A\" before=\"B\"}}{{#seal}}AAAA{{/seal}}{{/patch}}",
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].seal, Some(1));
        assert_eq!(blocks[0].body, vec![0, 0, 0]);
    }

    #[test]
    fn seals_are_numbered_across_blocks_and_text_bodies_have_no_marker() {
        let blocks = blocks(
            &plain(),
            "{{#patch \"between\" after=\"A\" before=\"B\"}}{{#seal}}AAAA{{/seal}}{{/patch}}\n\
             {{#patch \"line\" prefix=\"C:\"}}text{{/patch}}\n\
             {{#patch \"between\" after=\"D\" before=\"E\"}}{{#seal}}AAAA{{/seal}}{{/patch}}\n",
        );
        assert_eq!(blocks[0].seal, Some(1));
        assert_eq!(blocks[1].seal, None);
        assert_eq!(blocks[2].seal, Some(2));
    }

    #[test]
    fn text_beside_a_patch_seal_is_r1() {
        let err = refused(
            &plain(),
            "{{#patch \"between\" after=\"A\" before=\"B\"}}x{{#seal}}AAAA{{/seal}}{{/patch}}",
        );
        assert!(err.to_string().contains("shares the seal"));
    }

    // --- outline -------------------------------------------------------------------------------

    #[test]
    fn outline_separates_plain_text_from_a_template() {
        assert_eq!(outline("home/x", b"no tags\n").unwrap(), Outline::Plain);
        assert_eq!(outline("home/x", b"\xff{{x").unwrap(), Outline::Plain);
        assert_eq!(outline("home/x", b"{{x}}\n").unwrap(), Outline::Template);
        assert_eq!(outline("home/x", b"\\{{x}}\n").unwrap(), Outline::Plain);
    }

    #[test]
    fn outline_reads_a_whole_seal_without_opening_it() {
        let outline = outline("home/x", b"{{#seal}}AAAA{{/seal}}").unwrap();
        assert_eq!(
            outline,
            Outline::Whole(SealSite { seal: 1, container: Container::Whole, body: vec![0, 0, 0] })
        );
    }

    #[test]
    fn outline_lists_patch_seal_sites_in_source_order() {
        let outline = outline(
            "home/x",
            b"{{#patch \"between\" after=\"A\" before=\"B\"}}text{{/patch}}\n\
              {{#patch \"between\" after=\"B\" before=\"C\"}}{{#seal}}AAAA{{/seal}}{{/patch}}\n\
              {{#patch \"between\" after=\"D\" before=\"E\"}}{{#seal}}{{/seal}}{{/patch}}\n",
        )
        .unwrap();
        match outline {
            Outline::Patch(sites) => {
                assert_eq!(sites.len(), 2);
                assert_eq!(sites[0].seal, 1);
                assert_eq!(sites[0].body, vec![0, 0, 0]);
                assert_eq!(sites[1].seal, 2);
                assert!(sites[1].body.is_empty());
                assert_eq!(sites[1].container, Container::Patch);
            }
            other => panic!("expected patch sites, got {other:?}"),
        }
    }

    #[test]
    fn outline_checks_structure_but_not_data() {
        assert_eq!(outline("home/x", b"{{missing}}").unwrap(), Outline::Template);
        assert_eq!(
            outline("home/x", b"{{#patch \"between\" after=\"A\" before=\"B\"}}b{{/patch}}")
                .unwrap(),
            Outline::Patch(vec![])
        );
    }

    // --- reverse encoding ----------------------------------------------------------------------

    #[test]
    fn escape_adds_one_backslash_before_each_tag() {
        assert_eq!(escape("home/x", b"a{{b").unwrap(), b"a\\{{b");
        assert_eq!(escape("home/x", b"{{a}}{{b}}").unwrap(), b"\\{{a}}\\{{b}}");
    }

    #[test]
    fn escape_leaves_other_bytes_alone() {
        assert_eq!(escape("home/x", b"a\\b\n").unwrap(), b"a\\b\n");
        assert_eq!(escape("home/x", b"\xff\xfe").unwrap(), b"\xff\xfe");
        assert_eq!(escape("home/x", b"").unwrap(), b"");
    }

    #[test]
    fn an_escaped_and_rendered_target_is_the_target_again() {
        let target: &[u8] = b"discard = true\n{{ this was syntax }}\n";
        let source = escape("home/x", target).unwrap();
        assert_eq!(whole(&plain(), std::str::from_utf8(&source).unwrap()), target);
        for count in 2..=12 {
            let target = format!("before {} after", "{".repeat(count));
            let source = escape("home/x", target.as_bytes()).unwrap();
            assert_eq!(whole(&plain(), std::str::from_utf8(&source).unwrap()), target.as_bytes());
        }
    }

    /// `\{{` is the one byte sequence with no source: the backslash is consumed by the escape
    /// and two backslashes would become a real tag.
    #[test]
    fn a_backslash_before_a_tag_has_no_source_spelling() {
        let err = match escape("home/x", b"a\n\\{{b") {
            Err(Error::Diag(diag)) => diag,
            other => panic!("expected one diagnostic, got {other:?}"),
        };
        assert_eq!(err.code(), Code::Ab1);
        assert!(err.to_string().contains("line 2, column 1"), "{err}");
    }

    #[test]
    fn a_non_utf8_target_with_a_tag_has_no_source_spelling() {
        let err = match escape("home/x", b"\xff{{b") {
            Err(Error::Diag(diag)) => diag,
            other => panic!("expected one diagnostic, got {other:?}"),
        };
        assert_eq!(err.code(), Code::Ab1);
        assert!(err.to_string().contains("not UTF-8"));
    }
}
