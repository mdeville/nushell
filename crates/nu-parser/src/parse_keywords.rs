use log::trace;
use nu_path::canonicalize_with;
use nu_protocol::{
    ast::{
        Argument, Block, Call, Expr, Expression, ImportPattern, ImportPatternHead,
        ImportPatternMember, PathMember, Pipeline, PipelineElement,
    },
    engine::{StateWorkingSet, DEFAULT_OVERLAY_NAME},
    span, Alias, BlockId, Exportable, Module, PositionalArg, Span, Spanned, SyntaxShape, Type,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

static LIB_DIRS_ENV: &str = "NU_LIB_DIRS";
#[cfg(feature = "plugin")]
static PLUGIN_DIRS_ENV: &str = "NU_PLUGIN_DIRS";

use crate::{
    eval::{eval_constant, value_as_string},
    known_external::KnownExternal,
    lex,
    lite_parser::{lite_parse, LiteCommand, LiteElement},
    parser::{
        check_call, check_name, garbage, garbage_pipeline, parse, parse_call, parse_import_pattern,
        parse_internal_call, parse_multispan_value, parse_signature, parse_string, parse_value,
        parse_var_with_opt_type, trim_quotes, ParsedInternalCall,
    },
    unescape_unquote_string, ParseError, Token, TokenContents,
};

/// These parser keywords can be aliased
pub const ALIASABLE_PARSER_KEYWORDS: &[&[u8]] = &[b"overlay hide", b"overlay new", b"overlay use"];

/// These parser keywords cannot be aliased (either not possible, or support not yet added)
pub const UNALIASABLE_PARSER_KEYWORDS: &[&[u8]] = &[
    b"export",
    b"def",
    b"export def",
    b"for",
    b"extern",
    b"export extern",
    b"alias",
    b"export alias",
    b"export-env",
    b"module",
    b"use",
    b"export use",
    b"hide",
    // b"overlay",
    // b"overlay hide",
    // b"overlay new",
    // b"overlay use",
    b"let",
    b"const",
    b"mut",
    b"source",
    b"where",
    b"register",
];

/// Check whether spans start with a parser keyword that can be aliased
pub fn is_unaliasable_parser_keyword(working_set: &StateWorkingSet, spans: &[Span]) -> bool {
    // try two words
    if let (Some(span1), Some(span2)) = (spans.get(0), spans.get(1)) {
        let cmd_name = working_set.get_span_contents(span(&[*span1, *span2]));
        return UNALIASABLE_PARSER_KEYWORDS.contains(&cmd_name);
    }

    // try one word
    if let Some(span1) = spans.get(0) {
        let cmd_name = working_set.get_span_contents(*span1);
        UNALIASABLE_PARSER_KEYWORDS.contains(&cmd_name)
    } else {
        false
    }
}

/// This is a new more compact method of calling parse_xxx() functions without repeating the
/// parse_call() in each function. Remaining keywords can be moved here.
pub fn parse_keyword(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    expand_aliases_denylist: &[usize],
    is_subexpression: bool,
) -> (Pipeline, Option<ParseError>) {
    let (call_expr, err) = parse_call(
        working_set,
        &lite_command.parts,
        lite_command.parts[0],
        expand_aliases_denylist,
        is_subexpression,
    );

    if err.is_some() {
        return (Pipeline::from_vec(vec![call_expr]), err);
    }

    if let Expression {
        expr: Expr::Call(call),
        ..
    } = call_expr.clone()
    {
        // Apply parse keyword side effects
        let cmd = working_set.get_decl(call.decl_id);

        match cmd.name() {
            "overlay hide" => parse_overlay_hide(working_set, call),
            "overlay new" => parse_overlay_new(working_set, call),
            "overlay use" => parse_overlay_use(working_set, call, expand_aliases_denylist),
            _ => (Pipeline::from_vec(vec![call_expr]), err),
        }
    } else {
        (Pipeline::from_vec(vec![call_expr]), err)
    }
}

pub fn parse_def_predecl(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> Option<ParseError> {
    let name = working_set.get_span_contents(spans[0]);

    // handle "export def" same as "def"
    let (name, spans) = if name == b"export" && spans.len() >= 2 {
        (working_set.get_span_contents(spans[1]), &spans[1..])
    } else {
        (name, spans)
    };

    if (name == b"def" || name == b"def-env") && spans.len() >= 4 {
        let (name_expr, ..) = parse_string(working_set, spans[1], expand_aliases_denylist);
        let name = name_expr.as_string();

        working_set.enter_scope();
        // FIXME: because parse_signature will update the scope with the variables it sees
        // we end up parsing the signature twice per def. The first time is during the predecl
        // so that we can see the types that are part of the signature, which we need for parsing.
        // The second time is when we actually parse the body itworking_set.
        // We can't reuse the first time because the variables that are created during parse_signature
        // are lost when we exit the scope below.
        let (sig, ..) = parse_signature(working_set, spans[2], expand_aliases_denylist);
        let signature = sig.as_signature();
        working_set.exit_scope();
        if let (Some(name), Some(mut signature)) = (name, signature) {
            if name.contains('#')
                || name.contains('^')
                || name.parse::<bytesize::ByteSize>().is_ok()
                || name.parse::<f64>().is_ok()
            {
                return Some(ParseError::CommandDefNotValid(spans[1]));
            }

            signature.name = name;
            let decl = signature.predeclare();

            if working_set.add_predecl(decl).is_some() {
                return Some(ParseError::DuplicateCommandDef(spans[1]));
            }
        }
    } else if name == b"extern" && spans.len() == 3 {
        let (name_expr, ..) = parse_string(working_set, spans[1], expand_aliases_denylist);
        let name = name_expr.as_string();

        working_set.enter_scope();
        // FIXME: because parse_signature will update the scope with the variables it sees
        // we end up parsing the signature twice per def. The first time is during the predecl
        // so that we can see the types that are part of the signature, which we need for parsing.
        // The second time is when we actually parse the body itworking_set.
        // We can't reuse the first time because the variables that are created during parse_signature
        // are lost when we exit the scope below.
        let (sig, ..) = parse_signature(working_set, spans[2], expand_aliases_denylist);
        let signature = sig.as_signature();
        working_set.exit_scope();

        if let (Some(name), Some(mut signature)) = (name, signature) {
            if name.contains('#')
                || name.parse::<bytesize::ByteSize>().is_ok()
                || name.parse::<f64>().is_ok()
            {
                return Some(ParseError::CommandDefNotValid(spans[1]));
            }

            signature.name = name.clone();
            //let decl = signature.predeclare();
            let decl = KnownExternal {
                name,
                usage: "run external command".into(),
                signature,
            };

            if working_set.add_predecl(Box::new(decl)).is_some() {
                return Some(ParseError::DuplicateCommandDef(spans[1]));
            }
        }
    } else if name == b"alias" && spans.len() >= 4 {
        let (name_expr, ..) = parse_string(working_set, spans[1], expand_aliases_denylist);
        let name = name_expr.as_string();

        if let Some(name) = name {
            if name.contains('#')
                || name.contains('^')
                || name.parse::<bytesize::ByteSize>().is_ok()
                || name.parse::<f64>().is_ok()
            {
                return Some(ParseError::CommandDefNotValid(spans[1]));
            }

            // The signature will get replaced by the replacement signature
            // let mut signature = Signature::new(name.clone());
            // signature.name = name;

            // The fields get replaced during parsing
            let decl = Alias {
                name,
                command: None,
                wrapped_call: Expression::garbage(name_expr.span),
            };

            if working_set.add_predecl(Box::new(decl)).is_some() {
                return Some(ParseError::DuplicateCommandDef(spans[1]));
            }
        }
    }

    None
}

pub fn parse_for(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Expression, Option<ParseError>) {
    // Checking that the function is used with the correct name
    // Maybe this is not necessary but it is a sanity check
    if working_set.get_span_contents(spans[0]) != b"for" {
        return (
            garbage(spans[0]),
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for 'for' function".into(),
                span(spans),
            )),
        );
    }

    // Parsing the spans and checking that they match the register signature
    // Using a parsed call makes more sense than checking for how many spans are in the call
    // Also, by creating a call, it can be checked if it matches the declaration signature
    let (call, call_span) = match working_set.find_decl(b"for", &Type::Any) {
        None => {
            return (
                garbage(spans[0]),
                Some(ParseError::UnknownState(
                    "internal error: for declaration not found".into(),
                    span(spans),
                )),
            )
        }
        Some(decl_id) => {
            working_set.enter_scope();
            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );

            working_set.exit_scope();

            let call_span = span(spans);
            let decl = working_set.get_decl(decl_id);
            let sig = decl.signature();

            // Let's get our block and make sure it has the right signature
            if let Some(arg) = call.positional_nth(2) {
                match arg {
                    Expression {
                        expr: Expr::Block(block_id),
                        ..
                    }
                    | Expression {
                        expr: Expr::RowCondition(block_id),
                        ..
                    } => {
                        let block = working_set.get_block_mut(*block_id);

                        block.signature = Box::new(sig.clone());
                    }
                    _ => {}
                }
            }

            err = check_call(call_span, &sig, &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    },
                    err,
                );
            }

            (call, call_span)
        }
    };

    // All positional arguments must be in the call positional vector by this point
    let var_decl = call.positional_nth(0).expect("for call already checked");
    let block = call.positional_nth(2).expect("for call already checked");

    let error = None;
    if let (Some(var_id), Some(block_id)) = (&var_decl.as_var(), block.as_block()) {
        let block = working_set.get_block_mut(block_id);

        block.signature.required_positional.insert(
            0,
            PositionalArg {
                name: String::new(),
                desc: String::new(),
                shape: SyntaxShape::Any,
                var_id: Some(*var_id),
                default_value: None,
            },
        );
    }

    (
        Expression {
            expr: Expr::Call(call),
            span: call_span,
            ty: Type::Any,
            custom_completion: None,
        },
        error,
    )
}

