use crate::CliError;
use log::trace;
use nu_engine::eval_block;
use nu_parser::{lex, parse, unescape_unquote_string, Token, TokenContents};
use nu_protocol::engine::StateWorkingSet;
use nu_protocol::{
    ast::Call,
    engine::{EngineState, Stack},
    PipelineData, ShellError, Span, Value,
};
#[cfg(windows)]
use nu_utils::enable_vt_processing;
use std::io::Write;
use std::path::PathBuf;

pub fn print_pipeline_data(
    input: PipelineData,
    engine_state: &EngineState,
    stack: &mut Stack,
) -> Result<(), ShellError> {
    // If the table function is in the declarations, then we can use it
    // to create the table value that will be printed in the terminal

    let config = stack.get_config().unwrap_or_default();

    let stdout = std::io::stdout();

    if let PipelineData::ExternalStream {
        stdout: stream,
        exit_code,
        ..
    } = input
    {
        if let Some(stream) = stream {
            for s in stream {
                let _ = stdout.lock().write_all(s?.as_binary()?);
            }
        }

        // Make sure everything has finished
        if let Some(exit_code) = exit_code {
            let _: Vec<_> = exit_code.into_iter().collect();
        }

        return Ok(());
    }

    match engine_state.find_decl("table".as_bytes()) {
        Some(decl_id) => {
            let table = engine_state.get_decl(decl_id).run(
                engine_state,
                stack,
                &Call::new(Span::new(0, 0)),
                input,
            )?;

            for item in table {
                let stdout = std::io::stdout();

                if let Value::Error { error } = item {
                    return Err(error);
                }

                let mut out = item.into_string("\n", &config);
                out.push('\n');

                match stdout.lock().write_all(out.as_bytes()) {
                    Ok(_) => (),
                    Err(err) => eprintln!("{}", err),
                };
            }
        }
        None => {
            for item in input {
                let stdout = std::io::stdout();

                if let Value::Error { error } = item {
                    return Err(error);
                }

                let mut out = item.into_string("\n", &config);
                out.push('\n');

                match stdout.lock().write_all(out.as_bytes()) {
                    Ok(_) => (),
                    Err(err) => eprintln!("{}", err),
                };
            }
        }
    };

    Ok(())
}

// This will collect environment variables from std::env and adds them to a stack.
//
// In order to ensure the values have spans, it first creates a dummy file, writes the collected
// env vars into it (in a "NAME"="value" format, quite similar to the output of the Unix 'env'
// tool), then uses the file to get the spans. The file stays in memory, no filesystem IO is done.
pub fn gather_parent_env_vars(engine_state: &mut EngineState) {
    gather_env_vars(std::env::vars(), engine_state);
}

fn gather_env_vars(vars: impl Iterator<Item = (String, String)>, engine_state: &mut EngineState) {
    fn report_capture_error(engine_state: &EngineState, env_str: &str, msg: &str) {
        let working_set = StateWorkingSet::new(engine_state);
        report_error(
            &working_set,
            &ShellError::GenericError(
                format!("Environment variable was not captured: {}", env_str),
                "".to_string(),
                None,
                Some(msg.into()),
                Vec::new(),
            ),
        );
    }

    fn put_env_to_fake_file(name: &str, val: &str, fake_env_file: &mut String) {
        fn push_string_literal(s: &str, fake_env_file: &mut String) {
            fake_env_file.push('"');
            for c in s.chars() {
                if c == '\\' || c == '"' {
                    fake_env_file.push('\\');
                }
                fake_env_file.push(c);
            }
            fake_env_file.push('"');
        }

        push_string_literal(name, fake_env_file);
        fake_env_file.push('=');
        push_string_literal(val, fake_env_file);
        fake_env_file.push('\n');
    }

    let mut fake_env_file = String::new();
    let mut has_pwd = false;

    // Write all the env vars into a fake file
    for (name, val) in vars {
        if name == "PWD" {
            has_pwd = true;
        }
        put_env_to_fake_file(&name, &val, &mut fake_env_file);
    }

    if !has_pwd {
        match std::env::current_dir() {
            Ok(cwd) => {
                put_env_to_fake_file("PWD", &cwd.to_string_lossy(), &mut fake_env_file);
            }
            Err(e) => {
                // Could not capture current working directory
                let working_set = StateWorkingSet::new(engine_state);
                report_error(
                    &working_set,
                    &ShellError::GenericError(
                        "Current directory not found".to_string(),
                        "".to_string(),
                        None,
                        Some(format!("Retrieving current directory failed: {:?}", e)),
                        Vec::new(),
                    ),
                );
            }
        }
    }

    // Lex the fake file, assign spans to all environment variables and add them
    // to stack
    let span_offset = engine_state.next_span_start();

    engine_state.add_file(
        "Host Environment Variables".to_string(),
        fake_env_file.as_bytes().to_vec(),
    );

    let (tokens, _) = lex(fake_env_file.as_bytes(), span_offset, &[], &[], true);

    for token in tokens {
        if let Token {
            contents: TokenContents::Item,
            span: full_span,
        } = token
        {
            let contents = engine_state.get_span_contents(&full_span);
            let (parts, _) = lex(contents, full_span.start, &[], &[b'='], true);

            let name = if let Some(Token {
                contents: TokenContents::Item,
                span,
            }) = parts.get(0)
            {
                let bytes = engine_state.get_span_contents(span);

                if bytes.len() < 2 {
                    report_capture_error(
                        engine_state,
                        &String::from_utf8_lossy(contents),
                        "Got empty name.",
                    );

                    continue;
                }

                let (bytes, parse_error) = unescape_unquote_string(bytes, *span);

                if parse_error.is_some() {
                    report_capture_error(
                        engine_state,
                        &String::from_utf8_lossy(contents),
                        "Got unparsable name.",
                    );

                    continue;
                }

                bytes
            } else {
                report_capture_error(
                    engine_state,
                    &String::from_utf8_lossy(contents),
                    "Got empty name.",
                );

                continue;
            };

            let value = if let Some(Token {
                contents: TokenContents::Item,
                span,
            }) = parts.get(2)
            {
                let bytes = engine_state.get_span_contents(span);

                if bytes.len() < 2 {
                    report_capture_error(
                        engine_state,
                        &String::from_utf8_lossy(contents),
                        "Got empty value.",
                    );

                    continue;
                }

                let (bytes, parse_error) = unescape_unquote_string(bytes, *span);

                if parse_error.is_some() {
                    report_capture_error(
                        engine_state,
                        &String::from_utf8_lossy(contents),
                        "Got unparsable value.",
                    );

                    continue;
                }

                Value::String {
                    val: bytes,
                    span: *span,
                }
            } else {
                report_capture_error(
                    engine_state,
                    &String::from_utf8_lossy(contents),
                    "Got empty value.",
                );

                continue;
            };

            // stack.add_env_var(name, value);
            engine_state.env_vars.insert(name, value);
        }
    }
}

