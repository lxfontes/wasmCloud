use std::{
    collections::HashMap,
    io::{Error, ErrorKind},
    path::PathBuf,
    process::Command,
};

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use log::warn;
use serde_json::json;
use wash_lib::{
    cli::CommandOutput,
    config::{
        cfg_dir, DEFAULT_LATTICE_PREFIX, DEFAULT_NATS_HOST, DEFAULT_NATS_PORT,
        DEFAULT_NATS_TIMEOUT_MS,
    },
    context::{
        fs::{load_context, ContextDir},
        ContextManager, WashContext, HOST_CONFIG_NAME,
    },
    id::ClusterSeed,
};

use wash_lib::generate::{
    interactive::{prompt_for_choice, user_question},
    project_variables::StringEntry,
};

const CTX_DIR_NAME: &str = "contexts";

pub(crate) async fn handle_command(ctx_cmd: CtxCommand) -> Result<CommandOutput> {
    use CtxCommand::*;
    match ctx_cmd {
        List(cmd) => handle_list(cmd),
        Default(cmd) => handle_default(cmd),
        Edit(cmd) => handle_edit(cmd),
        New(cmd) => handle_new(cmd),
        Del(cmd) => handle_del(cmd),
    }
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum CtxCommand {
    /// Lists all stored contexts (JSON files) found in the context directory, with the exception of index.json
    #[clap(name = "list")]
    List(ListCommand),
    /// Delete a stored context
    #[clap(name = "del")]
    Del(DelCommand),
    /// Create a new context
    #[clap(name = "new")]
    New(NewCommand),
    /// Set the default context
    #[clap(name = "default")]
    Default(DefaultCommand),
    /// Edit a context directly using a text editor
    #[clap(name = "edit")]
    Edit(EditCommand),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ListCommand {
    /// Location of context files for managing. Defaults to $WASH_CONTEXTS ($HOME/.wash/contexts)
    #[clap(long = "directory", env = "WASH_CONTEXTS", hide_env_values = true)]
    directory: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DelCommand {
    /// Location of context files for managing. Defaults to $WASH_CONTEXTS ($HOME/.wash/contexts)
    #[clap(long = "directory", env = "WASH_CONTEXTS", hide_env_values = true)]
    directory: Option<PathBuf>,

    /// Name of the context to delete. If not supplied, the user will be prompted to select an existing context
    #[clap(name = "name")]
    name: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct NewCommand {
    /// Name of the context, will be sanitized to ensure it's a valid filename
    #[clap(name = "name", required_unless_present("interactive"))]
    pub(crate) name: Option<String>,

    /// Location of context files for managing. Defaults to $WASH_CONTEXTS ($HOME/.wash/contexts)
    #[clap(long = "directory", env = "WASH_CONTEXTS", hide_env_values = true)]
    directory: Option<PathBuf>,

    /// Create the context in an interactive terminal prompt, instead of an autogenerated default context
    #[clap(long = "interactive", short = 'i')]
    interactive: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DefaultCommand {
    /// Location of context files for managing. Defaults to $WASH_CONTEXTS ($HOME/.wash/contexts)
    #[clap(long = "directory", env = "WASH_CONTEXTS", hide_env_values = true)]
    directory: Option<PathBuf>,

    /// Name of the context to use for default. If not supplied, the user will be prompted to select a default
    #[clap(name = "name")]
    name: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct EditCommand {
    /// Location of context files for managing. Defaults to $WASH_CONTEXTS ($HOME/.wash/contexts)
    #[clap(long = "directory", env = "WASH_CONTEXTS", hide_env_values = true)]
    directory: Option<PathBuf>,

    /// Name of the context to edit, if not supplied the user will be prompted to select a context
    #[clap(name = "name")]
    pub(crate) name: Option<String>,

    /// Your terminal text editor of choice. This editor must be present in your $PATH, or an absolute filepath.
    #[clap(short = 'e', long = "editor", env = "EDITOR")]
    pub(crate) editor: String,
}

/// Lists all JSON files found in the context directory, with the exception of `index.json`
/// Being present in this list does not guarantee a valid context
fn handle_list(cmd: ListCommand) -> Result<CommandOutput> {
    let dir = ContextDir::new(context_dir(cmd.directory)?)?;
    ensure_host_config_context(&dir)?;

    let default_context = dir
        .default_context()?
        .unwrap_or_else(|| HOST_CONFIG_NAME.to_string());
    let contexts = dir.list_contexts()?;

    let text_contexts = contexts
        .iter()
        .map(|f| {
            if f == &default_context {
                format!("{f} (default)")
            } else {
                f.clone()
            }
        })
        .collect::<Vec<String>>()
        .join("\n");

    let mut map = HashMap::new();
    map.insert("contexts".to_string(), json!(contexts));
    map.insert("default".to_string(), json!(default_context));

    Ok(CommandOutput::new(
        format!(
            "== Contexts found in {} ==\n{}",
            dir.display(),
            text_contexts
        ),
        map,
    ))
}

/// Handles selecting a default context, which can be selected in the terminal or provided as an argument
fn handle_default(cmd: DefaultCommand) -> Result<CommandOutput> {
    let dir = ContextDir::new(context_dir(cmd.directory)?)?;
    ensure_host_config_context(&dir)?;

    let new_default = if let Some(n) = cmd.name {
        n
    } else {
        select_context(&dir, "Select a default context:")?.unwrap_or_default()
    };

    dir.set_default_context(&new_default)?;
    Ok(CommandOutput::from("Set new context successfully"))
}

/// Handles deleting an existing context
fn handle_del(cmd: DelCommand) -> Result<CommandOutput> {
    let dir = ContextDir::new(context_dir(cmd.directory)?)?;

    let ctx_to_delete = if let Some(n) = cmd.name {
        n
    } else {
        select_context(&dir, "Select a context to delete:")?.unwrap_or_default()
    };

    dir.delete_context(&ctx_to_delete)?;
    Ok(CommandOutput::from("Removed file successfully"))
}

/// Handles creating a new context by writing the default WashContext object to the specified path
fn handle_new(cmd: NewCommand) -> Result<CommandOutput> {
    let dir = ContextDir::new(context_dir(cmd.directory)?)?;

    let mut new_context = if cmd.interactive {
        prompt_for_context()?
    } else {
        WashContext::named(cmd.name.unwrap())
    };

    let options = sanitize_filename::Options {
        truncate: true,
        windows: true,
        replacement: "_",
    };

    // Ensure filename doesn't include uphill/downhill\ slashes, or reserved prefixes
    let sanitized = sanitize_filename::sanitize_with_options(&new_context.name, options);
    new_context.name = sanitized;
    dir.save_context(&new_context)?;
    Ok(CommandOutput::from(format!(
        "Created context {} with default values",
        new_context.name
    )))
}

/// Handles editing a context by opening the JSON file in the user's text editor of choice
fn handle_edit(cmd: EditCommand) -> Result<CommandOutput> {
    let dir = ContextDir::new(context_dir(cmd.directory)?)?;
    let editor = which::which(cmd.editor)?;

    ensure_host_config_context(&dir)?;

    let mut ctx_name = String::new();

    let ctx = if let Some(ctx) = cmd.name {
        let path = dir.get_context_path(&ctx)?;
        ctx_name = ctx;
        path
    } else if let Some(name) = select_context(&dir, "Select a context to edit:")? {
        let path = dir.get_context_path(&name)?;
        ctx_name = name;
        path
    } else {
        None
    };

    if let Some(path) = ctx {
        if ctx_name == HOST_CONFIG_NAME {
            warn!("Edits to the host_config context will be overwritten, make changes to the host config instead");
        }
        let status = Command::new(editor).arg(&path).status()?;

        match status.success() {
            true => Ok(CommandOutput::from("Finished editing context successfully")),
            false => bail!("Failed to edit context"),
        }
    } else {
        Err(Error::new(
            ErrorKind::NotFound,
            "Unable to find context supplied, please ensure it exists".to_string(),
        )
        .into())
    }
}

/// Ensures the host config context exists
pub(crate) fn ensure_host_config_context(context_dir: &ContextDir) -> Result<()> {
    create_host_config_context(context_dir)
}

/// Load the host configuration file and create a context called `host_config` from it
fn create_host_config_context(context_dir: &ContextDir) -> Result<()> {
    let host_config_ctx = WashContext {
        name: HOST_CONFIG_NAME.to_string(),
        ..load_context(cfg_dir()?.join(format!("{HOST_CONFIG_NAME}.json")))
            .unwrap_or_else(|_| WashContext::default())
    };
    context_dir.save_context(&host_config_ctx)?;
    // Set the default context if it isn't set yet
    if context_dir.default_context()?.is_none() {
        context_dir.set_default_context(HOST_CONFIG_NAME)?;
    }
    Ok(())
}

/// Given an optional supplied directory, determine the context directory either from the supplied
/// directory or using the home directory and the predefined `.wash/contexts` folder.
pub(crate) fn context_dir(cmd_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = cmd_dir {
        Ok(dir)
    } else {
        Ok(cfg_dir()?.join(CTX_DIR_NAME))
    }
}

/// Prompts the user with the provided `contexts` choices and returns the user's response.
/// This can be used to determine which context to delete, edit, or set as a default, for example
fn select_context(dir: &ContextDir, prompt: &str) -> Result<Option<String>> {
    let default = dir
        .default_context()?
        .unwrap_or_else(|| HOST_CONFIG_NAME.to_string());

    let choices: Vec<String> = dir.list_contexts()?;

    let entry = StringEntry {
        default: Some(default),
        choices: Some(choices.clone()),
        regex: None,
    };

    if let Ok(choice) = prompt_for_choice(&entry, prompt) {
        Ok(choices.get(choice).map(|c| c.to_string()))
    } else {
        Ok(None)
    }
}

/// Prompts the user interactively for context values, returning a constructed context
fn prompt_for_context() -> Result<WashContext> {
    let name = user_question(
        "What do you want to name the context?",
        &Some("default".to_string()),
    )?;

    let cluster_seed = match user_question(
        "What cluster seed do you want to use to sign invocations?",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s.parse::<ClusterSeed>()?),
        _ => None,
    };
    let ctl_host = user_question(
        "What is the control interface connection host?",
        &Some(DEFAULT_NATS_HOST.to_string()),
    )?;
    let ctl_port = user_question(
        "What is the control interface connection port?",
        &Some(DEFAULT_NATS_PORT.to_string()),
    )?;
    let ctl_jwt = match user_question(
        "Enter your JWT that you use to authenticate to the control interface connection, if applicable",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };
    let ctl_seed = match user_question(
        "Enter your user seed that you use to authenticate to the control interface connection, if applicable",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };
    let ctl_credsfile = match user_question(
        "Enter the absolute path to control interface connection credsfile, if applicable",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };
    let ctl_timeout = user_question(
        "What should the control interface timeout be (in milliseconds)?",
        &Some(DEFAULT_NATS_TIMEOUT_MS.to_string()),
    )?;

    let lattice_prefix = user_question(
        "What is the lattice prefix that the host will communicate on?",
        &Some(DEFAULT_LATTICE_PREFIX.to_string()),
    )?;

    let js_domain = match user_question(
        "What JetStream domain will the host be running, if any?",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };

    let rpc_host = user_question(
        "What is the RPC host?",
        &Some(DEFAULT_NATS_HOST.to_string()),
    )?;
    let rpc_port = user_question(
        "What is the RPC connection port?",
        &Some(DEFAULT_NATS_PORT.to_string()),
    )?;
    let rpc_jwt = match user_question(
        "Enter your JWT that you use to authenticate to the RPC connection, if applicable",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };
    let rpc_seed = match user_question(
        "Enter your user seed that you use to authenticate to the RPC connection, if applicable",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };
    let rpc_credsfile = match user_question(
        "Enter the absolute path to RPC connection credsfile, if applicable",
        &Some(String::new()),
    ) {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        _ => None,
    };
    let rpc_timeout = user_question(
        "What should the RPC timeout be (in milliseconds)?",
        &Some(DEFAULT_NATS_TIMEOUT_MS.to_string()),
    )?;

    Ok(WashContext {
        name,
        cluster_seed,
        ctl_host,
        ctl_port: ctl_port.parse().unwrap_or_default(),
        ctl_jwt,
        ctl_seed,
        ctl_credsfile: ctl_credsfile.map(PathBuf::from),
        ctl_timeout: ctl_timeout.parse()?,
        lattice_prefix,
        js_domain,
        rpc_host,
        rpc_port: rpc_port.parse().unwrap_or_default(),
        rpc_jwt,
        rpc_seed,
        rpc_credsfile: rpc_credsfile.map(PathBuf::from),
        rpc_timeout: rpc_timeout.parse()?,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cmd {
        #[clap(subcommand)]
        cmd: CtxCommand,
    }
    #[test]
    // Enumerates all options of ctx subcommands to ensure
    // changes are not made to the ctx API
    fn test_ctx_comprehensive() {
        let cmd: Cmd = Parser::try_parse_from([
            "ctx",
            "new",
            "my_name",
            "--interactive",
            "--directory",
            "./contexts",
        ])
        .unwrap();

        match cmd.cmd {
            CtxCommand::New(cmd) => {
                assert_eq!(cmd.directory.unwrap(), PathBuf::from("./contexts"));
                assert!(cmd.interactive);
                assert_eq!(cmd.name.unwrap(), "my_name");
            }
            _ => panic!("ctx constructed incorrect command"),
        }

        let cmd: Cmd = Parser::try_parse_from([
            "ctx",
            "edit",
            "my_context",
            "--editor",
            "vim",
            "--directory",
            "./contexts",
        ])
        .unwrap();
        match cmd.cmd {
            CtxCommand::Edit(cmd) => {
                assert_eq!(cmd.directory.unwrap(), PathBuf::from("./contexts"));
                assert_eq!(cmd.editor, "vim");
                assert_eq!(cmd.name.unwrap(), "my_context");
            }
            _ => panic!("ctx constructed incorrect command"),
        }

        let cmd: Cmd =
            Parser::try_parse_from(["ctx", "del", "my_context", "--directory", "./contexts"])
                .unwrap();
        match cmd.cmd {
            CtxCommand::Del(cmd) => {
                assert_eq!(cmd.directory.unwrap(), PathBuf::from("./contexts"));
                assert_eq!(cmd.name.unwrap(), "my_context");
            }
            _ => panic!("ctx constructed incorrect command"),
        }

        let cmd: Cmd =
            Parser::try_parse_from(["ctx", "list", "--directory", "./contexts"]).unwrap();
        match cmd.cmd {
            CtxCommand::List(cmd) => {
                assert_eq!(cmd.directory.unwrap(), PathBuf::from("./contexts"));
            }
            _ => panic!("ctx constructed incorrect command"),
        }

        let cmd: Cmd =
            Parser::try_parse_from(["ctx", "default", "host_config", "--directory", "./contexts"])
                .unwrap();
        match cmd.cmd {
            CtxCommand::Default(cmd) => {
                assert_eq!(cmd.directory.unwrap(), PathBuf::from("./contexts"));
                assert_eq!(cmd.name.unwrap(), "host_config");
            }
            _ => panic!("ctx constructed incorrect command"),
        }
    }
}
