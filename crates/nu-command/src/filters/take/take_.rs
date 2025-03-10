use nu_engine::CallExt;
use nu_protocol::ast::Call;
use nu_protocol::engine::{Command, EngineState, Stack};
use nu_protocol::{
    Category, Example, IntoInterruptiblePipelineData, PipelineData, ShellError, Signature, Span,
    SyntaxShape, Type, Value,
};

#[derive(Clone)]
pub struct Take;

impl Command for Take {
    fn name(&self) -> &str {
        "take"
    }

    fn signature(&self) -> Signature {
        Signature::build("take")
            .input_output_types(vec![
                (Type::Table(vec![]), Type::Table(vec![])),
                (
                    Type::List(Box::new(Type::Any)),
                    Type::List(Box::new(Type::Any)),
                ),
                (Type::Binary, Type::Binary),
                (Type::Range, Type::List(Box::new(Type::Number))),
            ])
            .required(
                "n",
                SyntaxShape::Int,
                "starting from the front, the number of elements to return",
            )
            .category(Category::Filters)
    }

    fn usage(&self) -> &str {
        "Take only the first n elements of a list, or the first n bytes of a binary value."
    }

    fn search_terms(&self) -> Vec<&str> {
        vec!["first", "slice", "head"]
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let rows_desired: usize = call.req(engine_state, stack, 0)?;

        let ctrlc = engine_state.ctrlc.clone();
        let metadata = input.metadata();

        match input {
            PipelineData::Value(val, _) => match val {
                Value::List { vals, .. } => Ok(vals
                    .into_iter()
                    .take(rows_desired)
                    .into_pipeline_data(ctrlc)
                    .set_metadata(metadata)),
                Value::Binary { val, span } => {
                    let slice: Vec<u8> = val.into_iter().take(rows_desired).collect();
                    Ok(PipelineData::Value(
                        Value::Binary { val: slice, span },
                        metadata,
                    ))
                }
                Value::Range { val, .. } => Ok(val
                    .into_range_iter(ctrlc.clone())?
                    .take(rows_desired)
                    .into_pipeline_data(ctrlc)
                    .set_metadata(metadata)),
                // Propagate errors by explicitly matching them before the final case.
                Value::Error { error } => Err(error),
                other => Err(ShellError::OnlySupportsThisInputType {
                    exp_input_type: "list, binary or range".into(),
                    wrong_type: other.get_type().to_string(),
                    dst_span: call.head,
                    src_span: other.expect_span(),
                }),
            },
            PipelineData::ListStream(ls, metadata) => Ok(ls
                .take(rows_desired)
                .into_pipeline_data(ctrlc)
                .set_metadata(metadata)),
            PipelineData::ExternalStream { span, .. } => {
                Err(ShellError::OnlySupportsThisInputType {
                    exp_input_type: "list, binary or range".into(),
                    wrong_type: "raw data".into(),
                    dst_span: call.head,
                    src_span: span,
                })
            }
            PipelineData::Empty => Err(ShellError::OnlySupportsThisInputType {
                exp_input_type: "list, binary or range".into(),
                wrong_type: "null".into(),
                dst_span: call.head,
                src_span: call.head,
            }),
        }
    }

    fn examples(&self) -> Vec<Example> {
        vec![
            Example {
                description: "Return the first item of a list/table",
                example: "[1 2 3] | take 1",
                result: Some(Value::List {
                    vals: vec![Value::test_int(1)],
                    span: Span::test_data(),
                }),
            },
            Example {
                description: "Return the first 2 items of a list/table",
                example: "[1 2 3] | take 2",
                result: Some(Value::List {
                    vals: vec![Value::test_int(1), Value::test_int(2)],
                    span: Span::test_data(),
                }),
            },
            Example {
                description: "Return the first two rows of a table",
                example: "[[editions]; [2015] [2018] [2021]] | take 2",
                result: Some(Value::List {
                    vals: vec![
                        Value::test_record(vec!["editions"], vec![Value::test_int(2015)]),
                        Value::test_record(vec!["editions"], vec![Value::test_int(2018)]),
                    ],
                    span: Span::test_data(),
                }),
            },
            Example {
                description: "Return the first 2 bytes of a binary value",
                example: "0x[01 23 45] | take 2",
                result: Some(Value::Binary {
                    val: vec![0x01, 0x23],
                    span: Span::test_data(),
                }),
            },
            Example {
                description: "Return the first 3 elements of a range",
                example: "1..10 | take 3",
                result: Some(Value::List {
                    vals: vec![Value::test_int(1), Value::test_int(2), Value::test_int(3)],
                    span: Span::test_data(),
                }),
            },
        ]
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_examples() {
        use crate::test_examples;

        test_examples(Take {})
    }
}