pub fn parse_def(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    module_name: Option<&[u8]>,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let spans = &lite_command.parts[..];

    let (usage, extra_usage) = working_set.build_usage(&lite_command.comments);

    // Checking that the function is used with the correct name
    // Maybe this is not necessary but it is a sanity check
    // Note: "export def" is treated the same as "def"

    let (name_span, split_id) =
        if spans.len() > 1 && working_set.get_span_contents(spans[0]) == b"export" {
            (spans[1], 2)
        } else {
            (spans[0], 1)
        };

    let def_call = working_set.get_span_contents(name_span).to_vec();
    if def_call != b"def" && def_call != b"def-env" {
        return (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for def function".into(),
                span(spans),
            )),
        );
    }

    // Parsing the spans and checking that they match the register signature
    // Using a parsed call makes more sense than checking for how many spans are in the call
    // Also, by creating a call, it can be checked if it matches the declaration signature
    let (call, call_span) = match working_set.find_decl(&def_call, &Type::Any) {
        None => {
            return (
                garbage_pipeline(spans),
                Some(ParseError::UnknownState(
                    "internal error: def declaration not found".into(),
                    span(spans),
                )),
            )
        }
        Some(decl_id) => {
            working_set.enter_scope();
            let (command_spans, rest_spans) = spans.split_at(split_id);
            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                span(command_spans),
                rest_spans,
                decl_id,
                expand_aliases_denylist,
            );

            working_set.exit_scope();

            let call_span = span(spans);
            let decl = working_set.get_decl(decl_id);
            let sig = decl.signature();

            // Let's get our block and make sure it has the right signature
            if let Some(arg) = call.positional_nth(2) {
                match arg {
                    Expression {
                        expr: Expr::Block(block_id),
                        ..
                    }
                    | Expression {
                        expr: Expr::RowCondition(block_id),
                        ..
                    } => {
                        let block = working_set.get_block_mut(*block_id);

                        block.signature = Box::new(sig.clone());
                    }
                    _ => {}
                }
            }

            err = check_call(call_span, &sig, &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    }]),
                    err,
                );
            }

            (call, call_span)
        }
    };

    // All positional arguments must be in the call positional vector by this point
    let name_expr = call.positional_nth(0).expect("def call already checked");
    let sig = call.positional_nth(1).expect("def call already checked");
    let block = call.positional_nth(2).expect("def call already checked");

    let mut error = None;

    let name = if let Some(name) = name_expr.as_string() {
        if let Some(mod_name) = module_name {
            if name.as_bytes() == mod_name {
                let name_expr_span = name_expr.span;

                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: Type::Any,
                        custom_completion: None,
                    }]),
                    Some(ParseError::NamedAsModule(
                        "command".to_string(),
                        name,
                        name_expr_span,
                    )),
                );
            }
        }

        name
    } else {
        return (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "Could not get string from string expression".into(),
                name_expr.span,
            )),
        );
    };

    if let (Some(mut signature), Some(block_id)) = (sig.as_signature(), block.as_block()) {
        if let Some(decl_id) = working_set.find_predecl(name.as_bytes()) {
            let declaration = working_set.get_decl_mut(decl_id);

            signature.name = name.clone();
            *signature = signature.add_help();
            signature.usage = usage;
            signature.extra_usage = extra_usage;

            *declaration = signature.clone().into_block_command(block_id);

            let mut block = working_set.get_block_mut(block_id);
            let calls_itself = block.pipelines.iter().any(|pipeline| {
                pipeline
                    .elements
                    .iter()
                    .any(|pipe_element| match pipe_element {
                        PipelineElement::Expression(
                            _,
                            Expression {
                                expr: Expr::Call(call_expr),
                                ..
                            },
                        ) => {
                            if call_expr.decl_id == decl_id {
                                return true;
                            }
                            call_expr.arguments.iter().any(|arg| match arg {
                                Argument::Positional(Expression { expr, .. }) => match expr {
                                    Expr::Keyword(.., expr) => {
                                        let expr = expr.as_ref();
                                        let Expression { expr, .. } = expr;
                                        match expr {
                                            Expr::Call(call_expr2) => call_expr2.decl_id == decl_id,
                                            _ => false,
                                        }
                                    }
                                    Expr::Call(call_expr2) => call_expr2.decl_id == decl_id,
                                    _ => false,
                                },
                                _ => false,
                            })
                        }
                        _ => false,
                    })
            });
            block.recursive = Some(calls_itself);
            block.signature = signature;
            block.redirect_env = def_call == b"def-env";
        } else {
            error = error.or_else(|| {
                Some(ParseError::InternalError(
                    "Predeclaration failed to add declaration".into(),
                    name_expr.span,
                ))
            });
        };
    }

    // It's OK if it returns None: The decl was already merged in previous parse pass.
    working_set.merge_predecl(name.as_bytes());

    (
        Pipeline::from_vec(vec![Expression {
            expr: Expr::Call(call),
            span: call_span,
            ty: Type::Any,
            custom_completion: None,
        }]),
        error,
    )
}

pub fn parse_extern(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    module_name: Option<&[u8]>,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let spans = &lite_command.parts;
    let mut error = None;

    let (usage, extra_usage) = working_set.build_usage(&lite_command.comments);

    // Checking that the function is used with the correct name
    // Maybe this is not necessary but it is a sanity check

    let (name_span, split_id) =
        if spans.len() > 1 && working_set.get_span_contents(spans[0]) == b"export" {
            (spans[1], 2)
        } else {
            (spans[0], 1)
        };

    let extern_call = working_set.get_span_contents(name_span).to_vec();
    if extern_call != b"extern" {
        return (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for extern function".into(),
                span(spans),
            )),
        );
    }

    // Parsing the spans and checking that they match the register signature
    // Using a parsed call makes more sense than checking for how many spans are in the call
    // Also, by creating a call, it can be checked if it matches the declaration signature
    let (call, call_span) = match working_set.find_decl(&extern_call, &Type::Any) {
        None => {
            return (
                garbage_pipeline(spans),
                Some(ParseError::UnknownState(
                    "internal error: def declaration not found".into(),
                    span(spans),
                )),
            )
        }
        Some(decl_id) => {
            working_set.enter_scope();

            let (command_spans, rest_spans) = spans.split_at(split_id);

            let ParsedInternalCall {
                call, error: err, ..
            } = parse_internal_call(
                working_set,
                span(command_spans),
                rest_spans,
                decl_id,
                expand_aliases_denylist,
            );
            working_set.exit_scope();

            error = error.or(err);

            let call_span = span(spans);
            //let decl = working_set.get_decl(decl_id);
            //let sig = decl.signature();

            (call, call_span)
        }
    };
    let name_expr = call.positional_nth(0);
    let sig = call.positional_nth(1);

    if let (Some(name_expr), Some(sig)) = (name_expr, sig) {
        if let (Some(name), Some(mut signature)) = (&name_expr.as_string(), sig.as_signature()) {
            if let Some(mod_name) = module_name {
                if name.as_bytes() == mod_name {
                    let name_expr_span = name_expr.span;
                    return (
                        Pipeline::from_vec(vec![Expression {
                            expr: Expr::Call(call),
                            span: call_span,
                            ty: Type::Any,
                            custom_completion: None,
                        }]),
                        Some(ParseError::NamedAsModule(
                            "known external".to_string(),
                            name.clone(),
                            name_expr_span,
                        )),
                    );
                }
            }

            if let Some(decl_id) = working_set.find_predecl(name.as_bytes()) {
                let declaration = working_set.get_decl_mut(decl_id);

                let external_name = if let Some(mod_name) = module_name {
                    if name.as_bytes() == b"main" {
                        String::from_utf8_lossy(mod_name).to_string()
                    } else {
                        name.clone()
                    }
                } else {
                    name.clone()
                };

                signature.name = external_name.clone();
                signature.usage = usage.clone();
                signature.extra_usage = extra_usage.clone();
                signature.allows_unknown_args = true;

                let decl = KnownExternal {
                    name: external_name,
                    usage: [usage, extra_usage].join("\n"),
                    signature,
                };

                *declaration = Box::new(decl);
            } else {
                error = error.or_else(|| {
                    Some(ParseError::InternalError(
                        "Predeclaration failed to add declaration".into(),
                        spans[split_id],
                    ))
                });
            };
        }
        if let Some(name) = name_expr.as_string() {
            // It's OK if it returns None: The decl was already merged in previous parse pass.
            working_set.merge_predecl(name.as_bytes());
        } else {
            error = error.or_else(|| {
                Some(ParseError::UnknownState(
                    "Could not get string from string expression".into(),
                    name_expr.span,
                ))
            });
        }
    }

    (
        Pipeline::from_vec(vec![Expression {
            expr: Expr::Call(call),
            span: call_span,
            ty: Type::Any,
            custom_completion: None,
        }]),
        error,
    )
}

pub fn parse_alias(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    module_name: Option<&[u8]>,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let spans = &lite_command.parts;

    let (name_span, split_id) =
        if spans.len() > 1 && working_set.get_span_contents(spans[0]) == b"export" {
            (spans[1], 2)
        } else {
            (spans[0], 1)
        };

    let name = working_set.get_span_contents(name_span);

    if name != b"alias" {
        return (
            garbage_pipeline(spans),
            Some(ParseError::InternalError(
                "Alias statement unparsable".into(),
                span(spans),
            )),
        );
    }

    if let Some((span, err)) = check_name(working_set, spans) {
        return (Pipeline::from_vec(vec![garbage(*span)]), Some(err));
    }

    if let Some(decl_id) = working_set.find_decl(b"alias", &Type::Any) {
        let (command_spans, rest_spans) = spans.split_at(split_id);

        let ParsedInternalCall {
            call: alias_call,
            output,
            ..
        } = parse_internal_call(
            working_set,
            span(command_spans),
            rest_spans,
            decl_id,
            expand_aliases_denylist,
        );

        let has_help_flag = alias_call.has_flag("help");

        let alias_pipeline = Pipeline::from_vec(vec![Expression {
            expr: Expr::Call(alias_call),
            span: span(spans),
            ty: output,
            custom_completion: None,
        }]);

        if has_help_flag {
            return (alias_pipeline, None);
        }

        if spans.len() >= split_id + 3 {
            let alias_name = working_set.get_span_contents(spans[split_id]);

            let alias_name = if alias_name.starts_with(b"\"")
                && alias_name.ends_with(b"\"")
                && alias_name.len() > 1
            {
                alias_name[1..(alias_name.len() - 1)].to_vec()
            } else {
                alias_name.to_vec()
            };

            if let Some(mod_name) = module_name {
                if alias_name == mod_name {
                    return (
                        alias_pipeline,
                        Some(ParseError::NamedAsModule(
                            "alias".to_string(),
                            String::from_utf8_lossy(&alias_name).to_string(),
                            spans[split_id],
                        )),
                    );
                }

                if &alias_name == b"main" {
                    return (
                        alias_pipeline,
                        Some(ParseError::ExportMainAliasNotAllowed(spans[split_id])),
                    );
                }
            }

            let _equals = working_set.get_span_contents(spans[split_id + 1]);

            let replacement_spans = &spans[(split_id + 2)..];

            let (expr, err) = parse_call(
                working_set,
                replacement_spans,
                replacement_spans[0],
                expand_aliases_denylist,
                false, // TODO: Should this be set properly???
            );

            if let Some(e) = err {
                if let ParseError::MissingPositional(..) = e {
                    // ignore missing required positional
                } else {
                    return (garbage_pipeline(replacement_spans), Some(e));
                }
            }

            let (command, wrapped_call) = match expr {
                Expression {
                    expr: Expr::Call(ref rhs_call),
                    ..
                } => {
                    let cmd = working_set.get_decl(rhs_call.decl_id);

                    if cmd.is_parser_keyword()
                        && !ALIASABLE_PARSER_KEYWORDS.contains(&cmd.name().as_bytes())
                    {
                        return (
                            alias_pipeline,
                            Some(ParseError::CantAliasKeyword(
                                ALIASABLE_PARSER_KEYWORDS
                                    .iter()
                                    .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                                    .collect::<Vec<String>>()
                                    .join(", "),
                                rhs_call.head,
                            )),
                        );
                    }

                    (Some(cmd.clone_box()), expr)
                }
                Expression {
                    expr: Expr::ExternalCall(..),
                    ..
                } => (None, expr),
                _ => {
                    return (
                        alias_pipeline,
                        Some(ParseError::InternalError(
                            "Parsed call not a call".into(),
                            expr.span,
                        )),
                    )
                }
            };

            if let Some(decl_id) = working_set.find_predecl(&alias_name) {
                let alias_decl = working_set.get_decl_mut(decl_id);

                let alias = Alias {
                    name: String::from_utf8_lossy(&alias_name).to_string(),
                    command,
                    wrapped_call,
                };

                *alias_decl = Box::new(alias);
            } else {
                return (
                    garbage_pipeline(spans),
                    Some(ParseError::InternalError(
                        "Predeclaration failed to add declaration".into(),
                        spans[split_id],
                    )),
                );
            }

            // It's OK if it returns None: The decl was already merged in previous parse pass.
            working_set.merge_predecl(&alias_name);
        }

        let err = if spans.len() < 4 {
            Some(ParseError::IncorrectValue(
                "Incomplete alias".into(),
                span(&spans[..split_id]),
                "incomplete alias".into(),
            ))
        } else {
            None
        };

        return (alias_pipeline, err);
    }

    (
        garbage_pipeline(spans),
        Some(ParseError::InternalError(
            "Alias statement unparsable".into(),
            span(spans),
        )),
    )
}

