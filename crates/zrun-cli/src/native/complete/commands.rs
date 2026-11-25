//! Small built-in vocabularies, available even when a command's completion scripts are absent.
use super::Position;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Group {
    Git,
    GitOtherDirectory,
    Cargo,
    Zrun,
    Ai,
}

pub(super) fn root(command: &str) -> Option<Group> {
    match command.rsplit('/').next()? {
        "git" => Some(Group::Git),
        "cargo" => Some(Group::Cargo),
        "zrun" => Some(Group::Zrun),
        _ => None,
    }
}

impl Group {
    pub(super) fn names(self) -> &'static str {
        match self {
            Self::Git | Self::GitOtherDirectory => {
                "add am apply bisect blame branch checkout cherry-pick clean clone commit config describe diff fetch grep init log ls-files merge mv pull push rebase reflog remote reset restore revert rev-parse rm show stash status submodule switch tag worktree"
            }
            Self::Cargo => {
                "add bench build check clean clippy doc fetch fix fmt help info init install metadata new package publish remove report run rustc rustdoc search test tree uninstall update vendor version"
            }
            Self::Zrun => "ai batch mcp run",
            Self::Ai => "compact config connect help run",
        }
    }

    pub(super) fn flags(self, leaf: Option<&str>) -> &'static str {
        match (self, leaf) {
            (Self::Git | Self::GitOtherDirectory, None) => {
                "-C -c --git-dir --help --no-pager --version --work-tree"
            }
            (Self::Git | Self::GitOtherDirectory, Some("status")) => {
                "--branch --ignored --long --porcelain --short --show-stash --untracked-files -b -s -u"
            }
            (Self::Git | Self::GitOtherDirectory, Some("switch")) => {
                "--create --detach --discard-changes --force-create --guess --no-guess --track -C -c -d"
            }
            (Self::Git | Self::GitOtherDirectory, Some("checkout")) => {
                "--detach --force --ours --patch --theirs --track -b"
            }
            (Self::Git | Self::GitOtherDirectory, Some("commit")) => {
                "--all --amend --dry-run --message --no-edit --signoff -a -m -s"
            }
            (Self::Git | Self::GitOtherDirectory, Some("add")) => {
                "--all --dry-run --intent-to-add --patch --update -A -n -p -u"
            }
            (Self::Git | Self::GitOtherDirectory, Some("diff")) => {
                "--cached --check --color --name-only --name-status --no-index --staged --stat --word-diff"
            }
            (Self::Git | Self::GitOtherDirectory, Some("log")) => {
                "--all --author --decorate --follow --graph --oneline --patch --since --stat"
            }
            (Self::Git | Self::GitOtherDirectory, Some("branch")) => {
                "--all --contains --delete --list --merged --move --no-merged --remotes --show-current -a -d -m -r"
            }
            (Self::Git | Self::GitOtherDirectory, Some("restore")) => {
                "--patch --source --staged --worktree -p -s -S -W"
            }
            (
                Self::Cargo,
                Some("build" | "check" | "clippy" | "run" | "test" | "bench" | "doc"),
            ) => {
                "--all-features --features --help --locked --manifest-path --no-default-features --offline --package --release --target -p"
            }
            (Self::Zrun, Some("batch")) => "--ready",
            (Self::Cargo, _) => "--color --help --list --locked --offline --verbose --version",
            (Self::Zrun, None) => {
                "--help --login --native-shell-version --shell --version -c -i -l"
            }
            (Self::Zrun, Some("mcp")) | (Self::Ai, Some("connect")) => {
                "--http --socket --stdio --unix"
            }
            (Self::Ai, Some("run" | "compact")) => "--save",
            _ => "",
        }
    }

    pub(super) fn after(self, value: &str) -> Position {
        if value == "--" {
            return Position::Group(self);
        }
        if matches!(self, Self::Git | Self::GitOtherDirectory) {
            if matches!(value, "-C" | "--git-dir" | "--work-tree") {
                return Position::GlobalValue(Self::GitOtherDirectory, true);
            }
            if matches!(value, "-c" | "--config-env" | "--namespace") {
                return Position::GlobalValue(self, false);
            }
            if value.starts_with("--git-dir=")
                || value.starts_with("--work-tree=")
                || value.starts_with("-C")
            {
                return Position::Group(Self::GitOtherDirectory);
            }
        }
        if value.starts_with('-') || (self == Self::Cargo && value.starts_with('+')) {
            return Position::Group(self);
        }
        if self == Self::Zrun && value == "ai" {
            return Position::Group(Self::Ai);
        }
        self.names()
            .split_ascii_whitespace()
            .find(|name| *name == value)
            .map_or(Position::Argument, |name| Position::Options(self, name))
    }
}
