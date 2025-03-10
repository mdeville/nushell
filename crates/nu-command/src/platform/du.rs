use crate::{DirBuilder, DirInfo, FileInfo};
use nu_engine::CallExt;
use nu_glob::{GlobError, MatchOptions, Pattern};
use nu_protocol::{
    ast::Call,
    engine::{Command, EngineState, Stack},
    Category, Example, IntoInterruptiblePipelineData, PipelineData, ShellError, Signature, Spanned,
    SyntaxShape, Type, Value,
};
use serde::Deserialize;
use std::path::PathBuf;

const GLOB_PARAMS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
    recursive_match_hidden_dir: true,
};

#[derive(Clone)]
pub struct Du;

#[derive(Deserialize, Clone, Debug)]
pub struct DuArgs {
    path: Option<Spanned<PathBuf>>,
    all: bool,
    deref: bool,
    exclude: Option<Spanned<String>>,
    #[serde(rename = "max-depth")]
    max_depth: Option<Spanned<i64>>,
    #[serde(rename = "min-size")]
    min_size: Option<Spanned<i64>>,
}

impl Command for Du {
    fn name(&self) -> &str {
        "du"
    }

    fn usage(&self) -> &str {
        "Find disk usage sizes of specified items."
    }

    fn signature(&self) -> Signature {
        Signature::build("du")
            .input_output_types(vec![(Type::Nothing, Type::Table(vec![]))])
            .allow_variants_without_examples(true)
            .optional("path", SyntaxShape::GlobPattern, "starting directory")
            .switch(
                "all",
                "Output file sizes as well as directory sizes",
                Some('a'),
            )
            .switch(
                "deref",
                "Dereference symlinks to their targets for size",
                Some('r'),
            )
            .named(
                "exclude",
                SyntaxShape::GlobPattern,
                "Exclude these file names",
                Some('x'),
            )
            .named(
                "max-depth",
                SyntaxShape::Int,
                "Directory recursion limit",
                Some('d'),
            )
            .named(
                "min-size",
                SyntaxShape::Int,
                "Exclude files below this size",
                Some('m'),
            )
            .category(Category::Core)
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let tag = call.head;
        let min_size: Option<Spanned<i64>> = call.get_flag(engine_state, stack, "min-size")?;
        let max_depth: Option<Spanned<i64>> = call.get_flag(engine_state, stack, "max-depth")?;
        if let Some(ref max_depth) = max_depth {
            if max_depth.item < 0 {
                return Err(ShellError::NeedsPositiveValue(max_depth.span));
            }
        }
        if let Some(ref min_size) = min_size {
            if min_size.item < 0 {
                return Err(ShellError::NeedsPositiveValue(min_size.span));
            }
        }
        let args = DuArgs {
            path: call.opt(engine_state, stack, 0)?,
            all: call.has_flag("all"),
            deref: call.has_flag("deref"),
            exclude: call.get_flag(engine_state, stack, "exclude")?,
            max_depth,
            min_size,
        };

        let exclude = args.exclude.map_or(Ok(None), move |x| {
            Pattern::new(&x.item).map(Some).map_err(|e| {
                ShellError::GenericError(
                    "glob error".to_string(),
                    e.msg.to_string(),
                    Some(x.span),
                    None,
                    Vec::new(),
                )
            })
        })?;

        let include_files = args.all;
        let mut paths = match args.path {
            Some(p) => {
                let item = p.item.to_str().expect("Why isn't this encoded properly?");
                match nu_glob::glob_with(item, GLOB_PARAMS) {
                    // Convert the PatternError to a ShellError, preserving the span
                    // of the inputted glob.
                    Err(e) => {
                        return Err(ShellError::GenericError(
                            "glob error".to_string(),
                            e.msg.to_string(),
                            Some(p.span),
                            None,
                            Vec::new(),
                        ))
                    }
                    Ok(path) => path,
                }
            }
            // The * pattern should never fail.
            None => nu_glob::glob_with("*", GLOB_PARAMS).expect("du: * pattern failed to glob"),
        }
        .filter(move |p| {
            if include_files {
                true
            } else {
                match p {
                    Ok(f) if f.is_dir() => true,
                    Err(e) if e.path().is_dir() => true,
                    _ => false,
                }
            }
        })
        .map(|v| v.map_err(glob_err_into));

        let all = args.all;
        let deref = args.deref;
        let max_depth = args.max_depth.map(|f| f.item as u64);
        let min_size = args.min_size.map(|f| f.item as u64);

        let params = DirBuilder {
            tag,
            min: min_size,
            deref,
            exclude,
            all,
        };

        let mut output: Vec<Value> = vec![];
        for p in paths.by_ref() {
            match p {
                Ok(a) => {
                    if a.is_dir() {
                        output.push(
                            DirInfo::new(a, &params, max_depth, engine_state.ctrlc.clone()).into(),
                        );
                    } else if let Ok(v) = FileInfo::new(a, deref, tag) {
                        output.push(v.into());
                    }
                }
                Err(e) => {
                    output.push(Value::Error { error: e });
                }
            }
        }

        Ok(output.into_pipeline_data(engine_state.ctrlc.clone()))
    }

    fn examples(&self) -> Vec<Example> {
        vec![Example {
            description: "Disk usage of the current directory",
            example: "du",
            result: None,
        }]
    }
}

fn glob_err_into(e: GlobError) -> ShellError {
    let e = e.into_error();
    ShellError::from(e)
}

#[cfg(test)]
mod tests {
    use super::Du;

    #[test]
    fn examples_work_as_expected() {
        use crate::test_examples;
        test_examples(Du {})
    }
}