pub fn parse_old_alias(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    module_name: Option<&[u8]>,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let spans = &lite_command.parts;

    // if the call is "alias", turn it into "print $nu.scope.aliases"
    if spans.len() == 1 {
        let head = Expression {
            expr: Expr::Var(nu_protocol::NU_VARIABLE_ID),
            span: Span::new(0, 0),
            ty: Type::Any,
            custom_completion: None,
        };
        let tail = vec![
            PathMember::String {
                val: "scope".to_string(),
                span: Span::new(0, 0),
            },
            PathMember::String {
                val: "aliases".to_string(),
                span: Span::new(0, 0),
            },
        ];
        let expr = Expression {
            ty: Type::Any,
            expr: Expr::FullCellPath(Box::new(nu_protocol::ast::FullCellPath { head, tail })),
            span: Span::new(0, 0),
            custom_completion: None,
        };
        if let Some(decl_id) = working_set.find_decl(b"print", &Type::Any) {
            let print_call = Expr::Call(Box::new(Call {
                head: spans[0],
                arguments: vec![Argument::Positional(expr)],
                decl_id,
                redirect_stdout: true,
                redirect_stderr: false,
                parser_info: vec![],
            }));
            return (
                Pipeline::from_vec(vec![Expression {
                    expr: print_call,
                    span: spans[0],
                    ty: Type::Any,
                    custom_completion: None,
                }]),
                None,
            );
        }
        return (Pipeline::from_vec(vec![expr]), None);
    }

    let (name_span, split_id) =
        if spans.len() > 1 && working_set.get_span_contents(spans[0]) == b"export" {
            (spans[1], 2)
        } else {
            (spans[0], 1)
        };

    let name = working_set.get_span_contents(name_span);

    if name == b"old-alias" {
        if let Some((span, err)) = check_name(working_set, spans) {
            return (Pipeline::from_vec(vec![garbage(*span)]), Some(err));
        }

        if let Some(decl_id) = working_set.find_decl(b"alias", &Type::Any) {
            let (command_spans, rest_spans) = spans.split_at(split_id);

            let ParsedInternalCall { call, output, .. } = parse_internal_call(
                working_set,
                span(command_spans),
                rest_spans,
                decl_id,
                expand_aliases_denylist,
            );

            if call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: span(spans),
                        ty: output,
                        custom_completion: None,
                    }]),
                    None,
                );
            }

            if spans.len() >= split_id + 3 {
                let alias_name = working_set.get_span_contents(spans[split_id]);

                let alias_name = if alias_name.starts_with(b"\"")
                    && alias_name.ends_with(b"\"")
                    && alias_name.len() > 1
                {
                    alias_name[1..(alias_name.len() - 1)].to_vec()
                } else {
                    alias_name.to_vec()
                };

                if let Some(mod_name) = module_name {
                    if alias_name == mod_name {
                        return (
                            Pipeline::from_vec(vec![Expression {
                                expr: Expr::Call(call),
                                span: span(spans),
                                ty: output,
                                custom_completion: None,
                            }]),
                            Some(ParseError::NamedAsModule(
                                "alias".to_string(),
                                String::from_utf8_lossy(&alias_name).to_string(),
                                spans[split_id],
                            )),
                        );
                    }

                    if &alias_name == b"main" {
                        return (
                            Pipeline::from_vec(vec![Expression {
                                expr: Expr::Call(call),
                                span: span(spans),
                                ty: output,
                                custom_completion: None,
                            }]),
                            Some(ParseError::ExportMainAliasNotAllowed(spans[split_id])),
                        );
                    }
                }

                let _equals = working_set.get_span_contents(spans[split_id + 1]);

                let replacement = spans[(split_id + 2)..].to_vec();

                let checked_name = String::from_utf8_lossy(&alias_name);
                if checked_name.contains('#')
                    || checked_name.contains('^')
                    || checked_name.parse::<bytesize::ByteSize>().is_ok()
                    || checked_name.parse::<f64>().is_ok()
                {
                    return (
                        Pipeline::from_vec(vec![garbage(name_span)]),
                        Some(ParseError::AliasNotValid(name_span)),
                    );
                }

                working_set.add_alias(alias_name, replacement, lite_command.comments.clone());
            }

            let err = if spans.len() < 4 {
                Some(ParseError::IncorrectValue(
                    "Incomplete alias".into(),
                    span(&spans[..split_id]),
                    "incomplete alias".into(),
                ))
            } else {
                None
            };

            return (
                Pipeline::from_vec(vec![Expression {
                    expr: Expr::Call(call),
                    span: span(spans),
                    ty: Type::Any,
                    custom_completion: None,
                }]),
                err,
            );
        }
    }

    (
        garbage_pipeline(spans),
        Some(ParseError::InternalError(
            "Alias statement unparsable".into(),
            span(spans),
        )),
    )
}

// This one will trigger if `export` appears during eval, e.g., in a script
pub fn parse_export_in_block(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let call_span = span(&lite_command.parts);

    let full_name = if lite_command.parts.len() > 1 {
        let sub = working_set.get_span_contents(lite_command.parts[1]);
        match sub {
            b"old-alias" | b"alias" | b"def" | b"def-env" | b"extern" | b"use" => {
                [b"export ", sub].concat()
            }
            _ => b"export".to_vec(),
        }
    } else {
        b"export".to_vec()
    };

    if let Some(decl_id) = working_set.find_decl(&full_name, &Type::Any) {
        let ParsedInternalCall {
            call,
            error: mut err,
            output,
            ..
        } = parse_internal_call(
            working_set,
            if full_name == b"export" {
                lite_command.parts[0]
            } else {
                span(&lite_command.parts[0..2])
            },
            if full_name == b"export" {
                &lite_command.parts[1..]
            } else {
                &lite_command.parts[2..]
            },
            decl_id,
            expand_aliases_denylist,
        );

        let decl = working_set.get_decl(decl_id);
        err = check_call(call_span, &decl.signature(), &call).or(err);

        if err.is_some() || call.has_flag("help") {
            return (
                Pipeline::from_vec(vec![Expression {
                    expr: Expr::Call(call),
                    span: call_span,
                    ty: output,
                    custom_completion: None,
                }]),
                err,
            );
        }
    } else {
        return (
            garbage_pipeline(&lite_command.parts),
            Some(ParseError::UnknownState(
                format!(
                    "internal error: '{}' declaration not found",
                    String::from_utf8_lossy(&full_name)
                ),
                span(&lite_command.parts),
            )),
        );
    };

    if &full_name == b"export" {
        // export by itself is meaningless
        return (
            garbage_pipeline(&lite_command.parts),
            Some(ParseError::UnexpectedKeyword(
                "export".into(),
                lite_command.parts[0],
            )),
        );
    }

    match full_name.as_slice() {
        b"export old-alias" => {
            parse_old_alias(working_set, lite_command, None, expand_aliases_denylist)
        }
        b"export alias" => parse_alias(working_set, lite_command, None, expand_aliases_denylist),
        b"export def" | b"export def-env" => {
            parse_def(working_set, lite_command, None, expand_aliases_denylist)
        }
        b"export use" => {
            let (pipeline, _, err) =
                parse_use(working_set, &lite_command.parts, expand_aliases_denylist);
            (pipeline, err)
        }
        b"export extern" => parse_extern(working_set, lite_command, None, expand_aliases_denylist),
        _ => (
            garbage_pipeline(&lite_command.parts),
            Some(ParseError::UnexpectedKeyword(
                String::from_utf8_lossy(&full_name).to_string(),
                lite_command.parts[0],
            )),
        ),
    }
}