pub fn eval_source(
    engine_state: &mut EngineState,
    stack: &mut Stack,
    source: &[u8],
    fname: &str,
    input: PipelineData,
) -> bool {
    trace!("eval_source");

    let (block, delta) = {
        let mut working_set = StateWorkingSet::new(engine_state);
        let (output, err) = parse(
            &mut working_set,
            Some(fname), // format!("entry #{}", entry_num)
            source,
            false,
            &[],
        );
        if let Some(err) = err {
            set_last_exit_code(stack, 1);
            report_error(&working_set, &err);
            return false;
        }

        (output, working_set.render())
    };

    let cwd = match nu_engine::env::current_dir_str(engine_state, stack) {
        Ok(p) => PathBuf::from(p),
        Err(e) => {
            let working_set = StateWorkingSet::new(engine_state);
            report_error(&working_set, &e);
            get_init_cwd()
        }
    };

    if let Err(err) = engine_state.merge_delta(delta, Some(stack), &cwd) {
        let working_set = StateWorkingSet::new(engine_state);
        report_error(&working_set, &err);
    }

    match eval_block(engine_state, stack, &block, input, false, false) {
        Ok(mut pipeline_data) => {
            if let PipelineData::ExternalStream { exit_code, .. } = &mut pipeline_data {
                if let Some(exit_code) = exit_code.take().and_then(|it| it.last()) {
                    stack.add_env_var("LAST_EXIT_CODE".to_string(), exit_code);
                } else {
                    set_last_exit_code(stack, 0);
                }
            } else {
                set_last_exit_code(stack, 0);
            }

            if let Err(err) = print_pipeline_data(pipeline_data, engine_state, stack) {
                let working_set = StateWorkingSet::new(engine_state);

                report_error(&working_set, &err);

                return false;
            }

            // reset vt processing, aka ansi because illbehaved externals can break it
            #[cfg(windows)]
            {
                let _ = enable_vt_processing();
            }
        }
        Err(err) => {
            set_last_exit_code(stack, 1);

            let working_set = StateWorkingSet::new(engine_state);

            report_error(&working_set, &err);

            return false;
        }
    }

    true
}

fn set_last_exit_code(stack: &mut Stack, exit_code: i64) {
    stack.add_env_var(
        "LAST_EXIT_CODE".to_string(),
        Value::Int {
            val: exit_code,
            span: Span { start: 0, end: 0 },
        },
    );
}

pub fn report_error(
    working_set: &StateWorkingSet,
    error: &(dyn miette::Diagnostic + Send + Sync + 'static),
) {
    eprintln!("Error: {:?}", CliError(error, working_set));
    // reset vt processing, aka ansi because illbehaved externals can break it
    #[cfg(windows)]
    {
        let _ = nu_utils::enable_vt_processing();
    }
}

pub fn get_init_cwd() -> PathBuf {
    match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => match std::env::var("PWD") {
            Ok(cwd) => PathBuf::from(cwd),
            Err(_) => match nu_path::home_dir() {
                Some(cwd) => cwd,
                None => PathBuf::new(),
            },
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_gather_env_vars() {
        let mut engine_state = EngineState::new();
        let symbols = r##" !"#$%&'()*+,-./:;<=>?@[\]^_`{|}~"##;

        gather_env_vars(
            [
                ("FOO".into(), "foo".into()),
                ("SYMBOLS".into(), symbols.into()),
                (symbols.into(), "symbols".into()),
            ]
            .into_iter(),
            &mut engine_state,
        );

        let env = engine_state.env_vars;

        assert!(matches!(env.get("FOO"), Some(Value::String { val, .. }) if val == "foo"));
        assert!(matches!(env.get("SYMBOLS"), Some(Value::String { val, .. }) if val == symbols));
        assert!(matches!(env.get(symbols), Some(Value::String { val, .. }) if val == "symbols"));
        assert!(env.get("PWD").is_some());
        assert_eq!(env.len(), 4);
    }
}
