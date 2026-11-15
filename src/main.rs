//! Command line surface: seven commands and their accepted arguments.
//!
//! The acceptance surface is a hard boundary, and there are three exit codes only. clap exits 2
//! on a usage error by itself, which is what the rest of the tool uses; help and version exit 0.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use tuckin::{Cmd, SealAction, run};

const SELECTOR_HELP: &str = "Repository-relative path or directory prefix";

#[derive(Parser)]
#[command(
    name = "tuckin",
    bin_name = "tuckin",
    version,
    about = "Deploy a dotfiles repository to this machine",
    after_help = "Run tuckin <COMMAND> --help for details.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: CliCmd,
}

#[derive(Args)]
struct SealArgs {
    /// Write one seal. Use --seal=NAME to select a named patch
    #[arg(long, value_name = "NAME", require_equals = true, requires = "paths")]
    seal: Option<Option<String>>,
    /// Generate fresh ciphertext for the configured recipients
    #[arg(long, requires = "paths")]
    rekey: bool,
}

impl SealArgs {
    fn action(self) -> Option<SealAction> {
        match self.seal {
            Some(name) => Some(SealAction::Seal { name, rekey: self.rekey }),
            None => self.rekey.then_some(SealAction::Rekey),
        }
    }
}

#[derive(Subcommand)]
enum CliCmd {
    /// Initialize a repository in an empty directory
    #[command(
        long_about = "Create a skeleton in an empty directory, or use --from to clone with Git.\nIn an existing repository, --key prepares the local identity and sets the first recipient.\nWith recipients already configured, it prints the local public keys.",
        after_long_help = "Example:\n  tuckin init"
    )]
    Init {
        /// Clone an existing Tuckin repository with Git
        #[arg(long, value_name = "SOURCE")]
        from: Option<String>,
        /// Prepare the local identity and set the first recipient
        #[arg(long, conflicts_with = "from")]
        key: bool,
    },
    /// Adopt existing target files into the repository
    #[command(
        long_about = "Adopt existing target files as new repository sources.\nUse exact file paths. A path with an existing source is refused.",
        after_long_help = "Example:\n  tuckin add --encrypt home/.ssh/id_ed25519"
    )]
    Add {
        /// Repository-relative path for a new source
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
        /// Seal each adopted file for the recipients in tuckin.toml
        #[arg(long)]
        encrypt: bool,
    },
    /// Open repository files for editing
    #[command(
        long_about = "Open existing repository files with the program named by $EDITOR.\nOmit PATH to open tuckin.toml. On POSIX, seals open in a protected session.\n\nWith --seal, take new contents from hidden input or stdin. With --rekey,\nopen the current ciphertext and seal it for the configured recipients.\nWith both options, seal the supplied contents without opening the old seal.\nThese options require one exact path.",
        after_long_help = "Example:\n  tuckin edit home/.ssh/id_ed25519 --seal"
    )]
    Edit {
        /// Repository-relative file path [default: tuckin.toml]
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        #[command(flatten)]
        seals: SealArgs,
    },
    /// Preview pending writes
    #[command(
        long_about = "Preview pending writes. Omit PATH to include all entries.\n\nBy default, the terminal shows a short summary. Redirect stdout for the\ncomplete pending list in TSV. Exit status 1 means writes are pending.",
        after_long_help = "Example:\n  tuckin diff --patch home/.gitconfig"
    )]
    Diff {
        #[arg(value_name = "PATH", help = SELECTOR_HELP)]
        paths: Vec<String>,
        /// Show a bounded line preview for one exact file
        #[arg(long, requires = "paths")]
        patch: bool,
    },
    /// Show the planned action for each entry
    #[command(
        long_about = "Show whether each selected entry would be created, overwritten or unchanged.\nOmit PATH to include all entries.",
        after_long_help = "Example:\n  tuckin status home/.config"
    )]
    Status {
        #[arg(value_name = "PATH", help = SELECTOR_HELP)]
        paths: Vec<String>,
    },
    /// Deploy files and run repository scripts
    #[command(
        long_about = "Deploy files, then run repository scripts. Omit PATH to deploy all entries.\n\nPATH selects which files to evaluate and deploy. All target placements are\nvalidated. Scripts run with your account's permissions and can write outside\nthe deployment targets. Use --no-scripts to skip them.",
        after_long_help = "Example:\n  tuckin apply --no-scripts home/.gitconfig"
    )]
    Apply {
        #[arg(value_name = "PATH", help = SELECTOR_HELP)]
        paths: Vec<String>,
        /// Skip the script phase entirely
        #[arg(long)]
        no_scripts: bool,
    },
    /// Copy target changes back into the repository
    #[command(
        long_about = "Copy target changes back into repository sources. Omit PATH to select all\nentries. For a patch, collect only the ranges it owns. Ordinary collection\nskips missing targets.\n\nWith --seal, seal one body from the target. With --rekey, recover filled\nseals from the target for the configured recipients. With both options,\nrecover only the selected body. These options require one exact path.",
        after_long_help = "Example:\n  tuckin absorb home/.config/scoop/config.json --seal=virustotal"
    )]
    Absorb {
        #[arg(value_name = "PATH", help = SELECTOR_HELP)]
        paths: Vec<String>,
        #[command(flatten)]
        seals: SealArgs,
    },
}