// This one will trigger only in a module
pub fn parse_export_in_module(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    module_name: &[u8],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Vec<Exportable>, Option<ParseError>) {
    let spans = &lite_command.parts[..];
    let mut error = None;

    let export_span = if let Some(sp) = spans.get(0) {
        if working_set.get_span_contents(*sp) != b"export" {
            return (
                garbage_pipeline(spans),
                vec![],
                Some(ParseError::UnknownState(
                    "expected export statement".into(),
                    span(spans),
                )),
            );
        }

        *sp
    } else {
        return (
            garbage_pipeline(spans),
            vec![],
            Some(ParseError::UnknownState(
                "got empty input for parsing export statement".into(),
                span(spans),
            )),
        );
    };

    let export_decl_id = if let Some(id) = working_set.find_decl(b"export", &Type::Any) {
        id
    } else {
        return (
            garbage_pipeline(spans),
            vec![],
            Some(ParseError::InternalError(
                "missing export command".into(),
                export_span,
            )),
        );
    };

    let mut call = Box::new(Call {
        head: spans[0],
        decl_id: export_decl_id,
        arguments: vec![],
        redirect_stdout: true,
        redirect_stderr: false,
        parser_info: vec![],
    });

    let exportables = if let Some(kw_span) = spans.get(1) {
        let kw_name = working_set.get_span_contents(*kw_span);
        match kw_name {
            b"def" => {
                let lite_command = LiteCommand {
                    comments: lite_command.comments.clone(),
                    parts: spans[1..].to_vec(),
                };
                let (pipeline, err) = parse_def(
                    working_set,
                    &lite_command,
                    Some(module_name),
                    expand_aliases_denylist,
                );
                error = error.or(err);

                let export_def_decl_id =
                    if let Some(id) = working_set.find_decl(b"export def", &Type::Any) {
                        id
                    } else {
                        return (
                            garbage_pipeline(spans),
                            vec![],
                            Some(ParseError::InternalError(
                                "missing 'export def' command".into(),
                                export_span,
                            )),
                        );
                    };

                // Trying to warp the 'def' call into the 'export def' in a very clumsy way
                if let Some(PipelineElement::Expression(
                    _,
                    Expression {
                        expr: Expr::Call(ref def_call),
                        ..
                    },
                )) = pipeline.elements.get(0)
                {
                    call = def_call.clone();

                    call.head = span(&spans[0..=1]);
                    call.decl_id = export_def_decl_id;
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "unexpected output from parsing a definition".into(),
                            span(&spans[1..]),
                        ))
                    });
                };

                let mut result = vec![];

                if let Some(decl_name_span) = spans.get(2) {
                    let decl_name = working_set.get_span_contents(*decl_name_span);
                    let decl_name = trim_quotes(decl_name);

                    if let Some(decl_id) = working_set.find_decl(decl_name, &Type::Any) {
                        result.push(Exportable::Decl {
                            name: decl_name.to_vec(),
                            id: decl_id,
                        });
                    } else {
                        error = error.or_else(|| {
                            Some(ParseError::InternalError(
                                "failed to find added declaration".into(),
                                span(&spans[1..]),
                            ))
                        });
                    }
                }

                result
            }
            b"def-env" => {
                let lite_command = LiteCommand {
                    comments: lite_command.comments.clone(),
                    parts: spans[1..].to_vec(),
                };
                let (pipeline, err) = parse_def(
                    working_set,
                    &lite_command,
                    Some(module_name),
                    expand_aliases_denylist,
                );
                error = error.or(err);

                let export_def_decl_id =
                    if let Some(id) = working_set.find_decl(b"export def-env", &Type::Any) {
                        id
                    } else {
                        return (
                            garbage_pipeline(spans),
                            vec![],
                            Some(ParseError::InternalError(
                                "missing 'export def-env' command".into(),
                                export_span,
                            )),
                        );
                    };

                // Trying to warp the 'def' call into the 'export def' in a very clumsy way
                if let Some(PipelineElement::Expression(
                    _,
                    Expression {
                        expr: Expr::Call(ref def_call),
                        ..
                    },
                )) = pipeline.elements.get(0)
                {
                    call = def_call.clone();

                    call.head = span(&spans[0..=1]);
                    call.decl_id = export_def_decl_id;
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "unexpected output from parsing a definition".into(),
                            span(&spans[1..]),
                        ))
                    });
                };

                let mut result = vec![];

                let decl_name = match spans.get(2) {
                    Some(span) => working_set.get_span_contents(*span),
                    None => &[],
                };
                let decl_name = trim_quotes(decl_name);

                if let Some(decl_id) = working_set.find_decl(decl_name, &Type::Any) {
                    result.push(Exportable::Decl {
                        name: decl_name.to_vec(),
                        id: decl_id,
                    });
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "failed to find added declaration".into(),
                            span(&spans[1..]),
                        ))
                    });
                }

                result
            }
            b"extern" => {
                let lite_command = LiteCommand {
                    comments: lite_command.comments.clone(),
                    parts: spans[1..].to_vec(),
                };
                let (pipeline, err) = parse_extern(
                    working_set,
                    &lite_command,
                    Some(module_name),
                    expand_aliases_denylist,
                );
                error = error.or(err);

                let export_def_decl_id =
                    if let Some(id) = working_set.find_decl(b"export extern", &Type::Any) {
                        id
                    } else {
                        return (
                            garbage_pipeline(spans),
                            vec![],
                            Some(ParseError::InternalError(
                                "missing 'export extern' command".into(),
                                export_span,
                            )),
                        );
                    };

                // Trying to warp the 'def' call into the 'export def' in a very clumsy way
                if let Some(PipelineElement::Expression(
                    _,
                    Expression {
                        expr: Expr::Call(ref def_call),
                        ..
                    },
                )) = pipeline.elements.get(0)
                {
                    call = def_call.clone();

                    call.head = span(&spans[0..=1]);
                    call.decl_id = export_def_decl_id;
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "unexpected output from parsing a definition".into(),
                            span(&spans[1..]),
                        ))
                    });
                };

                let mut result = vec![];

                let decl_name = match spans.get(2) {
                    Some(span) => working_set.get_span_contents(*span),
                    None => &[],
                };
                let decl_name = trim_quotes(decl_name);

                if let Some(decl_id) = working_set.find_decl(decl_name, &Type::Any) {
                    result.push(Exportable::Decl {
                        name: decl_name.to_vec(),
                        id: decl_id,
                    });
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "failed to find added declaration".into(),
                            span(&spans[1..]),
                        ))
                    });
                }

                result
            }
            b"old-alias" => {
                let lite_command = LiteCommand {
                    comments: lite_command.comments.clone(),
                    parts: spans[1..].to_vec(),
                };
                let (pipeline, err) = parse_old_alias(
                    working_set,
                    &lite_command,
                    Some(module_name),
                    expand_aliases_denylist,
                );
                error = error.or(err);

                let export_alias_decl_id =
                    if let Some(id) = working_set.find_decl(b"export old-alias", &Type::Any) {
                        id
                    } else {
                        return (
                            garbage_pipeline(spans),
                            vec![],
                            Some(ParseError::InternalError(
                                "missing 'export old-alias' command".into(),
                                export_span,
                            )),
                        );
                    };

                // Trying to warp the 'old-alias' call into the 'export old-alias' in a very clumsy way
                if let Some(PipelineElement::Expression(
                    _,
                    Expression {
                        expr: Expr::Call(ref alias_call),
                        ..
                    },
                )) = pipeline.elements.get(0)
                {
                    call = alias_call.clone();

                    call.head = span(&spans[0..=1]);
                    call.decl_id = export_alias_decl_id;
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "unexpected output from parsing a definition".into(),
                            span(&spans[1..]),
                        ))
                    });
                };

                let mut result = vec![];

                let alias_name = match spans.get(2) {
                    Some(span) => working_set.get_span_contents(*span),
                    None => &[],
                };
                let alias_name = trim_quotes(alias_name);

                if let Some(alias_id) = working_set.find_alias(alias_name) {
                    result.push(Exportable::Alias {
                        name: alias_name.to_vec(),
                        id: alias_id,
                    });
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "failed to find added alias".into(),
                            span(&spans[1..]),
                        ))
                    });
                }

                result
            }
            b"alias" => {
                let lite_command = LiteCommand {
                    comments: lite_command.comments.clone(),
                    parts: spans[1..].to_vec(),
                };
                let (pipeline, err) = parse_alias(
                    working_set,
                    &lite_command,
                    Some(module_name),
                    expand_aliases_denylist,
                );
                error = error.or(err);

                let export_alias_decl_id =
                    if let Some(id) = working_set.find_decl(b"export alias", &Type::Any) {
                        id
                    } else {
                        return (
                            garbage_pipeline(spans),
                            vec![],
                            Some(ParseError::InternalError(
                                "missing 'export alias' command".into(),
                                export_span,
                            )),
                        );
                    };

                // Trying to warp the 'alias' call into the 'export alias' in a very clumsy way
                if let Some(PipelineElement::Expression(
                    _,
                    Expression {
                        expr: Expr::Call(ref alias_call),
                        ..
                    },
                )) = pipeline.elements.get(0)
                {
                    call = alias_call.clone();

                    call.head = span(&spans[0..=1]);
                    call.decl_id = export_alias_decl_id;
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "unexpected output from parsing a definition".into(),
                            span(&spans[1..]),
                        ))
                    });
                };

                let mut result = vec![];

                let alias_name = match spans.get(2) {
                    Some(span) => working_set.get_span_contents(*span),
                    None => &[],
                };
                let alias_name = trim_quotes(alias_name);

                if let Some(alias_id) = working_set.find_decl(alias_name, &Type::Any) {
                    result.push(Exportable::Decl {
                        name: alias_name.to_vec(),
                        id: alias_id,
                    });
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "failed to find added alias".into(),
                            span(&spans[1..]),
                        ))
                    });
                }

                result
            }
            b"use" => {
                let lite_command = LiteCommand {
                    comments: lite_command.comments.clone(),
                    parts: spans[1..].to_vec(),
                };
                let (pipeline, exportables, err) =
                    parse_use(working_set, &lite_command.parts, expand_aliases_denylist);
                error = error.or(err);

                let export_use_decl_id =
                    if let Some(id) = working_set.find_decl(b"export use", &Type::Any) {
                        id
                    } else {
                        return (
                            garbage_pipeline(spans),
                            vec![],
                            Some(ParseError::InternalError(
                                "missing 'export use' command".into(),
                                export_span,
                            )),
                        );
                    };

                // Trying to warp the 'use' call into the 'export use' in a very clumsy way
                if let Some(PipelineElement::Expression(
                    _,
                    Expression {
                        expr: Expr::Call(ref use_call),
                        ..
                    },
                )) = pipeline.elements.get(0)
                {
                    call = use_call.clone();

                    call.head = span(&spans[0..=1]);
                    call.decl_id = export_use_decl_id;
                } else {
                    error = error.or_else(|| {
                        Some(ParseError::InternalError(
                            "unexpected output from parsing a definition".into(),
                            span(&spans[1..]),
                        ))
                    });
                };

                exportables
            }
            _ => {
                error = error.or_else(|| {
                    Some(ParseError::Expected(
                        // TODO: Fill in more keywords as they come
                        "def, def-env, alias, use, or env keyword".into(),
                        spans[1],
                    ))
                });

                vec![]
            }
        }
    } else {
        error = error.or_else(|| {
            Some(ParseError::MissingPositional(
                "def, def-env, alias, or env keyword".into(), // TODO: keep filling more keywords as they come
                Span::new(export_span.end, export_span.end),
                "'def', `def-env`, `alias`, or 'env' keyword.".to_string(),
            ))
        });

        vec![]
    };

    (
        Pipeline::from_vec(vec![Expression {
            expr: Expr::Call(call),
            span: span(spans),
            ty: Type::Any,
            custom_completion: None,
        }]),
        exportables,
        error,
    )
}

pub fn parse_export_env(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<BlockId>, Option<ParseError>) {
    if !spans.is_empty() && working_set.get_span_contents(spans[0]) != b"export-env" {
        return (
            garbage_pipeline(spans),
            None,
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for 'export-env' command".into(),
                span(spans),
            )),
        );
    }

    if spans.len() < 2 {
        return (
            garbage_pipeline(spans),
            None,
            Some(ParseError::MissingPositional(
                "block".into(),
                span(spans),
                "export-env <block>".into(),
            )),
        );
    }

    let call = match working_set.find_decl(b"export-env", &Type::Any) {
        Some(decl_id) => {
            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &[spans[1]],
                decl_id,
                expand_aliases_denylist,
            );
            let decl = working_set.get_decl(decl_id);

            let call_span = span(spans);

            err = check_call(call_span, &decl.signature(), &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    }]),
                    None,
                    err,
                );
            }

            call
        }
        None => {
            return (
                garbage_pipeline(spans),
                None,
                Some(ParseError::UnknownState(
                    "internal error: 'export-env' declaration not found".into(),
                    span(spans),
                )),
            )
        }
    };

    let block_id = if let Some(block) = call.positional_nth(0) {
        if let Some(block_id) = block.as_block() {
            block_id
        } else {
            return (
                garbage_pipeline(spans),
                None,
                Some(ParseError::UnknownState(
                    "internal error: 'export-env' block is not a block".into(),
                    block.span,
                )),
            );
        }
    } else {
        return (
            garbage_pipeline(spans),
            None,
            Some(ParseError::UnknownState(
                "internal error: 'export-env' block is missing".into(),
                span(spans),
            )),
        );
    };

    let pipeline = Pipeline::from_vec(vec![Expression {
        expr: Expr::Call(call),
        span: span(spans),
        ty: Type::Any,
        custom_completion: None,
    }]);

    (pipeline, Some(block_id), None)
}

fn collect_first_comments(tokens: &[Token]) -> Vec<Span> {
    let mut comments = vec![];

    let mut tokens_iter = tokens.iter().peekable();
    while let Some(token) = tokens_iter.next() {
        match token.contents {
            TokenContents::Comment => {
                comments.push(token.span);
            }
            TokenContents::Eol => {
                if let Some(Token {
                    contents: TokenContents::Eol,
                    ..
                }) = tokens_iter.peek()
                {
                    if !comments.is_empty() {
                        break;
                    }
                }
            }
            _ => {
                comments.clear();
                break;
            }
        }
    }

    comments
}

pub fn parse_module_block(
    working_set: &mut StateWorkingSet,
    span: Span,
    module_name: &[u8],
    expand_aliases_denylist: &[usize],
) -> (Block, Module, Vec<Span>, Option<ParseError>) {
    let mut error = None;

    working_set.enter_scope();

    let source = working_set.get_span_contents(span);

    let (output, err) = lex(source, span.start, &[], &[], false);
    error = error.or(err);

    let module_comments = collect_first_comments(&output);

    let (output, err) = lite_parse(&output);
    error = error.or(err);

    for pipeline in &output.block {
        if pipeline.commands.len() == 1 {
            if let LiteElement::Command(_, command) = &pipeline.commands[0] {
                parse_def_predecl(working_set, &command.parts, expand_aliases_denylist);
            }
        }
    }

    let mut module = Module::from_span(module_name.to_vec(), span);

    let block: Block = output
        .block
        .iter()
        .map(|pipeline| {
            if pipeline.commands.len() == 1 {
                match &pipeline.commands[0] {
                    LiteElement::Command(_, command) => {
                        let name = working_set.get_span_contents(command.parts[0]);

                        let (pipeline, err) = match name {
                            b"def" | b"def-env" => {
                                let (pipeline, err) = parse_def(
                                    working_set,
                                    command,
                                    None, // using commands named as the module locally is OK
                                    expand_aliases_denylist,
                                );

                                (pipeline, err)
                            }
                            b"extern" => {
                                let (pipeline, err) = parse_extern(
                                    working_set,
                                    command,
                                    None,
                                    expand_aliases_denylist,
                                );

                                (pipeline, err)
                            }
                            b"old-alias" => {
                                let (pipeline, err) = parse_old_alias(
                                    working_set,
                                    command,
                                    None, // using aliases named as the module locally is OK
                                    expand_aliases_denylist,
                                );

                                (pipeline, err)
                            }
                            b"alias" => {
                                let (pipeline, err) = parse_alias(
                                    working_set,
                                    command,
                                    None, // using aliases named as the module locally is OK
                                    expand_aliases_denylist,
                                );

                                (pipeline, err)
                            }
                            b"use" => {
                                let (pipeline, _, err) =
                                    parse_use(working_set, &command.parts, expand_aliases_denylist);

                                (pipeline, err)
                            }
                            b"export" => {
                                let (pipe, exportables, err) = parse_export_in_module(
                                    working_set,
                                    command,
                                    module_name,
                                    expand_aliases_denylist,
                                );

                                if err.is_none() {
                                    for exportable in exportables {
                                        match exportable {
                                            Exportable::Decl { name, id } => {
                                                if &name == b"main" {
                                                    module.main = Some(id);
                                                } else {
                                                    module.add_decl(name, id);
                                                }
                                            }
                                            Exportable::Alias { name, id } => {
                                                module.add_alias(name, id);
                                            }
                                        }
                                    }
                                }

                                (pipe, err)
                            }
                            b"export-env" => {
                                let (pipe, maybe_env_block, err) = parse_export_env(
                                    working_set,
                                    &command.parts,
                                    expand_aliases_denylist,
                                );

                                if let Some(block_id) = maybe_env_block {
                                    module.add_env_block(block_id);
                                }

                                (pipe, err)
                            }
                            _ => (
                                garbage_pipeline(&command.parts),
                                Some(ParseError::ExpectedKeyword(
                                    "def or export keyword".into(),
                                    command.parts[0],
                                )),
                            ),
                        };
                        if error.is_none() {
                            error = err;
                        }

                        pipeline
                    }
                    LiteElement::Redirection(_, _, command) => garbage_pipeline(&command.parts),
                    LiteElement::SeparateRedirection {
                        out: (_, command), ..
                    } => garbage_pipeline(&command.parts),
                }
            } else {
                error = Some(ParseError::Expected("not a pipeline".into(), span));
                garbage_pipeline(&[span])
            }
        })
        .into();

    working_set.exit_scope();

    (block, module, module_comments, error)
}

