mod control;
mod orchestrator;
mod types;
mod voice_loop;

use control::{ControlSubcommand, ControlTarget};
use orchestrator::OrchestratorProvider;
use std::path::PathBuf;
use types::{DelegateToOrchestratorArgs, VoiceUpdate};
use voice_loop::settings::Settings;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    load_dotenv();

    // Subcommand short-circuit: `gamechat inspect|tail|open …` route entirely
    // through the control module and never touch the realtime flag plumbing.
    if let Some((subcommand, target)) = parse_control_subcommand()? {
        return control::run_cli(subcommand, target).await;
    }

    let args = CliArgs::parse()?;

    if args.print_realtime_config {
        let resolved = Settings::load_with_override(args.settings_path.as_deref())?
            .resolve(args.voice.clone(), args.preset.clone())?;
        let config = voice_loop::session_update_json_for(&args.model, &resolved);
        println!(
            "{}",
            serde_json::to_string_pretty(&config)
                .map_err(|e| format!("failed to serialize realtime config: {e}"))?
        );
        return Ok(());
    }

    if args.realtime {
        let openai_api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            let path = global_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/gamechat/env".to_string());
            format!(
                "OPENAI_API_KEY is required for --realtime.\n\
                 set it in your shell, or write it to {path}:\n  \
                 echo 'OPENAI_API_KEY=sk-...' > {path}"
            )
        })?;
        let voice_settings = Settings::load_with_override(args.settings_path.as_deref())?
            .resolve(args.voice.clone(), args.preset.clone())?;
        let orchestrator_provider = build_orchestrator_provider(
            &args.provider,
            args.codex_bin,
            args.codex_model,
            args.claude_bin,
            args.claude_model,
        )?;
        voice_loop::run_realtime_voice(voice_loop::RealtimeRunConfig {
            openai_api_key,
            model: args.model,
            orchestrator_provider,
            voice_settings,
        })
        .await?;
        return Ok(());
    }

    let orchestrator_provider = build_orchestrator_provider(
        &args.provider,
        args.codex_bin,
        args.codex_model,
        args.claude_bin,
        args.claude_model,
    )?;

    if let Some(message) = args.once {
        let slug = sanitize_slug(&args.slug);
        let delegated = DelegateToOrchestratorArgs {
            slug: slug.clone(),
            user_intent: message,
            recent_context: args.context.unwrap_or_default(),
            urgency: args.urgency,
            suggested_user_update: args.suggested_user_update,
        };
        let update = delegate_to_orchestrator(&orchestrator_provider, &slug, delegated).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&update)
                .map_err(|e| format!("failed to serialize voice update: {e}"))?
        );
        return Ok(());
    }

    Err(usage())
}

async fn delegate_to_orchestrator(
    provider: &OrchestratorProvider,
    slug: &str,
    args: DelegateToOrchestratorArgs,
) -> Result<VoiceUpdate, String> {
    let message = args.to_agent_message();
    let mut session = provider.open_session(slug).await?;
    let response = session
        .send_message_until_done_for_job("voice-once", &message, None)
        .await?;

    Ok(VoiceUpdate {
        message: response.reply,
        should_interrupt: false,
        confidence: 0.6,
        done: !response.suspended,
    })
}

struct CliArgs {
    once: Option<String>,
    context: Option<String>,
    urgency: String,
    suggested_user_update: Option<String>,
    slug: String,
    provider: String,
    codex_bin: Option<String>,
    codex_model: Option<String>,
    claude_bin: Option<String>,
    claude_model: Option<String>,
    print_realtime_config: bool,
    realtime: bool,
    model: String,
    preset: Option<String>,
    voice: Option<String>,
    settings_path: Option<PathBuf>,
}

impl CliArgs {
    fn parse() -> Result<Self, String> {
        let mut once = None;
        let mut context = None;
        let mut urgency = "background".to_string();
        let mut suggested_user_update = None;
        let mut slug = "default".to_string();
        let mut provider = "claude".to_string();
        let mut codex_bin = None;
        let mut codex_model = None;
        let mut claude_bin = None;
        let mut claude_model = None;
        let mut print_realtime_config = false;
        let mut realtime = false;
        let mut model = "gpt-realtime-2".to_string();
        let mut preset = None;
        let mut voice = None;
        let mut settings_path = None;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--once" => once = Some(next_value(&mut args, "--once")?),
                "--context" => context = Some(next_value(&mut args, "--context")?),
                "--urgency" => urgency = next_value(&mut args, "--urgency")?,
                "--suggested-user-update" => {
                    suggested_user_update = Some(next_value(&mut args, "--suggested-user-update")?)
                }
                "--slug" => slug = next_value(&mut args, "--slug")?,
                "--task-slug" => slug = next_value(&mut args, "--task-slug")?,
                "--provider" => provider = next_value(&mut args, "--provider")?,
                "--codex-bin" => codex_bin = Some(next_value(&mut args, "--codex-bin")?),
                "--codex-model" => codex_model = Some(next_value(&mut args, "--codex-model")?),
                "--claude-bin" => claude_bin = Some(next_value(&mut args, "--claude-bin")?),
                "--claude-model" => claude_model = Some(next_value(&mut args, "--claude-model")?),
                "--print-realtime-config" => print_realtime_config = true,
                "--realtime" => realtime = true,
                "--model" => model = next_value(&mut args, "--model")?,
                "--preset" => preset = Some(next_value(&mut args, "--preset")?),
                "--voice" => voice = Some(next_value(&mut args, "--voice")?),
                "--settings" => {
                    settings_path = Some(PathBuf::from(next_value(&mut args, "--settings")?))
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown argument: {other}\n\n{}", usage())),
            }
        }