impl From<CliCmd> for Cmd {
    fn from(cli: CliCmd) -> Self {
        match cli {
            CliCmd::Init { key: true, .. } => Cmd::InitKey,
            CliCmd::Init { from, .. } => Cmd::Init { from },
            CliCmd::Add { paths, encrypt } => Cmd::Add { paths, encrypt },
            CliCmd::Edit { paths, seals } => Cmd::Edit { paths, seals: seals.action() },
            CliCmd::Diff { paths, patch } => Cmd::Diff { paths, patch },
            CliCmd::Status { paths } => Cmd::Status { paths },
            CliCmd::Apply { paths, no_scripts } => Cmd::Apply { paths, no_scripts },
            CliCmd::Absorb { paths, seals } => Cmd::Absorb { paths, seals: seals.action() },
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command.into()) {
        Ok(code) => code,
        Err(error) => {
            error.report();
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        let mut full = vec!["tuckin"];
        full.extend_from_slice(args);
        Cli::command().try_get_matches_from(full)
    }

    /// Every command accepts only the arguments it uses.
    #[test]
    fn parameter_acceptance_grid() {
        // (command, takes [PATH], takes --from, takes --no-scripts, takes --encrypt)
        let grid = [
            ("init", false, true, false, false),
            ("add", true, false, false, true),
            ("edit", true, false, false, false),
            ("diff", true, false, false, false),
            ("status", true, false, false, false),
            ("apply", true, false, true, false),
            ("absorb", true, false, false, false),
        ];
        for (command, takes_paths, takes_from, takes_no_scripts, takes_encrypt) in grid {
            assert_eq!(parse(&[command, "home/x"]).is_ok(), takes_paths, "{command} [PATH]");
            assert_eq!(parse(&[command, "--from", "url"]).is_ok(), takes_from, "{command} --from");
            assert_eq!(
                parse(&[command, "--no-scripts"]).is_ok(),
                takes_no_scripts,
                "{command} --no-scripts"
            );
            assert_eq!(
                parse(&[command, "--encrypt", "home/x"]).is_ok(),
                takes_encrypt,
                "{command} --encrypt"
            );
            assert_eq!(parse(&[command, "--patch", "home/x"]).is_ok(), command == "diff");
        }
    }

    #[test]
    fn add_requires_a_path() {
        let err = parse(&["add"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn usage_errors_exit_two() {
        for bad in [
            vec!["nope"],
            vec!["status", "--encrypt"],
            vec!["apply", "--encrypt", "home/x"],
            vec!["init", "home/x"],
            vec!["apply", "--from", "url"],
            vec!["diff", "--patch"],
        ] {
            let err = parse(&bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn seven_commands_and_no_eighth() {
        let names: Vec<String> =
            Cli::command().get_subcommands().map(|c| c.get_name().to_string()).collect();
        assert_eq!(names, vec!["init", "add", "edit", "diff", "status", "apply", "absorb"]);
    }

    /// `--encrypt` is irreversible, so it exists on exactly one command and only as an explicit
    /// flag: the help text can never advertise it where it would be rejected.
    #[test]
    fn encrypt_is_an_explicit_flag_on_add_only() {
        let text = Cli::command().render_long_help().to_string();
        assert!(!text.contains("--encrypt"), "{text}");
        let mut command = Cli::command();
        let add = command
            .get_subcommands_mut()
            .find(|command| command.get_name() == "add")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(add.contains("--encrypt"), "{add}");
    }
}