pub fn parse_module(
    working_set: &mut StateWorkingSet,
    lite_command: &LiteCommand,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    // TODO: Currently, module is closing over its parent scope (i.e., defs in the parent scope are
    // visible and usable in this module's scope). We want to disable that for files.

    let spans = &lite_command.parts;
    let mut module_comments = lite_command.comments.clone();

    let mut error = None;
    let bytes = working_set.get_span_contents(spans[0]);

    if bytes == b"module" && spans.len() >= 3 {
        let (module_name_expr, err) = parse_string(working_set, spans[1], expand_aliases_denylist);
        error = error.or(err);

        let module_name = module_name_expr
            .as_string()
            .expect("internal error: module name is not a string");

        let block_span = spans[2];
        let block_bytes = working_set.get_span_contents(block_span);
        let mut start = block_span.start;
        let mut end = block_span.end;

        if block_bytes.starts_with(b"{") {
            start += 1;
        } else {
            return (
                garbage_pipeline(spans),
                Some(ParseError::Expected("block".into(), block_span)),
            );
        }

        if block_bytes.ends_with(b"}") {
            end -= 1;
        } else {
            error = error.or_else(|| Some(ParseError::Unclosed("}".into(), Span::new(end, end))));
        }

        let block_span = Span::new(start, end);

        let (block, module, inner_comments, err) = parse_module_block(
            working_set,
            block_span,
            module_name.as_bytes(),
            expand_aliases_denylist,
        );
        error = error.or(err);

        let block_id = working_set.add_block(block);

        module_comments.extend(inner_comments);
        let _ = working_set.add_module(&module_name, module, module_comments);

        let block_expr = Expression {
            expr: Expr::Block(block_id),
            span: block_span,
            ty: Type::Block,
            custom_completion: None,
        };

        let module_decl_id = working_set
            .find_decl(b"module", &Type::Any)
            .expect("internal error: missing module command");

        let call = Box::new(Call {
            head: spans[0],
            decl_id: module_decl_id,
            arguments: vec![
                Argument::Positional(module_name_expr),
                Argument::Positional(block_expr),
            ],
            redirect_stdout: true,
            redirect_stderr: false,
            parser_info: vec![],
        });

        (
            Pipeline::from_vec(vec![Expression {
                expr: Expr::Call(call),
                span: span(spans),
                ty: Type::Any,
                custom_completion: None,
            }]),
            error,
        )
    } else {
        (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "Expected structure: module <name> {}".into(),
                span(spans),
            )),
        )
    }
}

pub fn parse_use(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Vec<Exportable>, Option<ParseError>) {
    let (name_span, split_id) =
        if spans.len() > 1 && working_set.get_span_contents(spans[0]) == b"export" {
            (spans[1], 2)
        } else {
            (spans[0], 1)
        };

    let use_call = working_set.get_span_contents(name_span).to_vec();
    if use_call != b"use" {
        return (
            garbage_pipeline(spans),
            vec![],
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for 'use' command".into(),
                span(spans),
            )),
        );
    }

    if working_set.get_span_contents(name_span) != b"use" {
        return (
            garbage_pipeline(spans),
            vec![],
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for 'use' command".into(),
                span(spans),
            )),
        );
    }

    let (call, call_span, args_spans) = match working_set.find_decl(b"use", &Type::Any) {
        Some(decl_id) => {
            let (command_spans, rest_spans) = spans.split_at(split_id);

            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                span(command_spans),
                rest_spans,
                decl_id,
                expand_aliases_denylist,
            );
            let decl = working_set.get_decl(decl_id);

            let call_span = span(spans);

            err = check_call(call_span, &decl.signature(), &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    }]),
                    vec![],
                    err,
                );
            }

            (call, call_span, rest_spans)
        }
        None => {
            return (
                garbage_pipeline(spans),
                vec![],
                Some(ParseError::UnknownState(
                    "internal error: 'use' declaration not found".into(),
                    span(spans),
                )),
            )
        }
    };

    let mut error = None;

    let (import_pattern_expr, err) =
        parse_import_pattern(working_set, args_spans, expand_aliases_denylist);
    error = error.or(err);

    let import_pattern = if let Expression {
        expr: Expr::ImportPattern(import_pattern),
        ..
    } = &import_pattern_expr
    {
        import_pattern.clone()
    } else {
        return (
            garbage_pipeline(spans),
            vec![],
            Some(ParseError::UnknownState(
                "internal error: Import pattern positional is not import pattern".into(),
                import_pattern_expr.span,
            )),
        );
    };

    let cwd = working_set.get_cwd();

    // TODO: Add checking for importing too long import patterns, e.g.:
    // > use spam foo non existent names here do not throw error
    let (import_pattern, module) = if let Some(module_id) = import_pattern.head.id {
        (import_pattern, working_set.get_module(module_id).clone())
    } else {
        // It could be a file
        // TODO: Do not close over when loading module from file?

        let (module_filename, err) =
            unescape_unquote_string(&import_pattern.head.name, import_pattern.head.span);

        if err.is_none() {
            if let Some(module_path) =
                find_in_dirs(&module_filename, working_set, &cwd, LIB_DIRS_ENV)
            {
                if let Some(i) = working_set
                    .parsed_module_files
                    .iter()
                    .rposition(|p| p == &module_path)
                {
                    let mut files: Vec<String> = working_set
                        .parsed_module_files
                        .split_off(i)
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();

                    files.push(module_path.to_string_lossy().to_string());

                    let msg = files.join("\nuses ");

                    return (
                        Pipeline::from_vec(vec![Expression {
                            expr: Expr::Call(call),
                            span: call_span,
                            ty: Type::Any,
                            custom_completion: None,
                        }]),
                        vec![],
                        Some(ParseError::CyclicalModuleImport(
                            msg,
                            import_pattern.head.span,
                        )),
                    );
                }

                let module_name = if let Some(stem) = module_path.file_stem() {
                    stem.to_string_lossy().to_string()
                } else {
                    return (
                        Pipeline::from_vec(vec![Expression {
                            expr: Expr::Call(call),
                            span: call_span,
                            ty: Type::Any,
                            custom_completion: None,
                        }]),
                        vec![],
                        Some(ParseError::ModuleNotFound(import_pattern.head.span)),
                    );
                };

                if let Ok(contents) = std::fs::read(&module_path) {
                    let span_start = working_set.next_span_start();
                    working_set.add_file(module_filename, &contents);
                    let span_end = working_set.next_span_start();

                    // Change the currently parsed directory
                    let prev_currently_parsed_cwd = if let Some(parent) = module_path.parent() {
                        let prev = working_set.currently_parsed_cwd.clone();

                        working_set.currently_parsed_cwd = Some(parent.into());

                        prev
                    } else {
                        working_set.currently_parsed_cwd.clone()
                    };

                    // Add the file to the stack of parsed module files
                    working_set.parsed_module_files.push(module_path);

                    // Parse the module
                    let (block, module, module_comments, err) = parse_module_block(
                        working_set,
                        Span::new(span_start, span_end),
                        module_name.as_bytes(),
                        expand_aliases_denylist,
                    );
                    error = error.or(err);

                    // Remove the file from the stack of parsed module files
                    working_set.parsed_module_files.pop();

                    // Restore the currently parsed directory back
                    working_set.currently_parsed_cwd = prev_currently_parsed_cwd;

                    let _ = working_set.add_block(block);
                    let module_id =
                        working_set.add_module(&module_name, module.clone(), module_comments);

                    (
                        ImportPattern {
                            head: ImportPatternHead {
                                name: module_name.into(),
                                id: Some(module_id),
                                span: import_pattern.head.span,
                            },
                            members: import_pattern.members,
                            hidden: HashSet::new(),
                        },
                        module,
                    )
                } else {
                    return (
                        Pipeline::from_vec(vec![Expression {
                            expr: Expr::Call(call),
                            span: call_span,
                            ty: Type::Any,
                            custom_completion: None,
                        }]),
                        vec![],
                        Some(ParseError::ModuleNotFound(import_pattern.head.span)),
                    );
                }
            } else {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: span(spans),
                        ty: Type::Any,
                        custom_completion: None,
                    }]),
                    vec![],
                    Some(ParseError::ModuleNotFound(import_pattern.head.span)),
                );
            }
        } else {
            return (
                garbage_pipeline(spans),
                vec![],
                Some(ParseError::NonUtf8(import_pattern.head.span)),
            );
        }
    };

    let (decls_to_use, aliases_to_use) = if import_pattern.members.is_empty() {
        (
            module.decls_with_head(&import_pattern.head.name),
            module.aliases_with_head(&import_pattern.head.name),
        )
    } else {
        match &import_pattern.members[0] {
            ImportPatternMember::Glob { .. } => (module.decls(), module.aliases()),
            ImportPatternMember::Name { name, span } => {
                let mut decl_output = vec![];
                let mut alias_output = vec![];

                if name == b"main" {
                    if let Some(id) = &module.main {
                        decl_output.push((import_pattern.head.name.clone(), *id));
                    } else {
                        error = error.or(Some(ParseError::ExportNotFound(*span)));
                    }
                } else if let Some(id) = module.get_decl_id(name) {
                    decl_output.push((name.clone(), id));
                } else if let Some(id) = module.get_alias_id(name) {
                    alias_output.push((name.clone(), id));
                } else {
                    error = error.or(Some(ParseError::ExportNotFound(*span)));
                }

                (decl_output, alias_output)
            }
            ImportPatternMember::List { names } => {
                let mut decl_output = vec![];
                let mut alias_output = vec![];

                for (name, span) in names {
                    if name == b"main" {
                        if let Some(id) = &module.main {
                            decl_output.push((import_pattern.head.name.clone(), *id));
                        } else {
                            error = error.or(Some(ParseError::ExportNotFound(*span)));
                        }
                    } else if let Some(id) = module.get_decl_id(name) {
                        decl_output.push((name.clone(), id));
                    } else if let Some(id) = module.get_alias_id(name) {
                        alias_output.push((name.clone(), id));
                    } else {
                        error = error.or(Some(ParseError::ExportNotFound(*span)));
                        break;
                    }
                }

                (decl_output, alias_output)
            }
        }
    };

    let exportables = decls_to_use
        .iter()
        .map(|(name, decl_id)| Exportable::Decl {
            name: name.clone(),
            id: *decl_id,
        })
        .chain(
            aliases_to_use
                .iter()
                .map(|(name, alias_id)| Exportable::Alias {
                    name: name.clone(),
                    id: *alias_id,
                }),
        )
        .collect();

    // Extend the current scope with the module's exportables
    working_set.use_decls(decls_to_use);
    working_set.use_aliases(aliases_to_use);

    // Create a new Use command call to pass the new import pattern
    let import_pattern_expr = Expression {
        expr: Expr::ImportPattern(import_pattern),
        span: span(args_spans),
        ty: Type::Any,
        custom_completion: None,
    };

    let mut call = call;
    call.add_parser_info(import_pattern_expr);

    (
        Pipeline::from_vec(vec![Expression {
            expr: Expr::Call(call),
            span: span(spans),
            ty: Type::Any,
            custom_completion: None,
        }]),
        exportables,
        error,
    )
}