        Ok(Self {
            once,
            context,
            urgency,
            suggested_user_update,
            slug,
            provider,
            codex_bin,
            codex_model,
            claude_bin,
            claude_model,
            print_realtime_config,
            realtime,
            model,
            preset,
            voice,
            settings_path,
        })
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{name} requires a value"))
}

fn parse_control_subcommand() -> Result<Option<(ControlSubcommand, ControlTarget)>, String> {
    let mut iter = std::env::args().skip(1);
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    let subcommand_name = first.as_str();
    if !matches!(
        subcommand_name,
        "inspect" | "tail" | "open" | "reset" | "discover"
    ) {
        return Ok(None);
    }
    let mut positional: Vec<String> = Vec::new();
    let mut target = ControlTarget::default();
    let mut launch = false;
    let mut reset_reason: Option<String> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pid" => {
                let value = next_value(&mut iter, "--pid")?;
                let pid: u32 = value
                    .parse()
                    .map_err(|e| format!("--pid expected a number: {e}"))?;
                target.pid = Some(pid);
            }
            "--socket" => {
                target.socket = Some(PathBuf::from(next_value(&mut iter, "--socket")?));
            }
            "--launch" => launch = true,
            "--reason" => reset_reason = Some(next_value(&mut iter, "--reason")?),
            "--help" | "-h" => return Err(control_usage()),
            other if other.starts_with("--") => {
                return Err(format!("unknown argument for {subcommand_name}: {other}"));
            }
            other => positional.push(other.to_string()),
        }
    }
    let subcommand = match subcommand_name {
        "inspect" => {
            if !positional.is_empty() {
                return Err(format!(
                    "inspect takes no positional arguments, got: {}",
                    positional.join(" ")
                ));
            }
            ControlSubcommand::Inspect
        }
        "tail" => {
            let slug = positional
                .into_iter()
                .next()
                .ok_or_else(|| "tail requires a slug, e.g. `gamechat tail refactor_docs`".to_string())?;
            ControlSubcommand::Tail { slug }
        }
        "open" => {
            let slug = positional
                .into_iter()
                .next()
                .ok_or_else(|| "open requires a slug, e.g. `gamechat open refactor_docs`".to_string())?;
            ControlSubcommand::Open { slug, launch }
        }
        "reset" => {
            if !positional.is_empty() {
                return Err(format!(
                    "reset takes no positional arguments, got: {}. Use --reason <text> if you want to record one.",
                    positional.join(" ")
                ));
            }
            ControlSubcommand::Reset { reason: reset_reason }
        }
        "discover" => {
            if !positional.is_empty() {
                return Err(format!(
                    "discover takes no positional arguments, got: {}",
                    positional.join(" ")
                ));
            }
            if target.pid.is_some() || target.socket.is_some() {
                return Err(
                    "discover surveys every live gamechat in the runtime dir; --pid/--socket do not apply".to_string(),
                );
            }
            ControlSubcommand::Discover
        }
        _ => unreachable!(),
    };
    Ok(Some((subcommand, target)))
}

fn control_usage() -> String {
    "usage:
  gamechat inspect [--pid N | --socket PATH]
  gamechat tail <slug> [--pid N | --socket PATH]
  gamechat open <slug> [--pid N | --socket PATH] [--launch]
  gamechat reset [--pid N | --socket PATH] [--reason TEXT]
  gamechat discover

Inspect a running gamechat realtime session from another terminal. With no
target flag the client connects to the only running gamechat instance and
errors if more than one is present.

`reset` clears the realtime conversation context on the running voice loop
without disturbing audio playback. `discover` walks the runtime dir and
reports the active sub-agent slugs of every other live gamechat instance.
"
    .to_string()
}

