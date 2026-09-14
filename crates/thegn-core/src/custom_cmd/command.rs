//! Conservative shell admission. Unsupported shell grammar is an error, not
//! an invitation to guess whether quoting a value would be safe in context.

use super::{TemplateCtx, TemplateError, resolve};
use crate::config::GitCommand;

#[derive(Clone)]
enum Program {
    Argv(Vec<String>),
    Shell(String),
}

/// Only compile can construct this type. Debug never prints repository text
/// or prompt responses; the executable projection is made at the last edge.
#[derive(Clone)]
pub struct ExpandedCommand(Program);

impl std::fmt::Debug for ExpandedCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExpandedCommand([redacted])")
    }
}

impl ExpandedCommand {
    /// Terminal execution must not reinterpret POSIX quoting with an arbitrary
    /// user shell (fish, PowerShell or cmd). Argv needs no shell at all.
    pub fn terminal_argv(&self) -> Vec<String> {
        match &self.0 {
            Program::Argv(argv) => argv.clone(),
            Program::Shell(script) => vec!["sh".into(), "-c".into(), script.clone()],
        }
    }

    pub fn shell_script(&self) -> String {
        match &self.0 {
            Program::Argv(argv) => crate::util::sh_join(argv),
            Program::Shell(script) => script.clone(),
        }
    }

    pub fn command(&self, loc: &crate::remote::GitLoc) -> std::process::Command {
        let mut command = match &self.0 {
            Program::Argv(argv) => {
                let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
                loc.cli_command(&argv[0], &args)
            }
            Program::Shell(script) => loc.sh_command(script),
        };
        for var in crate::util::GIT_ENV_VARS {
            command.env_remove(var);
        }
        command
    }
}

const MAX_TEMPLATE: usize = 64 * 1024;
const MAX_EXPANDED: usize = 1024 * 1024;
const MAX_ARGS: usize = 256;

fn err(message: &'static str) -> TemplateError {
    TemplateError::Policy(message)
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Argv,
    Safe,
    Raw,
}

fn mode(cmd: &GitCommand) -> Result<Mode, TemplateError> {
    if cmd.argv.len() > MAX_ARGS {
        return Err(err("argv exceeds 256 entries"));
    }
    if !cmd.argv.is_empty() {
        if !cmd.command.is_empty() || cmd.template_policy.is_some() {
            return Err(err(
                "argv cannot be combined with command or template_policy",
            ));
        }
        if cmd.argv[0].is_empty() || cmd.argv[0].contains("{{") {
            return Err(err("argv[0] must be a nonempty literal executable"));
        }
        return Ok(Mode::Argv);
    }
    if cmd.command.trim().is_empty() {
        return Err(err("set either command or argv"));
    }
    match cmd.template_policy.as_deref() {
        Some("safe") => Ok(Mode::Safe),
        Some("dangerously_allow_raw") => Ok(Mode::Raw),
        None if !cmd.command.contains("{{") => Ok(Mode::Safe),
        None => Err(err(
            "legacy placeholders are disabled; migrate to argv or set template_policy = safe with complete unquoted argument placeholders",
        )),
        _ => Err(err("template_policy must be safe or dangerously_allow_raw")),
    }
}

fn placeholder(body: &str, mode: Mode) -> Result<(&str, bool), TemplateError> {
    let (path, filter) = body
        .trim()
        .split_once('|')
        .map_or((body.trim(), None), |(p, f)| (p.trim(), Some(f.trim())));
    // Resolve only the path grammar, never user values, during validation.
    match resolve(path, &TemplateCtx::default()) {
        Ok(_) | Err(TemplateError::MissingValue(_)) => {}
        Err(_) => return Err(err("unknown or malformed placeholder path")),
    }
    match (mode, filter) {
        (_, None) => Ok((path, false)),
        (Mode::Safe | Mode::Raw, Some("quote")) => Ok((path, false)),
        (Mode::Raw, Some("dangerously_unquoted_shell")) => Ok((path, true)),
        _ => Err(err("unknown or disallowed placeholder filter")),
    }
}

fn append(out: &mut String, value: &str, budget: usize) -> Result<(), TemplateError> {
    if value.len() > budget.saturating_sub(out.len()) {
        return Err(err("expanded command exceeds its size limit"));
    }
    out.push_str(value);
    Ok(())
}

// Mirrors sh_quote's representation length without constructing it. Check
// expansion amplification before allocating the quoted representation.
fn quoted_len(value: &str) -> usize {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        value.len()
    } else {
        value
            .len()
            .saturating_add(
                value
                    .bytes()
                    .filter(|&b| b == b'\'')
                    .count()
                    .saturating_mul(3),
            )
            .saturating_add(2)
    }
}