pub fn parse_hide(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    if working_set.get_span_contents(spans[0]) != b"hide" {
        return (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for 'hide' command".into(),
                span(spans),
            )),
        );
    }

    let (call, args_spans) = match working_set.find_decl(b"hide", &Type::Any) {
        Some(decl_id) => {
            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );
            let decl = working_set.get_decl(decl_id);

            let call_span = span(spans);

            err = check_call(call_span, &decl.signature(), &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    }]),
                    err,
                );
            }

            (call, &spans[1..])
        }
        None => {
            return (
                garbage_pipeline(spans),
                Some(ParseError::UnknownState(
                    "internal error: 'hide' declaration not found".into(),
                    span(spans),
                )),
            )
        }
    };

    let mut error = None;

    let (import_pattern_expr, err) =
        parse_import_pattern(working_set, args_spans, expand_aliases_denylist);
    error = error.or(err);

    let import_pattern = if let Expression {
        expr: Expr::ImportPattern(import_pattern),
        ..
    } = &import_pattern_expr
    {
        import_pattern.clone()
    } else {
        return (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "internal error: Import pattern positional is not import pattern".into(),
                import_pattern_expr.span,
            )),
        );
    };

    let bytes = working_set.get_span_contents(spans[0]);

    if bytes == b"hide" && spans.len() >= 2 {
        for span in spans[1..].iter() {
            let (_, err) = parse_string(working_set, *span, expand_aliases_denylist);
            error = error.or(err);
        }

        // module used only internally, not saved anywhere
        let (is_module, module) = if let Some(module_id) =
            working_set.find_module(&import_pattern.head.name)
        {
            (true, working_set.get_module(module_id).clone())
        } else if import_pattern.members.is_empty() {
            // The pattern head can be:
            if let Some(id) = working_set.find_alias(&import_pattern.head.name) {
                // an alias,
                let mut module = Module::new(b"tmp".to_vec());
                module.add_alias(import_pattern.head.name.clone(), id);

                (false, module)
            } else if let Some(id) = working_set.find_decl(&import_pattern.head.name, &Type::Any) {
                // a custom command,
                let mut module = Module::new(b"tmp".to_vec());
                module.add_decl(import_pattern.head.name.clone(), id);

                (false, module)
            } else {
                // , or it could be an env var (handled by the engine)
                (false, Module::new(b"tmp".to_vec()))
            }
        } else {
            return (
                garbage_pipeline(spans),
                Some(ParseError::ModuleNotFound(spans[1])),
            );
        };

        // This kind of inverts the import pattern matching found in parse_use()
        let (aliases_to_hide, decls_to_hide) = if import_pattern.members.is_empty() {
            if is_module {
                (
                    module.alias_names_with_head(&import_pattern.head.name),
                    module.decl_names_with_head(&import_pattern.head.name),
                )
            } else {
                (module.alias_names(), module.decl_names())
            }
        } else {
            match &import_pattern.members[0] {
                ImportPatternMember::Glob { .. } => (module.alias_names(), module.decl_names()),
                ImportPatternMember::Name { name, span } => {
                    let mut aliases = vec![];
                    let mut decls = vec![];

                    if name == b"main" {
                        if module.main.is_some() {
                            decls.push(import_pattern.head.name.clone());
                        } else {
                            error = error.or(Some(ParseError::ExportNotFound(*span)));
                        }
                    } else if let Some(item) =
                        module.alias_name_with_head(name, &import_pattern.head.name)
                    {
                        aliases.push(item);
                    } else if let Some(item) =
                        module.decl_name_with_head(name, &import_pattern.head.name)
                    {
                        decls.push(item);
                    } else {
                        error = error.or(Some(ParseError::ExportNotFound(*span)));
                    }

                    (aliases, decls)
                }
                ImportPatternMember::List { names } => {
                    let mut aliases = vec![];
                    let mut decls = vec![];

                    for (name, span) in names {
                        if name == b"main" {
                            if module.main.is_some() {
                                decls.push(import_pattern.head.name.clone());
                            } else {
                                error = error.or(Some(ParseError::ExportNotFound(*span)));
                                break;
                            }
                        } else if let Some(item) =
                            module.alias_name_with_head(name, &import_pattern.head.name)
                        {
                            aliases.push(item);
                        } else if let Some(item) =
                            module.decl_name_with_head(name, &import_pattern.head.name)
                        {
                            decls.push(item);
                        } else {
                            error = error.or(Some(ParseError::ExportNotFound(*span)));
                            break;
                        }
                    }

                    (aliases, decls)
                }
            }
        };

        let import_pattern = {
            let aliases: HashSet<Vec<u8>> = aliases_to_hide.iter().cloned().collect();
            let decls: HashSet<Vec<u8>> = decls_to_hide.iter().cloned().collect();

            import_pattern.with_hidden(decls.union(&aliases).cloned().collect())
        };

        // TODO: `use spam; use spam foo; hide foo` will hide both `foo` and `spam foo` since
        // they point to the same DeclId. Do we want to keep it that way?
        working_set.hide_decls(&decls_to_hide);
        working_set.hide_aliases(&aliases_to_hide);

        // Create a new Use command call to pass the new import pattern
        let import_pattern_expr = Expression {
            expr: Expr::ImportPattern(import_pattern),
            span: span(args_spans),
            ty: Type::Any,
            custom_completion: None,
        };

        let mut call = call;
        call.add_parser_info(import_pattern_expr);

        (
            Pipeline::from_vec(vec![Expression {
                expr: Expr::Call(call),
                span: span(spans),
                ty: Type::Any,
                custom_completion: None,
            }]),
            error,
        )
    } else {
        (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "Expected structure: hide <name>".into(),
                span(spans),
            )),
        )
    }
}

pub fn parse_overlay_new(
    working_set: &mut StateWorkingSet,
    call: Box<Call>,
) -> (Pipeline, Option<ParseError>) {
    let call_span = call.span();

    let (overlay_name, _) = if let Some(expr) = call.positional_nth(0) {
        match eval_constant(working_set, expr) {
            Ok(val) => match value_as_string(val, expr.span) {
                Ok(s) => (s, expr.span),
                Err(err) => {
                    return (garbage_pipeline(&[call_span]), Some(err));
                }
            },
            Err(err) => {
                return (garbage_pipeline(&[call_span]), Some(err));
            }
        }
    } else {
        return (
            garbage_pipeline(&[call_span]),
            Some(ParseError::UnknownState(
                "internal error: Missing required positional after call parsing".into(),
                call_span,
            )),
        );
    };

    let pipeline = Pipeline::from_vec(vec![Expression {
        expr: Expr::Call(call),
        span: call_span,
        ty: Type::Any,
        custom_completion: None,
    }]);

    let module_id = working_set.add_module(
        &overlay_name,
        Module::new(overlay_name.as_bytes().to_vec()),
        vec![],
    );

    working_set.add_overlay(
        overlay_name.as_bytes().to_vec(),
        module_id,
        vec![],
        vec![],
        false,
    );

    (pipeline, None)
}

pub fn parse_overlay_use(
    working_set: &mut StateWorkingSet,
    call: Box<Call>,
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let call_span = call.span();

    let (overlay_name, overlay_name_span) = if let Some(expr) = call.positional_nth(0) {
        match eval_constant(working_set, expr) {
            Ok(val) => match value_as_string(val, expr.span) {
                Ok(s) => (s, expr.span),
                Err(err) => {
                    return (garbage_pipeline(&[call_span]), Some(err));
                }
            },
            Err(err) => {
                return (garbage_pipeline(&[call_span]), Some(err));
            }
        }
    } else {
        return (
            garbage_pipeline(&[call_span]),
            Some(ParseError::UnknownState(
                "internal error: Missing required positional after call parsing".into(),
                call_span,
            )),
        );
    };

    let new_name = if let Some(kw_expression) = call.positional_nth(1) {
        if let Some(new_name_expression) = kw_expression.as_keyword() {
            match eval_constant(working_set, new_name_expression) {
                Ok(val) => match value_as_string(val, new_name_expression.span) {
                    Ok(s) => Some(Spanned {
                        item: s,
                        span: new_name_expression.span,
                    }),
                    Err(err) => return (garbage_pipeline(&[call_span]), Some(err)),
                },
                Err(err) => return (garbage_pipeline(&[call_span]), Some(err)),
            }
        } else {
            return (
                garbage_pipeline(&[call_span]),
                Some(ParseError::ExpectedKeyword(
                    "as keyword".to_string(),
                    kw_expression.span,
                )),
            );
        }
    } else {
        None
    };

    let has_prefix = call.has_flag("prefix");
    let do_reload = call.has_flag("reload");

    let pipeline = Pipeline::from_vec(vec![Expression {
        expr: Expr::Call(call.clone()),
        span: call_span,
        ty: Type::Any,
        custom_completion: None,
    }]);

    let cwd = working_set.get_cwd();

    let mut error = None;

    let (final_overlay_name, origin_module, origin_module_id, is_module_updated) = if let Some(
        overlay_frame,
    ) =
        working_set.find_overlay(overlay_name.as_bytes())
    {
        // Activate existing overlay

        // First, check for errors
        if has_prefix && !overlay_frame.prefixed {
            return (
                pipeline,
                Some(ParseError::OverlayPrefixMismatch(
                    overlay_name,
                    "without".to_string(),
                    overlay_name_span,
                )),
            );
        }

        if !has_prefix && overlay_frame.prefixed {
            return (
                pipeline,
                Some(ParseError::OverlayPrefixMismatch(
                    overlay_name,
                    "with".to_string(),
                    overlay_name_span,
                )),
            );
        }

        if let Some(new_name) = new_name {
            if new_name.item != overlay_name {
                return (
                    pipeline,
                    Some(ParseError::CantAddOverlayHelp(
                            format!("Cannot add overlay as '{}' because it already exists under the name '{}'", new_name.item, overlay_name),
                            new_name.span,
                    )),
                );
            }
        }

        let module_id = overlay_frame.origin;

        if let Some(new_module_id) = working_set.find_module(overlay_name.as_bytes()) {
            if !do_reload && (module_id == new_module_id) {
                (
                    overlay_name,
                    Module::new(working_set.get_module(module_id).name.clone()),
                    module_id,
                    false,
                )
            } else {
                // The origin module of an overlay changed => update it
                (
                    overlay_name,
                    working_set.get_module(new_module_id).clone(),
                    new_module_id,
                    true,
                )
            }
        } else {
            let module_name = overlay_name.as_bytes().to_vec();
            (overlay_name, Module::new(module_name), module_id, true)
        }
    } else {
        // Create a new overlay from a module
        if let Some(module_id) =
            // the name is a module
            working_set.find_module(overlay_name.as_bytes())
        {
            (
                new_name.map(|spanned| spanned.item).unwrap_or(overlay_name),
                working_set.get_module(module_id).clone(),
                module_id,
                true,
            )
        } else {
            // try if the name is a file
            if let Ok(module_filename) =
                String::from_utf8(trim_quotes(overlay_name.as_bytes()).to_vec())
            {
                if let Some(module_path) =
                    find_in_dirs(&module_filename, working_set, &cwd, LIB_DIRS_ENV)
                {
                    let overlay_name = if let Some(stem) = module_path.file_stem() {
                        stem.to_string_lossy().to_string()
                    } else {
                        return (
                            pipeline,
                            Some(ParseError::ModuleOrOverlayNotFound(overlay_name_span)),
                        );
                    };

                    if let Ok(contents) = std::fs::read(&module_path) {
                        let span_start = working_set.next_span_start();
                        working_set.add_file(module_filename, &contents);
                        let span_end = working_set.next_span_start();

                        // Change currently parsed directory
                        let prev_currently_parsed_cwd = if let Some(parent) = module_path.parent() {
                            let prev = working_set.currently_parsed_cwd.clone();

                            working_set.currently_parsed_cwd = Some(parent.into());

                            prev
                        } else {
                            working_set.currently_parsed_cwd.clone()
                        };

                        let (block, module, module_comments, err) = parse_module_block(
                            working_set,
                            Span::new(span_start, span_end),
                            overlay_name.as_bytes(),
                            expand_aliases_denylist,
                        );
                        error = error.or(err);

                        // Restore the currently parsed directory back
                        working_set.currently_parsed_cwd = prev_currently_parsed_cwd;

                        let _ = working_set.add_block(block);
                        let module_id =
                            working_set.add_module(&overlay_name, module.clone(), module_comments);

                        (
                            new_name.map(|spanned| spanned.item).unwrap_or(overlay_name),
                            module,
                            module_id,
                            true,
                        )
                    } else {
                        return (
                            pipeline,
                            Some(ParseError::ModuleOrOverlayNotFound(overlay_name_span)),
                        );
                    }
                } else {
                    return (
                        pipeline,
                        Some(ParseError::ModuleOrOverlayNotFound(overlay_name_span)),
                    );
                }
            } else {
                return (
                    garbage_pipeline(&[call_span]),
                    Some(ParseError::NonUtf8(overlay_name_span)),
                );
            }
        }
    };

    let (decls_to_lay, aliases_to_lay) = if is_module_updated {
        if has_prefix {
            (
                origin_module.decls_with_head(final_overlay_name.as_bytes()),
                origin_module.aliases_with_head(final_overlay_name.as_bytes()),
            )
        } else {
            (origin_module.decls(), origin_module.aliases())
        }
    } else {
        (vec![], vec![])
    };

    working_set.add_overlay(
        final_overlay_name.as_bytes().to_vec(),
        origin_module_id,
        decls_to_lay,
        aliases_to_lay,
        has_prefix,
    );

    // Change the call argument to include the Overlay expression with the module ID
    let mut call = call;
    call.add_parser_info(Expression {
        expr: Expr::Overlay(if is_module_updated {
            Some(origin_module_id)
        } else {
            None
        }),
        span: overlay_name_span,
        ty: Type::Any,
        custom_completion: None,
    });

    let pipeline = Pipeline::from_vec(vec![Expression {
        expr: Expr::Call(call),
        span: call_span,
        ty: Type::Any,
        custom_completion: None,
    }]);

    (pipeline, error)
}

