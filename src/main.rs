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
    after_help = "Commands for an existing repository find the nearest tuckin.toml in the\ncurrent directory or a parent. Run tuckin <COMMAND> --help for details.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: CliCmd,
}

#[derive(Args)]
struct SealArgs {
    /// Fill or update a seal. Use --seal=NAME for a named patch
    #[arg(long, value_name = "NAME", require_equals = true, requires = "paths")]
    seal: Option<Option<String>>,
    /// Encrypt again for the configured recipients
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
    /// Create or clone a repository, or prepare its age identity
    #[command(
        long_about = concat!(
            "Plain init creates tuckin.toml in the empty current directory without Git.\n",
            "\n",
            "--from SOURCE requires git and an empty directory with a writable parent.\n",
            "SOURCE can be a Git URL or local repository path. Git handles authentication.\n",
            "Tuckin checks the layout before installing the clone.\n",
            "Without Git, unpack a Tuckin repository snapshot and run tuckin status there.\n",
            "\n",
            "Use --key inside an existing Tuckin repository. It finds the nearest tuckin.toml\n",
            "and creates a private age identity if missing, or reuses a readable identity.\n",
            "Identity lookup uses AGE_IDENTITY_FILE, then identity_file in tuckin.toml,\n",
            "then ~/.config/age/keys.txt. Use AGE_PASSPHRASE for a protected identity.\n",
            "\n",
            "If recipients is unset and the identity contains one distinct key, --key writes\n",
            "its public key to tuckin.toml. With several keys, set recipients explicitly.\n",
            "If recipients is already set, --key prints the local public keys and keeps the\n",
            "configured recipients. An unusable identity stops setup and stays in place.\n",
            "--key and --from cannot be combined. Existing ciphertext is kept as-is.",
        ),
        after_long_help = "Examples\n  tuckin init\n  tuckin init --from ../dotfiles\n  tuckin init --key"
    )]
    Init {
        /// Use Git to clone into the empty current directory
        #[arg(long, value_name = "SOURCE")]
        from: Option<String>,
        /// Prepare the local identity and initialize or inspect recipients
        #[arg(long, conflicts_with = "from")]
        key: bool,
    },
    /// Adopt existing target files into the repository
    #[command(
        long_about = concat!(
            "Copy existing regular target files into the repository as new sources.\n",
            "Use exact repository-relative file paths with a target on this machine.\n",
            "An existing source prevents adoption. Target files stay in place.\n",
            "\n",
            "Use --encrypt to store whole-file seals for the recipients in tuckin.toml.\n",
            "Encryption needs configured public recipients and can run without a private key.",
        ),
        after_long_help = "Examples\n  tuckin add home/.gitconfig\n  tuckin add --encrypt home/.ssh/id_ed25519"
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
        long_about = concat!(
            "Open existing repository files with $EDITOR. Set it to one executable name or\n",
            "path. Omit PATH to open tuckin.toml. Specify exact file paths to edit other files.\n",
            "Sealed sources use a private temporary directory. On Windows it needs local NTFS.\n",
            "Set EDITOR to a program that waits until you close the file.\n",
            "\n",
            "Seal options require configured recipients and write one exact source file.\n",
            "--seal takes nonempty hidden input or raw bytes from redirected stdin. It\n",
            "selects a whole-file seal or a patch body. Updating a filled seal needs its\n",
            "old identity. --rekey alone opens all filled seals and re-encrypts them for\n",
            "the configured recipients. Combine both options to write one body from new\n",
            "input using only the configured recipients. The seal options skip the editor.",
        ),
        after_long_help = "Examples\n  tuckin edit\n  tuckin edit home/.ssh/id_ed25519 --seal"
    )]
    Edit {
        /// Repository-relative file path [default: tuckin.toml]
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        #[command(flatten)]
        seals: SealArgs,
    },
    /// Preview pending or blocked changes
    #[command(
        long_about = concat!(
            "Preview pending and blocked changes. Omit PATH to include all active entries.\n",
            "The terminal shows a short summary. Redirect stdout for the full list in TSV.\n",
            "\n",
            "Use --patch with one exact source file for a bounded preview. Sealed and binary\n",
            "files show only their status. Exit status 0 means no differences, 1 means\n",
            "differences or blocked changes, and 2 means an error.",
        ),
        after_long_help = "Example\n  tuckin diff --patch home/.gitconfig"
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
        long_about = concat!(
            "List the planned action for each selected file as TSV. Omit PATH to include all\n",
            "active entries. Actions are create, overwrite, repair, blocked and unchanged.\n",
            "A ? row names an unmanaged neighboring file. An ignored count covers the\n",
            "whole repository scan. Exit status 0 means the report succeeded, even when\n",
            "it contains pending or blocked changes.",
        ),
        after_long_help = "Example\n  tuckin status home/.config"
    )]
    Status {
        #[arg(value_name = "PATH", help = SELECTOR_HELP)]
        paths: Vec<String>,
    },
    /// Deploy files and run repository scripts
    #[command(
        long_about = concat!(
            "Deploy selected files, then run enabled repository scripts. Omit PATH to select\n",
            "all active entries. Replacing existing managed bytes requires a matching\n",
            "deployment record. A conflict stops the batch before file writes or scripts. Use\n",
            "--overwrite with exact source paths to choose the repository versions.\n",
            "\n",
            "File selection leaves scripts enabled, even if no files change. Scripts run\n",
            "from the repository root with your account permissions. Use --no-scripts for\n",
            "files only. A failure after publication begins keeps the writes already made.\n",
            "\n",
            "The terminal shows counts and a short list of published paths. Redirected\n",
            "default output contains TSV counts. Use --list for every published path or\n",
            "--patch for a bounded preview of one applied file. These modes are exclusive.",
        ),
        after_long_help = "Examples\n  tuckin apply --no-scripts\n  tuckin apply --overwrite --patch home/.gitconfig"
    )]
    Apply {
        #[arg(value_name = "PATH", help = SELECTOR_HELP)]
        paths: Vec<String>,
        /// Skip repository scripts
        #[arg(long)]
        no_scripts: bool,
        /// Replace exact files with the repository versions
        #[arg(long, requires = "paths")]
        overwrite: bool,
        /// Print every published file action and path as TSV
        #[arg(long, conflicts_with = "patch")]
        list: bool,
        /// Preview one written file. Requires an exact source path
        #[arg(long, requires = "paths")]
        patch: bool,
    },
    /// Copy target changes back into the repository
    #[command(
        long_about = concat!(
            "Copy target contents back into existing repository sources. Omit PATH to include\n",
            "all active entries. Ordinary absorb collects whole literal files, filled seals\n",
            "and representable patch bodies. It skips missing targets and unfilled seals.\n",
            "Use diff --patch PATH to compare the two versions before collecting.\n",
            "\n",
            "Seal options require one exact source file and configured recipients. --seal\n",
            "takes the value from the selected target file or patch range. --rekey rebuilds\n",
            "all filled seals in the file from the target using the current recipients.\n",
            "Combine both options to rebuild just the selected body. Rekeying from the\n",
            "target can recover it without access to the old identity.",
        ),
        after_long_help = "Examples\n  tuckin absorb home/.gitconfig\n  tuckin absorb home/.config/scoop/config.json --seal=virustotal"
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
            CliCmd::Apply { paths, no_scripts, overwrite, list, patch } => {
                Cmd::Apply { paths, no_scripts, overwrite, list, patch }
            }
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
            assert_eq!(
                parse(&[command, "--patch", "home/x"]).is_ok(),
                matches!(command, "diff" | "apply")
            );
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