fn substitute(
    template: &str,
    mode: Mode,
    ctx: Option<&TemplateCtx>,
    budget: usize,
) -> Result<String, TemplateError> {
    if template.len() > MAX_TEMPLATE || template.contains('\0') {
        return Err(err("template is oversized or contains NUL"));
    }
    if mode != Mode::Argv && template.contains("{{") {
        validate_shell(template)?;
    }
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        append(&mut out, &rest[..start], budget)?;
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| err("unterminated placeholder"))?;
        let (path, raw) = placeholder(&after[..end], mode)?;
        if let Some(ctx) = ctx {
            let value = resolve(path, ctx)
                .map_err(|_| err("required selection or prompt value is unavailable"))?;
            if value.contains('\0') || value.len() > MAX_EXPANDED {
                return Err(err("placeholder value is oversized or contains NUL"));
            }
            if mode == Mode::Argv || raw {
                append(&mut out, &value, budget)?;
            } else {
                if quoted_len(&value) > budget.saturating_sub(out.len()) {
                    return Err(err("quoted command exceeds its size limit"));
                }
                append(&mut out, &crate::util::sh_quote(&value), budget)?;
            }
        }
        rest = &after[end + 2..];
    }
    append(&mut out, rest, budget)?;
    Ok(out)
}

/// Only a small, explicit shell subset is admitted when data placeholders are
/// present. Static commands without placeholders preserve shell semantics.
fn validate_shell(template: &str) -> Result<(), TemplateError> {
    let bytes = template.as_bytes();
    let mut quote = None;
    let mut i = 0;
    let mut command_start = true;
    let mut word = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        if bytes[i..].starts_with(b"{{") {
            if command_start || quote.is_some() || (i > 0 && !bytes[i - 1].is_ascii_whitespace()) {
                return Err(err(
                    "shell placeholders must be complete unquoted words; use argv for embedded values",
                ));
            }
            let end = template[i + 2..]
                .find("}}")
                .ok_or_else(|| err("unterminated placeholder"))?
                + i
                + 4;
            if end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                return Err(err(
                    "separate shell placeholders from operators and adjacent words with whitespace",
                ));
            }
            i = end;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            } else if q == b'"' && matches!(b, b'\\' | b'$' | b'`') {
                return Err(err(
                    "shell substitutions and escaped double quotes are unsupported with placeholders; use argv",
                ));
            }
        } else {
            match b {
                b'\'' | b'"' => quote = Some(b),
                b'\\' | b'$' | b'`' | b'#' | b'(' | b')' | b'{' | b'}' | b'<' | b'>' | b'=' => {
                    return Err(err(
                        "unsupported shell context with placeholders; use argv or a literal script",
                    ));
                }
                _ => {}
            }
        }
        if quote.is_none() && (b.is_ascii_whitespace() || matches!(b, b';' | b'|' | b'&')) {
            if !word.is_empty() {
                if command_start
                    && matches!(
                        word.as_str(),
                        "if" | "then"
                            | "else"
                            | "elif"
                            | "fi"
                            | "for"
                            | "while"
                            | "until"
                            | "do"
                            | "done"
                            | "case"
                            | "esac"
                            | "in"
                            | "!"
                            | "function"
                    )
                {
                    return Err(err(
                        "shell control structures are unsupported with placeholders; use argv",
                    ));
                }
                command_start = false;
                word.clear();
            }
            if matches!(b, b';' | b'|' | b'&' | b'\n') {
                command_start = true;
            }
        } else {
            word.push(char::from(b));
        }
        i += 1;
    }
    if quote.is_some() {
        return Err(err("unterminated shell quote"));
    }
    Ok(())
}

fn validate(cmd: &GitCommand) -> Result<Mode, TemplateError> {
    let mode = mode(cmd)?;
    let templates: Vec<&str> = if mode == Mode::Argv {
        cmd.argv.iter().map(String::as_str).collect()
    } else {
        vec![&cmd.command]
    };
    if templates.iter().map(|s| s.len()).sum::<usize>() > MAX_TEMPLATE {
        return Err(err("templates exceed their combined size limit"));
    }
    for template in templates {
        substitute(template, mode, None, MAX_EXPANDED)?;
    }
    Ok(mode)
}

pub fn validate_commands(commands: &[GitCommand]) -> Vec<String> {
    commands
        .iter()
        .enumerate()
        .filter_map(|(i, cmd)| {
            validate(cmd)
                .err()
                .map(|e| format!("git_commands[{i}]: {e}"))
        })
        .collect()
}

pub fn compile(cmd: &GitCommand, ctx: &TemplateCtx) -> Result<ExpandedCommand, TemplateError> {
    let mode = validate(cmd)?;
    let program = if mode == Mode::Argv {
        let mut args = Vec::with_capacity(cmd.argv.len());
        let mut remaining = MAX_EXPANDED;
        let mut wire_remaining = MAX_EXPANDED;
        for template in &cmd.argv {
            let arg = substitute(template, mode, Some(ctx), remaining)?;
            let wire_len = quoted_len(&arg).saturating_add(usize::from(!args.is_empty()));
            if wire_len > wire_remaining {
                return Err(err("quoted argv exceeds its combined size limit"));
            }
            remaining -= arg.len();
            wire_remaining -= wire_len;
            args.push(arg);
        }
        Program::Argv(args)
    } else {
        Program::Shell(substitute(&cmd.command, mode, Some(ctx), MAX_EXPANDED)?)
    };
    Ok(ExpandedCommand(program))
}