pub fn parse_overlay_hide(
    working_set: &mut StateWorkingSet,
    call: Box<Call>,
) -> (Pipeline, Option<ParseError>) {
    let call_span = call.span();

    let (overlay_name, overlay_name_span) = if let Some(expr) = call.positional_nth(0) {
        match eval_constant(working_set, expr) {
            Ok(val) => match value_as_string(val, expr.span) {
                Ok(s) => (s, expr.span),
                Err(err) => {
                    return (garbage_pipeline(&[call_span]), Some(err));
                }
            },
            Err(err) => {
                return (garbage_pipeline(&[call_span]), Some(err));
            }
        }
    } else {
        (
            String::from_utf8_lossy(working_set.last_overlay_name()).to_string(),
            call_span,
        )
    };

    let keep_custom = call.has_flag("keep-custom");

    let pipeline = Pipeline::from_vec(vec![Expression {
        expr: Expr::Call(call),
        span: call_span,
        ty: Type::Any,
        custom_completion: None,
    }]);

    if overlay_name == DEFAULT_OVERLAY_NAME {
        return (
            pipeline,
            Some(ParseError::CantHideDefaultOverlay(
                overlay_name,
                overlay_name_span,
            )),
        );
    }

    if !working_set
        .unique_overlay_names()
        .contains(&overlay_name.as_bytes().to_vec())
    {
        return (
            pipeline,
            Some(ParseError::ActiveOverlayNotFound(overlay_name_span)),
        );
    }

    if working_set.num_overlays() < 2 {
        return (
            pipeline,
            Some(ParseError::CantRemoveLastOverlay(overlay_name_span)),
        );
    }

    working_set.remove_overlay(overlay_name.as_bytes(), keep_custom);

    (pipeline, None)
}

pub fn parse_let_or_const(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let name = working_set.get_span_contents(spans[0]);

    if name == b"let" || name == b"const" {
        let is_const = &name == b"const";

        if let Some((span, err)) = check_name(working_set, spans) {
            return (Pipeline::from_vec(vec![garbage(*span)]), Some(err));
        }

        if let Some(decl_id) =
            working_set.find_decl(if is_const { b"const" } else { b"let" }, &Type::Any)
        {
            let cmd = working_set.get_decl(decl_id);
            let call_signature = cmd.signature().call_signature();

            if spans.len() >= 4 {
                // This is a bit of by-hand parsing to get around the issue where we want to parse in the reverse order
                // so that the var-id created by the variable isn't visible in the expression that init it
                for span in spans.iter().enumerate() {
                    let item = working_set.get_span_contents(*span.1);
                    if item == b"=" && spans.len() > (span.0 + 1) {
                        let mut error = None;

                        let mut idx = span.0;
                        let (rvalue, err) = parse_multispan_value(
                            working_set,
                            spans,
                            &mut idx,
                            &SyntaxShape::Keyword(b"=".to_vec(), Box::new(SyntaxShape::Expression)),
                            expand_aliases_denylist,
                        );
                        error = error.or(err);

                        if idx < (spans.len() - 1) {
                            error = error.or(Some(ParseError::ExtraPositional(
                                call_signature,
                                spans[idx + 1],
                            )));
                        }

                        let mut idx = 0;
                        let (lvalue, err) = parse_var_with_opt_type(
                            working_set,
                            &spans[1..(span.0)],
                            &mut idx,
                            false,
                        );
                        error = error.or(err);

                        let var_name =
                            String::from_utf8_lossy(working_set.get_span_contents(lvalue.span))
                                .to_string();

                        if ["in", "nu", "env", "nothing"].contains(&var_name.as_str()) {
                            error = if is_const {
                                error.or(Some(ParseError::ConstBuiltinVar(var_name, lvalue.span)))
                            } else {
                                error.or(Some(ParseError::LetBuiltinVar(var_name, lvalue.span)))
                            };
                        }

                        let var_id = lvalue.as_var();
                        let rhs_type = rvalue.ty.clone();

                        if let Some(var_id) = var_id {
                            working_set.set_variable_type(var_id, rhs_type);

                            if is_const {
                                match eval_constant(working_set, &rvalue) {
                                    Ok(val) => {
                                        working_set.add_constant(var_id, val);
                                    }
                                    Err(err) => error = error.or(Some(err)),
                                }
                            }
                        }

                        let call = Box::new(Call {
                            decl_id,
                            head: spans[0],
                            arguments: vec![
                                Argument::Positional(lvalue),
                                Argument::Positional(rvalue),
                            ],
                            redirect_stdout: true,
                            redirect_stderr: false,
                            parser_info: vec![],
                        });

                        return (
                            Pipeline::from_vec(vec![Expression {
                                expr: Expr::Call(call),
                                span: nu_protocol::span(spans),
                                ty: Type::Any,
                                custom_completion: None,
                            }]),
                            error,
                        );
                    }
                }
            }
            let ParsedInternalCall {
                call,
                error: err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );

            return (
                Pipeline::from_vec(vec![Expression {
                    expr: Expr::Call(call),
                    span: nu_protocol::span(spans),
                    ty: output,
                    custom_completion: None,
                }]),
                err,
            );
        }
    }
    (
        garbage_pipeline(spans),
        Some(ParseError::UnknownState(
            "internal error: let or const statement unparsable".into(),
            span(spans),
        )),
    )
}

pub fn parse_mut(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let name = working_set.get_span_contents(spans[0]);

    if name == b"mut" {
        if let Some((span, err)) = check_name(working_set, spans) {
            return (Pipeline::from_vec(vec![garbage(*span)]), Some(err));
        }

        if let Some(decl_id) = working_set.find_decl(b"mut", &Type::Any) {
            let cmd = working_set.get_decl(decl_id);
            let call_signature = cmd.signature().call_signature();

            if spans.len() >= 4 {
                // This is a bit of by-hand parsing to get around the issue where we want to parse in the reverse order
                // so that the var-id created by the variable isn't visible in the expression that init it
                for span in spans.iter().enumerate() {
                    let item = working_set.get_span_contents(*span.1);
                    if item == b"=" && spans.len() > (span.0 + 1) {
                        let mut error = None;

                        let mut idx = span.0;
                        let (rvalue, err) = parse_multispan_value(
                            working_set,
                            spans,
                            &mut idx,
                            &SyntaxShape::Keyword(b"=".to_vec(), Box::new(SyntaxShape::Expression)),
                            expand_aliases_denylist,
                        );
                        error = error.or(err);

                        if idx < (spans.len() - 1) {
                            error = error.or(Some(ParseError::ExtraPositional(
                                call_signature,
                                spans[idx + 1],
                            )));
                        }

                        let mut idx = 0;
                        let (lvalue, err) = parse_var_with_opt_type(
                            working_set,
                            &spans[1..(span.0)],
                            &mut idx,
                            true,
                        );
                        error = error.or(err);

                        let var_name =
                            String::from_utf8_lossy(working_set.get_span_contents(lvalue.span))
                                .to_string();

                        if ["in", "nu", "env", "nothing"].contains(&var_name.as_str()) {
                            error =
                                error.or(Some(ParseError::MutBuiltinVar(var_name, lvalue.span)));
                        }

                        let var_id = lvalue.as_var();
                        let rhs_type = rvalue.ty.clone();

                        if let Some(var_id) = var_id {
                            working_set.set_variable_type(var_id, rhs_type);
                        }

                        let call = Box::new(Call {
                            decl_id,
                            head: spans[0],
                            arguments: vec![
                                Argument::Positional(lvalue),
                                Argument::Positional(rvalue),
                            ],
                            redirect_stdout: true,
                            redirect_stderr: false,
                            parser_info: vec![],
                        });

                        return (
                            Pipeline::from_vec(vec![Expression {
                                expr: Expr::Call(call),
                                span: nu_protocol::span(spans),
                                ty: Type::Any,
                                custom_completion: None,
                            }]),
                            error,
                        );
                    }
                }
            }
            let ParsedInternalCall {
                call,
                error: err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );

            return (
                Pipeline::from_vec(vec![Expression {
                    expr: Expr::Call(call),
                    span: nu_protocol::span(spans),
                    ty: output,
                    custom_completion: None,
                }]),
                err,
            );
        }
    }
    (
        garbage_pipeline(spans),
        Some(ParseError::UnknownState(
            "internal error: mut statement unparsable".into(),
            span(spans),
        )),
    )
}