fn build_orchestrator_provider(
    provider: &str,
    codex_bin: Option<String>,
    codex_model: Option<String>,
    claude_bin: Option<String>,
    claude_model: Option<String>,
) -> Result<OrchestratorProvider, String> {
    match provider {
        "claude" | "claude-code" => Ok(OrchestratorProvider::claude(claude_bin, claude_model)),
        "openai" | "codex" => Ok(OrchestratorProvider::openai(codex_bin, codex_model)),
        other => Err(format!(
            "unknown orchestrator provider: {other} (expected claude or codex)"
        )),
    }
}

fn usage() -> String {
    "usage:
  gamechat --realtime
  gamechat --once \"user asked for deeper work\"
  gamechat --print-realtime-config

  gamechat inspect                    List active sub-agents in a running session.
  gamechat tail <slug>                Stream a sub-agent's progress buffer.
  gamechat open <slug> [--launch]     Print (or launch on macOS) the resume command.
  gamechat reset [--reason TEXT]      Reset the realtime voice conversation context.
  gamechat discover                   List sub-agents in every other live gamechat.

options:
  --realtime                      Start the live voice loop (mic + speakers).
  --once <msg>                    Run a single delegation and print the VoiceUpdate JSON.
  --print-realtime-config         Print the session.update JSON sent to Realtime.

  --provider <claude|codex>       Background coding agent. Defaults to claude.
  --model <model>                 Realtime voice model. Defaults to gpt-realtime-2.

  --preset <name>                 Voice/personality preset (default, jarvis, concise, pirate, ...).
  --voice <name>                  Override voice (alloy, ash, ballad, cedar, coral, echo,
                                  marin, sage, shimmer, verse). Wins over preset + settings.
  --settings <path>               Override settings file (default: ~/.config/gamechat/settings.json).

  --slug <slug>                   Background task slug. Defaults to default.
  --context <text>                Recent voice transcript / context (passed to the agent).
  --urgency <now|background>      Relay urgency. Defaults to background.
  --suggested-user-update <text>  Short phrase the voice model may say while waiting.

  --claude-bin <path>             Override path to the `claude` binary.
  --claude-model <model>          Model passed to Claude Code (e.g. claude-sonnet-4).
  --codex-bin <path>              Override path to the `codex` binary.
  --codex-model <model>           Model passed to Codex (e.g. gpt-5-codex).

env:
  OPENAI_API_KEY                  Required for --realtime (OpenAI Realtime API).
"
    .to_string()
}

fn sanitize_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator && !slug.is_empty() {
            slug.push('_');
            last_was_separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "default".to_string()
    } else {
        slug
    }
}

fn load_dotenv() {
    // 1. Walk up from cwd looking for a .env (per-project override).
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let env_path = d.join(".env");
        if env_path.exists() {
            apply_env_file(&env_path);
            break;
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    // 2. Fall back to the installer-written global config. apply_env_file
    //    never overwrites an existing var, so the cwd .env wins.
    if let Some(path) = global_config_path() {
        if path.exists() {
            apply_env_file(&path);
        }
    }
}

fn global_config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("gamechat").join("env"))
}

fn apply_env_file(path: &std::path::Path) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = unquote_env_value(val.trim());
            if std::env::var(key).is_err() {
                // SAFETY: called once at startup before any worker tasks.
                unsafe {
                    std::env::set_var(key, val);
                }
            }
        }
    }
}

// Strip exactly one matching outer pair of single or double quotes.
// For single-quoted values, decode the POSIX `'\''` literal-quote escape
// produced by the installer's shell-safe quoting.
fn unquote_env_value(val: &str) -> String {
    let bytes = val.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if first == b'"' && last == b'"' {
            return val[1..val.len() - 1].to_string();
        }
        if first == b'\'' && last == b'\'' {
            return val[1..val.len() - 1].replace("'\\''", "'");
        }
    }
    val.to_string()
}

#[cfg(test)]
mod tests {
    use super::unquote_env_value;

    #[test]
    fn unquote_plain_value_passthrough() {
        assert_eq!(unquote_env_value("sk-abc_DEF-123"), "sk-abc_DEF-123");
    }

    #[test]
    fn unquote_strips_outer_double_quotes() {
        assert_eq!(unquote_env_value("\"sk-abc\""), "sk-abc");
    }

    #[test]
    fn unquote_strips_outer_single_quotes() {
        assert_eq!(unquote_env_value("'sk-abc'"), "sk-abc");
    }

    #[test]
    fn unquote_decodes_posix_apostrophe_escape() {
        // What the installer writes for a key containing a literal `'`.
        assert_eq!(unquote_env_value("'sk-with'\\''apos'"), "sk-with'apos");
    }

    #[test]
    fn unquote_preserves_inner_whitespace() {
        assert_eq!(unquote_env_value("'sk weird value'"), "sk weird value");
    }

    #[test]
    fn unquote_leaves_unbalanced_quotes_alone() {
        assert_eq!(unquote_env_value("'no-trailing"), "'no-trailing");
    }
}
