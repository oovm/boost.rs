//! Help text rendering. Pure functions producing display lines; the caller
//! routes them to stdout (human mode) or the stderr writer (JSON mode).

use std::collections::BTreeMap;

use crate::diag::localized_or_id;
use crate::ports::Localizer;
use crate::spec::{Command, OptionSpec, ValueDecl};

/// Renders the root help: optional intro, usage, command list, option list.
pub(crate) fn root_help(
    cli_name: &str,
    intro: Option<&str>,
    global_options: &[OptionSpec],
    commands: &[Command],
    localizer: &dyn Localizer,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(key) = intro {
        lines.push(localized_or_id(localizer, key, &BTreeMap::new()).unwrap_or_default());
        lines.push(String::new());
    }
    let mut args = BTreeMap::new();
    args.insert("name".to_string(), cli_name.to_string());
    lines.push(
        localized_or_id(localizer, "commander.ui.usage", &args)
            .unwrap_or_else(|| format!("Usage: {cli_name} <command> [options]")),
    );
    lines.push(String::new());
    lines.extend(help_section(
        localizer,
        "commander.ui.commands",
        "Commands:",
        &visible_commands(commands),
    ));
    let opts = union_options(global_options, commands);
    lines.extend(help_section(localizer, "commander.ui.options", "Options:", &option_rows(&opts)));
    trim_trailing_blanks(lines).join("\n")
}

/// Renders one command's help: title, subcommand list, option list.
pub(crate) fn command_help(
    cli_name: &str,
    command: &Command,
    path: &[String],
    global_options: &[OptionSpec],
    localizer: &dyn Localizer,
) -> String {
    let title = format!("{cli_name} {}", path.join(" "));
    let heading = match &command.help_key {
        Some(key) => match localized_or_id(localizer, key, &BTreeMap::new()) {
            Some(text) => format!("{title} — {text}"),
            None => title,
        },
        None => title,
    };
    let mut lines: Vec<String> = vec![heading];
    let children = visible_commands(&command.subcommands);
    if !children.is_empty() {
        lines.push(String::new());
        lines.extend(help_section(localizer, "commander.ui.commands", "Commands:", &children));
    }
    let mut opts: Vec<OptionSpec> = Vec::new();
    for option in global_options.iter().filter(|o| !o.hidden) {
        opts.push(option.clone());
    }
    for option in command.options.iter().filter(|o| !o.hidden) {
        if !opts.iter().any(|seen| seen.long == option.long) {
            opts.push(option.clone());
        }
    }
    let rows = option_rows(&opts);
    if !rows.is_empty() {
        lines.push(String::new());
        lines.extend(help_section(localizer, "commander.ui.options", "Options:", &rows));
    }
    trim_trailing_blanks(lines).join("\n")
}

fn visible_commands(commands: &[Command]) -> Vec<(String, Option<String>)> {
    commands
        .iter()
        .filter(|c| !c.name.is_empty())
        .map(|c| (c.name.clone(), c.help_key.clone()))
        .collect()
}

fn option_rows(options: &[OptionSpec]) -> Vec<(String, Option<String>)> {
    options
        .iter()
        .filter(|o| !o.hidden)
        .map(|o| {
            let mut token = match o.short {
                Some(c) => format!("-{c}, --{}", o.long),
                None => format!("--{}", o.long),
            };
            match o.value {
                ValueDecl::Flag => {}
                ValueDecl::Required => token.push_str(" <value>"),
                ValueDecl::OptionalValue => token.push_str(" [value]"),
            }
            if o.repeatable {
                token.push_str("...");
            }
            (token, o.help_key.clone())
        })
        .collect()
}

fn help_section(
    localizer: &dyn Localizer,
    header_key: &str,
    header_fallback: &str,
    rows: &[(String, Option<String>)],
) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let header = localized_or_id(localizer, header_key, &BTreeMap::new())
        .unwrap_or_else(|| header_fallback.to_string());
    let width = rows.iter().map(|(token, _)| token.chars().count()).max().unwrap_or(0);
    let mut lines = vec![format!("{header}")];
    for (token, help) in rows {
        let text = match help {
            Some(key) => localized_or_id(localizer, key, &BTreeMap::new()).unwrap_or_default(),
            None => String::new(),
        };
        let pad = " ".repeat(width - token.chars().count() + 2);
        if text.is_empty() {
            lines.push(format!("  {token}"));
        } else {
            lines.push(format!("  {token}{pad}{text}"));
        }
    }
    lines
}

fn union_options(global_options: &[OptionSpec], commands: &[Command]) -> Vec<OptionSpec> {
    let mut opts: Vec<OptionSpec> = Vec::new();
    for option in global_options {
        if !opts.iter().any(|seen: &OptionSpec| seen.long == option.long) {
            opts.push(option.clone());
        }
    }
    for command in commands {
        for option in &command.options {
            if !opts.iter().any(|seen| seen.long == option.long) {
                opts.push(option.clone());
            }
        }
    }
    opts
}

fn trim_trailing_blanks(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}