pub fn parse_source(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let mut error = None;
    let name = working_set.get_span_contents(spans[0]);

    if name == b"source" || name == b"source-env" {
        let scoped = name == b"source-env";

        if let Some(decl_id) = working_set.find_decl(name, &Type::Any) {
            let cwd = working_set.get_cwd();

            // Is this the right call to be using here?
            // Some of the others (`parse_let`) use it, some of them (`parse_hide`) don't.
            let ParsedInternalCall {
                call,
                error: err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );
            error = error.or(err);

            if error.is_some() || call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: span(spans),
                        ty: output,
                        custom_completion: None,
                    }]),
                    error,
                );
            }

            // Command and one file name
            if spans.len() >= 2 {
                let (expr, err) = parse_value(
                    working_set,
                    spans[1],
                    &SyntaxShape::Any,
                    expand_aliases_denylist,
                );

                error = error.or(err);

                let val = match eval_constant(working_set, &expr) {
                    Ok(val) => val,
                    Err(err) => {
                        return (
                            Pipeline::from_vec(vec![Expression {
                                expr: Expr::Call(call),
                                span: span(&spans[1..]),
                                ty: Type::Any,
                                custom_completion: None,
                            }]),
                            Some(err),
                        );
                    }
                };

                let filename = match value_as_string(val, spans[1]) {
                    Ok(s) => s,
                    Err(err) => {
                        return (
                            Pipeline::from_vec(vec![Expression {
                                expr: Expr::Call(call),
                                span: span(&spans[1..]),
                                ty: Type::Any,
                                custom_completion: None,
                            }]),
                            Some(err),
                        );
                    }
                };

                if let Some(path) = find_in_dirs(&filename, working_set, &cwd, LIB_DIRS_ENV) {
                    if let Ok(contents) = std::fs::read(&path) {
                        // Change currently parsed directory
                        let prev_currently_parsed_cwd = if let Some(parent) = path.parent() {
                            let prev = working_set.currently_parsed_cwd.clone();

                            working_set.currently_parsed_cwd = Some(parent.into());

                            prev
                        } else {
                            working_set.currently_parsed_cwd.clone()
                        };

                        // This will load the defs from the file into the
                        // working set, if it was a successful parse.
                        let (block, err) = parse(
                            working_set,
                            path.file_name().and_then(|x| x.to_str()),
                            &contents,
                            scoped,
                            expand_aliases_denylist,
                        );

                        // Restore the currently parsed directory back
                        working_set.currently_parsed_cwd = prev_currently_parsed_cwd;

                        if err.is_some() {
                            // Unsuccessful parse of file
                            return (
                                Pipeline::from_vec(vec![Expression {
                                    expr: Expr::Call(call),
                                    span: span(&spans[1..]),
                                    ty: Type::Any,
                                    custom_completion: None,
                                }]),
                                // Return the file parse error
                                err,
                            );
                        } else {
                            // Save the block into the working set
                            let block_id = working_set.add_block(block);

                            let mut call_with_block = call;

                            // FIXME: Adding this expression to the positional creates a syntax highlighting error
                            // after writing `source example.nu`
                            call_with_block.add_parser_info(Expression {
                                expr: Expr::Int(block_id as i64),
                                span: spans[1],
                                ty: Type::Any,
                                custom_completion: None,
                            });

                            return (
                                Pipeline::from_vec(vec![Expression {
                                    expr: Expr::Call(call_with_block),
                                    span: span(spans),
                                    ty: Type::Any,
                                    custom_completion: None,
                                }]),
                                None,
                            );
                        }
                    }
                } else {
                    error = error.or(Some(ParseError::SourcedFileNotFound(filename, spans[1])));
                }
            }
            return (
                Pipeline::from_vec(vec![Expression {
                    expr: Expr::Call(call),
                    span: span(spans),
                    ty: Type::Any,
                    custom_completion: None,
                }]),
                error,
            );
        }
    }
    (
        garbage_pipeline(spans),
        Some(ParseError::UnknownState(
            "internal error: source statement unparsable".into(),
            span(spans),
        )),
    )
}

pub fn parse_where_expr(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Expression, Option<ParseError>) {
    trace!("parsing: where");

    if !spans.is_empty() && working_set.get_span_contents(spans[0]) != b"where" {
        return (
            garbage(span(spans)),
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for 'where' command".into(),
                span(spans),
            )),
        );
    }

    if spans.len() < 2 {
        return (
            garbage(span(spans)),
            Some(ParseError::MissingPositional(
                "row condition".into(),
                span(spans),
                "where <row_condition>".into(),
            )),
        );
    }

    let call = match working_set.find_decl(b"where", &Type::Any) {
        Some(decl_id) => {
            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );
            let decl = working_set.get_decl(decl_id);

            let call_span = span(spans);

            err = check_call(call_span, &decl.signature(), &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    },
                    err,
                );
            }

            call
        }
        None => {
            return (
                garbage(span(spans)),
                Some(ParseError::UnknownState(
                    "internal error: 'where' declaration not found".into(),
                    span(spans),
                )),
            )
        }
    };

    (
        Expression {
            expr: Expr::Call(call),
            span: span(spans),
            ty: Type::Any,
            custom_completion: None,
        },
        None,
    )
}

pub fn parse_where(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    let (expression, err) = parse_where_expr(working_set, spans, expand_aliases_denylist);
    (Pipeline::from_vec(vec![expression]), err)
}

#[cfg(feature = "plugin")]
pub fn parse_register(
    working_set: &mut StateWorkingSet,
    spans: &[Span],
    expand_aliases_denylist: &[usize],
) -> (Pipeline, Option<ParseError>) {
    use nu_plugin::{get_signature, PluginDeclaration};
    use nu_protocol::{engine::Stack, PluginSignature};

    let cwd = working_set.get_cwd();

    // Checking that the function is used with the correct name
    // Maybe this is not necessary but it is a sanity check
    if working_set.get_span_contents(spans[0]) != b"register" {
        return (
            garbage_pipeline(spans),
            Some(ParseError::UnknownState(
                "internal error: Wrong call name for parse plugin function".into(),
                span(spans),
            )),
        );
    }

    // Parsing the spans and checking that they match the register signature
    // Using a parsed call makes more sense than checking for how many spans are in the call
    // Also, by creating a call, it can be checked if it matches the declaration signature
    let (call, call_span) = match working_set.find_decl(b"register", &Type::Any) {
        None => {
            return (
                garbage_pipeline(spans),
                Some(ParseError::UnknownState(
                    "internal error: Register declaration not found".into(),
                    span(spans),
                )),
            )
        }
        Some(decl_id) => {
            let ParsedInternalCall {
                call,
                error: mut err,
                output,
            } = parse_internal_call(
                working_set,
                spans[0],
                &spans[1..],
                decl_id,
                expand_aliases_denylist,
            );
            let decl = working_set.get_decl(decl_id);

            let call_span = span(spans);

            err = check_call(call_span, &decl.signature(), &call).or(err);
            if err.is_some() || call.has_flag("help") {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: output,
                        custom_completion: None,
                    }]),
                    err,
                );
            }

            (call, call_span)
        }
    };

    // Extracting the required arguments from the call and keeping them together in a tuple
    // The ? operator is not used because the error has to be kept to be printed in the shell
    // For that reason the values are kept in a result that will be passed at the end of this call
    let arguments = call
        .positional_nth(0)
        .map(|expr| {
            let name_expr = working_set.get_span_contents(expr.span);

            let (name, err) = unescape_unquote_string(name_expr, expr.span);

            if let Some(err) = err {
                Err(err)
            } else {
                let path = if let Some(p) = find_in_dirs(&name, working_set, &cwd, PLUGIN_DIRS_ENV)
                {
                    p
                } else {
                    return Err(ParseError::RegisteredFileNotFound(name, expr.span));
                };

                if path.exists() & path.is_file() {
                    Ok(path)
                } else {
                    Err(ParseError::RegisteredFileNotFound(
                        format!("{path:?}"),
                        expr.span,
                    ))
                }
            }
        })
        .expect("required positional has being checked");

    // Signature is an optional value from the call and will be used to decide if
    // the plugin is called to get the signatures or to use the given signature
    let signature = call.positional_nth(1).map(|expr| {
        let signature = working_set.get_span_contents(expr.span);
        serde_json::from_slice::<PluginSignature>(signature).map_err(|e| {
            ParseError::LabeledError(
                "Signature deserialization error".into(),
                format!("unable to deserialize signature: {e}"),
                spans[0],
            )
        })
    });

    // Shell is another optional value used as base to call shell to plugins
    let shell = call.get_flag_expr("shell").map(|expr| {
        let shell_expr = working_set.get_span_contents(expr.span);

        String::from_utf8(shell_expr.to_vec())
            .map_err(|_| ParseError::NonUtf8(expr.span))
            .and_then(|name| {
                canonicalize_with(&name, cwd)
                    .map_err(|_| ParseError::RegisteredFileNotFound(name, expr.span))
            })
            .and_then(|path| {
                if path.exists() & path.is_file() {
                    Ok(path)
                } else {
                    Err(ParseError::RegisteredFileNotFound(
                        format!("{path:?}"),
                        expr.span,
                    ))
                }
            })
    });

    let shell = match shell {
        None => None,
        Some(path) => match path {
            Ok(path) => Some(path),
            Err(err) => {
                return (
                    Pipeline::from_vec(vec![Expression {
                        expr: Expr::Call(call),
                        span: call_span,
                        ty: Type::Any,
                        custom_completion: None,
                    }]),
                    Some(err),
                );
            }
        },
    };

    // We need the current environment variables for `python` based plugins
    // Or we'll likely have a problem when a plugin is implemented in a virtual Python environment.
    let stack = Stack::new();
    let current_envs =
        nu_engine::env::env_to_strings(working_set.permanent_state, &stack).unwrap_or_default();
    let error = match signature {
        Some(signature) => arguments.and_then(|path| {
            // restrict plugin file name starts with `nu_plugin_`
            let f_name = path
                .file_name()
                .map(|s| s.to_string_lossy().starts_with("nu_plugin_"));

            if let Some(true) = f_name {
                signature.map(|signature| {
                    let plugin_decl = PluginDeclaration::new(path, signature, shell);
                    working_set.add_decl(Box::new(plugin_decl));
                    working_set.mark_plugins_file_dirty();
                })
            } else {
                Ok(())
            }
        }),
        None => arguments.and_then(|path| {
            // restrict plugin file name starts with `nu_plugin_`
            let f_name = path
                .file_name()
                .map(|s| s.to_string_lossy().starts_with("nu_plugin_"));

            if let Some(true) = f_name {
                get_signature(path.as_path(), &shell, &current_envs)
                    .map_err(|err| {
                        ParseError::LabeledError(
                            "Error getting signatures".into(),
                            err.to_string(),
                            spans[0],
                        )
                    })
                    .map(|signatures| {
                        for signature in signatures {
                            // create plugin command declaration (need struct impl Command)
                            // store declaration in working set
                            let plugin_decl =
                                PluginDeclaration::new(path.clone(), signature, shell.clone());

                            working_set.add_decl(Box::new(plugin_decl));
                        }

                        working_set.mark_plugins_file_dirty();
                    })
            } else {
                Ok(())
            }
        }),
    }
    .err();

    (
        Pipeline::from_vec(vec![Expression {
            expr: Expr::Call(call),
            span: call_span,
            ty: Type::Nothing,
            custom_completion: None,
        }]),
        error,
    )
}

/// This helper function is used to find files during parsing
///
/// First, the actual current working directory is selected as
///   a) the directory of a file currently being parsed
///   b) current working directory (PWD)
///
/// Then, if the file is not found in the actual cwd, NU_LIB_DIRS is checked.
/// If there is a relative path in NU_LIB_DIRS, it is assumed to be relative to the actual cwd
/// determined in the first step.
///
/// Always returns an absolute path
pub fn find_in_dirs(
    filename: &str,
    working_set: &StateWorkingSet,
    cwd: &str,
    dirs_env: &str,
) -> Option<PathBuf> {
    // Choose whether to use file-relative or PWD-relative path
    let actual_cwd = if let Some(currently_parsed_cwd) = &working_set.currently_parsed_cwd {
        currently_parsed_cwd.as_path()
    } else {
        Path::new(cwd)
    };

    if let Ok(p) = canonicalize_with(filename, actual_cwd) {
        Some(p)
    } else {
        let path = Path::new(filename);

        if path.is_relative() {
            if let Some(lib_dirs) = working_set.get_env_var(dirs_env) {
                if let Ok(dirs) = lib_dirs.as_list() {
                    for lib_dir in dirs {
                        if let Ok(dir) = lib_dir.as_path() {
                            // make sure the dir is absolute path
                            if let Ok(dir_abs) = canonicalize_with(dir, actual_cwd) {
                                if let Ok(path) = canonicalize_with(filename, dir_abs) {
                                    return Some(path);
                                }
                            }
                        }
                    }

                    None
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }
}
